//! Turning a library [`EventStream`] into terminal output, the same way for every
//! CLI command.
//!
//! Every command goes through [`EventProcessor::process_events`], passing a
//! handler that returns `true` for an event it rendered itself and `false` to
//! take the default rendering.

use futures::StreamExt;
use selfie::package::{
    event::{ConsoleOutput, EventStream, OperationResult, Outcome, PackageEvent},
    port::{PackageError, PackageParseKind},
};

use crate::display_manager::{DisplayManager, ErrorDetail};

/// How a command's run ended, as the process reports it.
///
/// | code | meaning |
/// | ---- | ------- |
/// | 0    | clean |
/// | 1    | failed, refused part of its work, or could not answer |
/// | 2    | usage error (set by clap, never here) |
/// | 3    | found what it was asked to look for |
/// | 130  | cancelled (128 + SIGINT) |
// A new code takes the next free value in 3-63, never 64-78 (`sysexits.h`) or
// 126 and up (reserved by the shell), and no code is renumbered or reused. The
// README's exit-code table is the public contract and changes with this enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Exit {
    /// Did everything asked, and found nothing to report.
    Clean,
    /// Finished, and found what it was asked to look for.
    Found,
    /// Failed, refused part of its work, or ended without saying how it went.
    Failed,
    /// Interrupted.
    Cancelled,
}

impl Exit {
    /// The process exit code.
    pub(crate) fn code(self) -> i32 {
        match self {
            Exit::Clean => 0,
            Exit::Failed => 1,
            Exit::Found => 3,
            // 128 + 2 (SIGINT), the Unix convention for Ctrl+C termination.
            Exit::Cancelled => 130,
        }
    }
}

impl From<Outcome> for Exit {
    fn from(outcome: Outcome) -> Self {
        match outcome {
            Outcome::Clean => Exit::Clean,
            Outcome::Found => Exit::Found,
            Outcome::Failed => Exit::Failed,
        }
    }
}

/// What processing a stream decided.
#[derive(Debug, Clone)]
pub struct EventProcessingResult {
    /// The exit code for the operation
    pub exit_code: i32,
}

/// A reusable event processor for handling package operation events
///
/// This processor standardizes how events from the selfie library are handled
/// and displayed in the CLI, reducing boilerplate across different commands.
#[derive(Debug)]
pub struct EventProcessor {
    display: DisplayManager,
}

impl EventProcessor {
    /// Create a new event processor with the given display manager
    pub fn new(display: DisplayManager) -> Self {
        Self { display }
    }

    /// Get a reference to the display manager
    #[allow(dead_code)]
    pub(crate) fn display(&self) -> &DisplayManager {
        &self.display
    }

    /// Process events from the stream with a custom event handler
    ///
    /// This allows commands to provide custom handling for specific event types
    /// while still getting the default behavior for standard events.
    ///
    /// The custom handler should return:
    /// - `true` if the event was handled (skip default handling)
    /// - `false` to use default handling for the event
    pub async fn process_events<F>(
        self,
        mut stream: EventStream,
        mut custom_handler: F,
    ) -> EventProcessingResult
    where
        F: FnMut(&PackageEvent) -> bool,
    {
        let mut ending = None;

        while let Some(event) = stream.next().await {
            // Scored before the custom handler, which may claim either event: the
            // exit code is the library's verdict, whoever renders it.
            let cancelled = match &event {
                PackageEvent::Completed { result, .. } => {
                    ending = Some(Exit::from(result.outcome()));
                    false
                }
                PackageEvent::Canceled { .. } => {
                    ending = Some(Exit::Cancelled);
                    true
                }
                _ => false,
            };

            if !custom_handler(&event) {
                self.handle_event(event);
            }

            // Nothing after a cancellation is read, whoever rendered it.
            if cancelled {
                break;
            }
        }

        // A stream that ends without saying how the operation went, as one does when
        // the task running it panics, is a failure: nothing reported the work done.
        let exit = ending.unwrap_or_else(|| {
            self.display
                .print_error("The operation ended without reporting a result");
            Exit::Failed
        });

        self.display.finish();

        EventProcessingResult {
            exit_code: exit.code(),
        }
    }

    /// Render one event the default way.
    fn handle_event(&self, event: PackageEvent) {
        match event {
            PackageEvent::Started { operation_info } => {
                // Handle list operations differently since they don't have a specific package name
                let message = if operation_info.package_name.is_empty() {
                    format!(
                        "{} in environment '{}'",
                        operation_info.operation_type.to_string().to_title_case(),
                        operation_info.environment
                    )
                } else {
                    format!(
                        "{} package '{}' in environment '{}'",
                        operation_info.operation_type.to_string().to_title_case(),
                        operation_info.package_name,
                        operation_info.environment
                    )
                };
                self.display.print_info(message);
            }

            PackageEvent::Progress { message, .. } => {
                self.display.print_progress(message);
            }

            PackageEvent::Info { output, .. } => {
                handle_console_output(output);
            }

            PackageEvent::Trace { message, .. } => {
                tracing::trace!("{}", message);
            }

            PackageEvent::Debug { message, .. } => {
                tracing::debug!("{}", message);
            }

            // One line per skipped file, whatever the reason. A source window
            // belongs to a command asked about one spec: `apply` over N unparsable
            // specs would print N windows, which is the output that made
            // `package list` unreadable, and each is text from a file the reader
            // never asked about.
            PackageEvent::SpecSkipped { error, .. } => {
                self.display
                    .print_warning(selfie::package::service::skipped_spec_warning(&error));
            }

            PackageEvent::Warning { message, .. } => {
                self.display.print_warning(message);
                // Warnings don't set failure exit code by default
            }

            PackageEvent::Completed {
                operation_info,
                result: op_result,
            } => match op_result {
                // The level follows the library's verdict, so a run that refused
                // part of its work reads as an error and one that found drift as
                // a warning. The exit code was scored in `process_events`.
                OperationResult::Success(success) => match success.outcome() {
                    Outcome::Clean => self.display.print_success(success.to_string()),
                    Outcome::Found => self.display.print_warning(success.to_string()),
                    Outcome::Failed => {
                        self.display.collect_error(ErrorDetail {
                            package_name: operation_info.package_name,
                            operation: operation_info.operation_type.to_string(),
                            command: None,
                            exit_code: None,
                            stderr: None,
                            message: success.to_string(),
                        });
                        self.display.print_error(success.to_string());
                    }
                },
                OperationResult::Failure(err) => {
                    use selfie::package::event::{CommandFailure, OperationFailure};

                    // Collect structured error detail for end-of-operation summary
                    let error_detail = match &err {
                        OperationFailure::CommandError(CommandFailure::ExecutionFailed {
                            command,
                            exit_code,
                            stderr,
                        }) => ErrorDetail {
                            package_name: operation_info.package_name,
                            operation: operation_info.operation_type.to_string(),
                            command: Some(command.clone()),
                            exit_code: *exit_code,
                            stderr: Some(stderr.as_str().to_string()),
                            message: err.to_string(),
                        },
                        _ => ErrorDetail {
                            package_name: operation_info.package_name,
                            operation: operation_info.operation_type.to_string(),
                            command: None,
                            exit_code: None,
                            stderr: None,
                            message: err.to_string(),
                        },
                    };
                    self.display.collect_error(error_detail);

                    match err {
                        OperationFailure::Package(PackageError::PackageNotFound {
                            name,
                            packages_path,
                            ..
                        }) => {
                            self.display.print_error(format!(
                                "Package `{name}` not found in path {}",
                                packages_path.display()
                            ));
                        }
                        OperationFailure::PackageList(listing) => {
                            // The reason, not "not found". A plain file or a dangling
                            // link at the path is not a missing directory, and saying
                            // it is sends the user to a `mkdir -p` that fails with
                            // "File exists" or "No such file or directory". The
                            // repository classified the path once and the error carries
                            // the answer, so this renders it rather than looking again.
                            self.display.print_error(format!(
                                "Package directory at {} {}",
                                listing.path().display(),
                                listing.clause()
                            ));
                            // `selfie config` requires a subcommand and has only
                            // one, `validate`, so it is not a way to set the
                            // directory — naming it here sends a reader whose
                            // directory is already missing to a usage error.
                            // The global flag is per-run and the file is the
                            // durable fix, so both are named.
                            let mut suggestion = String::from(
                                "Edit 'package_directory' in your config file, or name another for this run with the global flag 'selfie --package-directory <path> …'",
                            );
                            // `mkdir -p` answers an empty path and nothing else, so
                            // the remedy comes from the reason rather than being
                            // offered for every state. The two settings are named
                            // either way, since they are the fix when no command is.
                            // Same quoter and same `--` as the dotfiles directory's
                            // remedy, because this sentence is pasted too: a path
                            // holding a space breaks the command, and one beginning
                            // with a dash is read by mkdir as options.
                            if let selfie::fs::DirectoryState::Absent(reason) = listing.state()
                                && let Some(command) = reason.remedy(listing.path())
                            {
                                // The command ends the sentence, and nothing may follow
                                // it. A shell word ends at whitespace, so a comma or a
                                // period touching the closing quote is inside the word,
                                // and the pasted command creates a directory whose name
                                // carries it.
                                suggestion.push_str(". Or ");
                                suggestion.push_str(&command[..1].to_lowercase());
                                suggestion.push_str(&command[1..]);
                            }
                            self.display.print_suggestion(suggestion);
                        }
                        OperationFailure::Privilege(refusal) => {
                            self.display.print_error(refusal.message());
                            self.display.print_suggestion(refusal.suggestion());
                        }
                        // A parse failure reaches only this arm. The three
                        // commands that render a failure themselves --
                        // commands/package/check.rs, audit.rs and install.rs --
                        // gate on `failure.is_environment_error()`, which a
                        // `ParseError` does not satisfy, so none of them sees one.
                        OperationFailure::Package(PackageError::ParseError {
                            name,
                            failed_file,
                            source,
                            ..
                        }) => {
                            // Built from the parts rather than from `Display`,
                            // which interpolates the whole chain including the
                            // "at line N, column M" suffix. The location header
                            // below states it once, so `Display` would say it
                            // twice.
                            match source.kind() {
                                PackageParseKind::Yaml { source: failure } => {
                                    self.display.print_error(format!(
                                        "Parse error in package `{name}`: {}",
                                        failure.reason()
                                    ));

                                    if let Some(at) = failure.location() {
                                        self.display.print_error_context(&format!(
                                            "{}:{}:{}",
                                            failed_file.display(),
                                            at.line(),
                                            at.column()
                                        ));
                                        if let Some(window) = crate::snippet::window(
                                            &failed_file,
                                            at.line(),
                                            at.column(),
                                        ) {
                                            self.display.print_error_context(&window);
                                        }
                                    }
                                }
                                // Every kind but `Yaml` is routed to
                                // another variant before it gets here, so this
                                // is growth insurance rather than a live case.
                                other => {
                                    self.display.print_error(format!(
                                        "Parse error in package `{name}`: {other}"
                                    ));
                                }
                            }
                        }
                        _ => {
                            self.display.print_error(err.to_string());
                        }
                    }
                }
            },

            PackageEvent::Canceled { reason, .. } => {
                self.display
                    .print_warning(format!("Operation canceled: {reason}"));
            }

            PackageEvent::RecommendStarted { recommend_name, .. } => {
                self.display
                    .print_info(format!("  Installing recommended: {recommend_name}..."));
            }

            PackageEvent::RecommendSucceeded { recommend_name, .. } => {
                self.display.print_success(format!("  ✓ {recommend_name}"));
            }

            PackageEvent::RecommendFailed {
                recommend_name,
                error,
                ..
            } => {
                self.display
                    .print_warning(format!("  ⚠ {recommend_name} failed: {error}"));
            }

            PackageEvent::DotfileDeploying { source, target, .. } => {
                let short_source = crate::display_manager::shorten_path(&source);
                let short_target = crate::display_manager::shorten_path(&target);
                self.display
                    .print_info(format!("  Deploying {short_source} → {short_target}"));
            }

            PackageEvent::DotfileDeployed {
                source,
                target,
                backup,
                ..
            } => {
                let short_source = crate::display_manager::shorten_path(&source);
                let short_target = crate::display_manager::shorten_path(&target);
                self.display
                    .print_success(format!("  {short_source} → {short_target}"));
                // Only when something was kept. Saying so on every deploy would
                // train the reader to skip the line that matters.
                if let Some(backup) = backup {
                    let short_backup = crate::display_manager::shorten_path(&backup);
                    self.display
                        .print_info(format!("    previous content copied to {short_backup}"));
                }
            }

            PackageEvent::DotfileSkipped {
                source,
                target,
                reason,
                ..
            } => {
                let short_source = crate::display_manager::shorten_path(&source);
                let short_target = crate::display_manager::shorten_path(&target);
                self.display.print_info(format!(
                    "  ⊘ {short_source} → {short_target} skipped: {reason}"
                ));
            }

            PackageEvent::DotfileConflict {
                source,
                target,
                diff,
                ..
            } => {
                let short_source = crate::display_manager::shorten_path(&source);
                let short_target = crate::display_manager::shorten_path(&target);
                self.display.println("");
                self.display
                    .print_warning(format!("  Conflict: {short_target}"));
                self.display
                    .print_progress(format!("{short_source} → {short_target}"));
                self.display.print_diff(&diff);
            }

            PackageEvent::DotfileDriftDetected {
                target, drift_type, ..
            } => {
                let short_target = crate::display_manager::shorten_path(&target);
                self.display
                    .print_warning(format!("  Drift in {short_target}: {drift_type}"));
            }

            // A warning: the file is one the user may still want, and nothing
            // will manage it again unless they act.
            PackageEvent::DotfileOrphaned {
                source,
                target,
                package,
                ..
            } => {
                let short_target = crate::display_manager::shorten_path(&target);
                let by = package
                    .map(|package| format!(" by package '{package}'"))
                    .unwrap_or_default();
                self.display.print_warning(format!(
                    "  Orphaned {short_target}: deployed from {source}{by}, and no entry deploys \
                     to it now. selfie leaves it in place; check whether you still need it"
                ));
            }

            PackageEvent::PostInstallNote { note, .. } => {
                self.display.print_info(format!("\n📋 {note}"));
            }

            PackageEvent::DotfileCleanupInfo {
                package_name,
                dotfile_targets,
                ..
            } => {
                self.display.print_info(format!(
                    "\nPackage '{}' has deployed dotfiles:",
                    package_name
                ));
                for target in &dotfile_targets {
                    self.display.print_info(format!("  - {}", target));
                }
                self.display.print_info(
                    "  These files were NOT removed. Delete them manually if no longer needed.",
                );
            }

            PackageEvent::PackageInfoLoaded { .. }
            | PackageEvent::EnvironmentStatusChecked { .. }
            | PackageEvent::PackageListReady { .. }
            | PackageEvent::PackageListLoaded { .. }
            | PackageEvent::CheckResultCompleted { .. }
            | PackageEvent::AuditResultCompleted { .. }
            | PackageEvent::ValidationResultCompleted { .. }
            | PackageEvent::PackageListItemCompleted { .. }
            | PackageEvent::RemovalDependencyInfo { .. }
            | PackageEvent::SpecListItemCompleted { .. }
            | PackageEvent::SpecListLoaded { .. }
            | PackageEvent::DotfileListLoaded { .. }
            | PackageEvent::SyncRepoStatus { .. }
            | PackageEvent::SyncDriftSummary { .. }
            | PackageEvent::SyncCommitCreated { .. } => {
                // These structured events are handled by command-specific handlers
                // If no custom handler processed them, just continue
            }
        }
    }
}

/// Handle console output appropriately
fn handle_console_output(output: ConsoleOutput) {
    match output {
        ConsoleOutput::Stdout(msg) => {
            println!("{msg}");
        }
        ConsoleOutput::Stderr(msg) => {
            eprintln!("{msg}");
        }
    }
}

/// Extension trait to add title case conversion to strings
trait ToTitleCase {
    fn to_title_case(&self) -> String;
}

impl ToTitleCase for str {
    fn to_title_case(&self) -> String {
        // Replace underscores with spaces and convert to title case
        let cleaned = self.replace('_', " ");
        let mut chars = cleaned.chars();
        match chars.next() {
            None => String::new(),
            Some(first) => {
                first.to_uppercase().collect::<String>() + &chars.as_str().to_lowercase()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::stream;

    #[test]
    fn test_to_title_case() {
        assert_eq!("check".to_title_case(), "Check");
        assert_eq!("install".to_title_case(), "Install");
        assert_eq!("VALIDATE".to_title_case(), "Validate");
        assert_eq!("".to_title_case(), "");
    }

    #[test]
    fn test_event_processor_creation() {
        let display = DisplayManager::new(false);
        let processor = EventProcessor::new(display);

        // Just verify it can be created
        assert!(std::mem::size_of_val(&processor) > 0);
    }

    #[tokio::test]
    async fn test_process_empty_stream() {
        let display = DisplayManager::new(false);
        let processor = EventProcessor::new(display);

        let events: Vec<PackageEvent> = vec![];
        let event_stream = Box::pin(stream::iter(events));
        let result = processor.process_events(event_stream, |_event| false).await;

        // Nothing reported a result, so nothing reported success either.
        assert_eq!(result.exit_code, 1);
    }

    // A stream that starts and then stops, as one does when the task running the
    // operation panics, exits 1 rather than 0.
    #[tokio::test]
    async fn a_stream_that_ends_without_a_result_exits_one() {
        let events = vec![PackageEvent::Started {
            operation_info: make_operation_info("panicked"),
        }];

        let processor = EventProcessor::new(DisplayManager::new(false));
        let result = processor
            .process_events(Box::pin(stream::iter(events)), |_event| false)
            .await;

        assert_eq!(result.exit_code, 1);
    }

    fn drift_checked(drift: usize, refused: usize) -> PackageEvent {
        use selfie::package::event::{OperationResult, OperationSuccess, StepCount};

        PackageEvent::Completed {
            operation_info: make_operation_info("drift"),
            result: OperationResult::Success(OperationSuccess::DotfileDriftChecked {
                drift_count: drift,
                total_count: 1,
                refused_count: refused,
                unverified_count: 0,
                orphan_count: 0,
                environment: "test".to_string(),
                steps_completed: StepCount::new(1, 1),
            }),
        }
    }

    // The verdict is taken before the command's own handler sees the event, so a
    // handler that renders the completion itself cannot lose the exit code.
    #[tokio::test]
    async fn a_claimed_completion_still_exits_with_its_outcome() {
        for (event, expected) in [
            (drift_checked(0, 0), 0),
            (drift_checked(1, 0), 3),
            (drift_checked(1, 1), 1),
        ] {
            let processor = EventProcessor::new(DisplayManager::new(false));
            let result = processor
                .process_events(Box::pin(stream::iter(vec![event])), |_event| true)
                .await;
            assert_eq!(result.exit_code, expected);
        }
    }

    // A cancellation a handler claims still exits 130, and nothing after it is
    // read.
    #[tokio::test]
    async fn a_claimed_cancellation_exits_130_and_stops() {
        let events = vec![
            PackageEvent::Canceled {
                operation_info: make_operation_info("cancelled"),
                reason: "Ctrl+C".to_string(),
            },
            drift_checked(0, 0),
        ];

        let mut completions_seen = 0;
        let processor = EventProcessor::new(DisplayManager::new(false));
        let result = processor
            .process_events(Box::pin(stream::iter(events)), |event| {
                if matches!(event, PackageEvent::Completed { .. }) {
                    completions_seen += 1;
                }
                true
            })
            .await;

        assert_eq!(result.exit_code, 130);
        assert_eq!(completions_seen, 0);
    }

    // Control: a custom handler claiming the completion still counts as a result,
    // so the check above cannot be satisfied by failing every claimed stream.
    #[tokio::test]
    async fn a_claimed_completion_is_still_a_result() {
        use selfie::package::event::{OperationResult, OperationSuccess};

        let events = vec![PackageEvent::Completed {
            operation_info: make_operation_info("claimed"),
            result: OperationResult::Success(OperationSuccess::Generic("done".to_string())),
        }];

        let processor = EventProcessor::new(DisplayManager::new(false));
        let result = processor
            .process_events(Box::pin(stream::iter(events)), |_event| true)
            .await;

        assert_eq!(result.exit_code, 0);
    }

    #[tokio::test]
    async fn test_custom_handler_behavior() {
        let display = DisplayManager::new(false);
        let processor = EventProcessor::new(display);

        let events: Vec<PackageEvent> = vec![];
        let event_stream = Box::pin(stream::iter(events));

        // Test that custom handler gets called with None for empty stream
        let mut handler_called = false;
        let result = processor
            .process_events(event_stream, |_event| {
                handler_called = true;
                true
            })
            .await;

        assert_eq!(result.exit_code, 1);
        // Handler should not be called for empty stream
        assert!(!handler_called);
    }

    fn make_operation_info(package_name: &str) -> selfie::package::event::OperationInfo {
        use selfie::package::event::OperationContext;
        use selfie::package::event::OperationType;

        selfie::package::event::OperationInfo {
            id: uuid::Uuid::new_v4(),
            operation_type: OperationType::PackageCheck,
            package_name: package_name.to_string(),
            environment: "test".to_string(),
            context: OperationContext {
                package_path: None,
                target_environment: None,
            },
            timestamp: std::time::Instant::now(),
        }
    }

    #[tokio::test]
    async fn a_failed_completion_produces_a_failure_result() {
        use selfie::package::event::{OperationFailure, OperationResult};

        let op = make_operation_info("nonexistent-test-package");

        let events: Vec<PackageEvent> = vec![
            PackageEvent::Started {
                operation_info: op.clone(),
            },
            PackageEvent::Progress {
                operation_info: op.clone(),
                step: 1,
                total_steps: 2,
                percent_complete: 0.5,
                message: "Loading package file".to_string(),
            },
            PackageEvent::Completed {
                operation_info: op,
                result: OperationResult::Failure(OperationFailure::Generic(
                    "Package not found".to_string(),
                )),
            },
        ];

        let display = DisplayManager::new(false);
        let processor = EventProcessor::new(display);
        let event_stream = Box::pin(stream::iter(events));
        let result = processor.process_events(event_stream, |_event| false).await;

        assert_eq!(result.exit_code, 1);
    }

    // The CLI half of the sudo refusal: exit non-zero, and put the two halves of
    // the refusal in their own channels rather than one blob. Constructed as a
    // stream rather than driven through a service, per the event-consumer
    // convention.
    #[tokio::test]
    async fn a_privilege_refusal_exits_non_zero_and_keeps_its_suggestion() {
        use selfie::package::event::{OperationFailure, OperationResult};
        use selfie::privilege::{Elevation, Privilege, SudoPolicy, WriteScope};

        // Minted through the policy rather than constructed here: `SudoRefusal`'s
        // field is private, so the library is the only thing that can produce
        // one. An adapter cannot fabricate a refusal that never happened.
        struct UnderSudo;
        impl Privilege for UnderSudo {
            fn elevation(&self) -> Elevation {
                Elevation::Sudo
            }
        }
        let refusal = SudoPolicy::new(UnderSudo)
            .refusal(WriteScope::Dotfiles)
            .expect("a sudo run must be refused");

        let events: Vec<PackageEvent> = vec![PackageEvent::Completed {
            operation_info: make_operation_info("dotfiles"),
            result: OperationResult::Failure(OperationFailure::Privilege(refusal)),
        }];

        let display = DisplayManager::new(false);
        let display_for_assert = display.clone();
        let processor = EventProcessor::new(display);
        let event_stream = Box::pin(stream::iter(events));
        let result = processor.process_events(event_stream, |_event| false).await;

        assert_eq!(result.exit_code, 1);

        // The collected detail carries both halves, so `--verbose` summaries and
        // the MCP server's JSON still say what to do about it.
        let collected = display_for_assert.collected_errors();
        let message = &collected.first().expect("an error was collected").message;
        assert!(message.contains("under sudo"), "got: {message}");
        assert!(message.contains("--allow-sudo"), "got: {message}");
    }

    #[tokio::test]
    async fn test_custom_handler_counts_event_types() {
        use selfie::package::event::{OperationFailure, OperationResult};

        let op = make_operation_info("nonexistent-test-package");

        let events: Vec<PackageEvent> = vec![
            PackageEvent::Started {
                operation_info: op.clone(),
            },
            PackageEvent::Progress {
                operation_info: op.clone(),
                step: 1,
                total_steps: 3,
                percent_complete: 1.0 / 3.0,
                message: "Step 1".to_string(),
            },
            PackageEvent::Progress {
                operation_info: op.clone(),
                step: 2,
                total_steps: 3,
                percent_complete: 2.0 / 3.0,
                message: "Step 2".to_string(),
            },
            PackageEvent::Completed {
                operation_info: op,
                result: OperationResult::Failure(OperationFailure::Generic("Failed".to_string())),
            },
        ];

        let display = DisplayManager::new(false);
        let processor = EventProcessor::new(display);

        let mut started_events_seen = 0;
        let mut progress_events_seen = 0;
        let mut completed_events_seen = 0;

        let event_stream = Box::pin(stream::iter(events));
        let result = processor
            .process_events(event_stream, |event| match event {
                PackageEvent::Started { .. } => {
                    started_events_seen += 1;
                    false
                }
                PackageEvent::Progress { .. } => {
                    progress_events_seen += 1;
                    false
                }
                PackageEvent::Completed { .. } => {
                    completed_events_seen += 1;
                    false
                }
                _ => false,
            })
            .await;

        assert_eq!(started_events_seen, 1);
        assert_eq!(progress_events_seen, 2);
        assert_eq!(completed_events_seen, 1);
        assert_eq!(result.exit_code, 1);
    }

    #[tokio::test]
    async fn test_canceled_event_stops_processing_with_exit_130() {
        use selfie::package::event::{OperationResult, OperationSuccess};

        let op = make_operation_info("cancel-test-package");

        // Canceled followed by Completed — the Completed should never be processed
        let events: Vec<PackageEvent> = vec![
            PackageEvent::Started {
                operation_info: op.clone(),
            },
            PackageEvent::Canceled {
                operation_info: op.clone(),
                reason: "User pressed Ctrl+C".to_string(),
            },
            PackageEvent::Completed {
                operation_info: op,
                result: OperationResult::Success(OperationSuccess::package_checked(
                    "cancel-test-package".to_string(),
                    "test".to_string(),
                    selfie::package::event::CheckVerdict::Installed,
                    (1, 1).into(),
                )),
            },
        ];

        let display = DisplayManager::new(false);
        let processor = EventProcessor::new(display);
        let event_stream = Box::pin(stream::iter(events));

        let mut events_after_cancel = 0;
        let result = processor
            .process_events(event_stream, |event| {
                if matches!(event, PackageEvent::Completed { .. }) {
                    events_after_cancel += 1;
                }
                false
            })
            .await;

        // Should use exit code 130 (128 + SIGINT)
        assert_eq!(result.exit_code, 130);
        // Completed event after Canceled should not have been processed
        assert_eq!(events_after_cancel, 0);
    }

    #[tokio::test]
    async fn test_completed_failure_collects_error_detail() {
        use selfie::package::event::{CommandFailure, OperationFailure, OperationResult};

        let op = make_operation_info("fail-pkg");

        let events: Vec<PackageEvent> = vec![PackageEvent::Completed {
            operation_info: op,
            result: OperationResult::Failure(OperationFailure::CommandError(
                CommandFailure::ExecutionFailed {
                    command: "brew install fail-pkg".to_string(),
                    exit_code: Some(1),
                    stderr: selfie::commands::BoundedText::bound(b"not found"),
                },
            )),
        }];

        let display = DisplayManager::new(false);
        let display_clone = display.clone();
        let processor = EventProcessor::new(display);
        let event_stream = Box::pin(stream::iter(events));
        processor.process_events(event_stream, |_event| false).await;

        let errors = display_clone.collected_errors();
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].package_name, "fail-pkg");
        assert_eq!(errors[0].command.as_deref(), Some("brew install fail-pkg"));
        assert_eq!(errors[0].exit_code, Some(1));
        assert_eq!(errors[0].stderr.as_deref(), Some("not found"));
    }

    // A completed apply that refused an entry exits non-zero.
    //
    // The library counts the refusal; this asserts the adapter acts on it. A
    // script checking `$?` is the caller this matters for.
    #[tokio::test]
    async fn a_completed_apply_that_refused_something_exits_non_zero() {
        use selfie::package::event::{OperationResult, OperationSuccess, StepCount};

        let events: Vec<PackageEvent> = vec![PackageEvent::Completed {
            operation_info: make_operation_info("apply"),
            result: OperationResult::Success(OperationSuccess::DotfilesApplied {
                deployed_count: 0,
                skipped_count: 0,
                conflict_count: 0,
                refused_count: 1,
                orphan_count: 0,
                environment: "test".to_string(),
                steps_completed: StepCount::new(1, 1),
            }),
        }];

        let display = DisplayManager::new(false);
        let display_clone = display.clone();
        let processor = EventProcessor::new(display);
        let event_stream = Box::pin(stream::iter(events));
        let result = processor.process_events(event_stream, |_event| false).await;

        assert_eq!(result.exit_code, 1);
        assert_eq!(
            display_clone.collected_errors().len(),
            1,
            "the refusal belongs in the end-of-run summary too"
        );
    }

    // Control: the same event with nothing refused still exits 0.
    //
    // Without this, an implementation that failed every completed apply would
    // satisfy the test above.
    #[tokio::test]
    async fn a_completed_apply_that_refused_nothing_exits_zero() {
        use selfie::package::event::{OperationResult, OperationSuccess, StepCount};

        let events: Vec<PackageEvent> = vec![PackageEvent::Completed {
            operation_info: make_operation_info("apply"),
            result: OperationResult::Success(OperationSuccess::DotfilesApplied {
                deployed_count: 1,
                skipped_count: 0,
                conflict_count: 0,
                refused_count: 0,
                orphan_count: 0,
                environment: "test".to_string(),
                steps_completed: StepCount::new(1, 1),
            }),
        }];

        let display = DisplayManager::new(false);
        let processor = EventProcessor::new(display);
        let event_stream = Box::pin(stream::iter(events));
        let result = processor.process_events(event_stream, |_event| false).await;

        assert_eq!(result.exit_code, 0);
    }

    #[tokio::test]
    async fn test_completed_generic_failure_collects_error_detail() {
        use selfie::package::event::{OperationFailure, OperationResult};

        let op = make_operation_info("generic-fail");

        let events: Vec<PackageEvent> = vec![PackageEvent::Completed {
            operation_info: op,
            result: OperationResult::Failure(OperationFailure::Generic(
                "something went wrong".to_string(),
            )),
        }];

        let display = DisplayManager::new(false);
        let display_clone = display.clone();
        let processor = EventProcessor::new(display);
        let event_stream = Box::pin(stream::iter(events));
        processor.process_events(event_stream, |_event| false).await;

        let errors = display_clone.collected_errors();
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].package_name, "generic-fail");
        assert_eq!(errors[0].message, "something went wrong");
        assert!(errors[0].command.is_none());
        assert!(errors[0].exit_code.is_none());
    }

    #[test]
    fn test_title_case_with_different_operations() {
        // Test the ToTitleCase trait with operation names that might come from the system
        assert_eq!("package_check".to_title_case(), "Package check");
        assert_eq!("package_install".to_title_case(), "Package install");
        assert_eq!("package_validate".to_title_case(), "Package validate");
        assert_eq!("PACKAGE_LIST".to_title_case(), "Package list");
    }
}
