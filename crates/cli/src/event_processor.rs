//! Event processing utilities for CLI commands
//!
//! This module provides a reusable event processor that can handle package events
//! from the selfie library and present them consistently across different CLI commands.
//!
//! # Usage
//!
//! ## Event Processing with Custom Handlers
//!
//! All commands use `process_events` which allows custom handling
//! of specific events while providing default behavior for standard events:
//!
//! ```rust,ignore
//! async fn handle_command_with_custom_progress(
//!     display: DisplayManager,
//!     event_stream: EventStream
//! ) -> i32 {
//!     let processor = EventProcessor::new(display);
//!
//!     processor.process_events(event_stream, |event| {
//!         match event {
//!             PackageEvent::Progress { percent_complete, step, total_steps, message, .. } => {
//!                 // Custom progress handling
//!                 println!("[{:.0}%] Step {}/{}: {}",
//!                     percent_complete * 100.0, step, total_steps, message);
//!                 true // Handled - continue processing
//!             }
//!             PackageEvent::Warning { message, .. } => {
//!                 // Custom warning handling
//!                 eprintln!("Warning: {}", message);
//!                 true // Handled - continue processing
//!             }
//!             _ => false, // Use default handling for other events
//!         }
//!     }).await
//! }
//! ```
//!
//! The custom handler should return:
//! - `true` to continue processing after handling the event (skip default handling)
//! - `false` to use the default handling for the event

use futures::StreamExt;
use selfie::package::{
    event::{ConsoleOutput, EventStream, OperationResult, PackageEvent},
    port::{PackageError, PackageListError},
};

use crate::display_manager::{DisplayManager, ErrorDetail};

/// Result of processing events, including metadata about what was encountered
#[derive(Debug, Clone)]
pub struct EventProcessingResult {
    /// The exit code for the operation
    pub exit_code: i32,
    /// Whether any errors were encountered during processing
    pub had_errors: bool,
}

impl EventProcessingResult {
    fn new() -> Self {
        Self {
            exit_code: 0,
            had_errors: false,
        }
    }
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
        let mut result = EventProcessingResult::new();

        while let Some(event) = stream.next().await {
            // Try custom handler first
            if custom_handler(&event) {
                // Custom handler handled the event, continue to next event
                continue;
            }

            // Fall back to default handling
            if self.handle_event(event, &mut result) {
                break;
            }
        }

        self.display.finish();

        result
    }

    /// Handle a single event and update the result as needed
    ///
    /// Returns true if processing should stop (early termination)
    fn handle_event(&self, event: PackageEvent, result: &mut EventProcessingResult) -> bool {
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

            PackageEvent::Warning { message, .. } => {
                self.display.print_warning(message);
                // Warnings don't set failure exit code by default
            }

            PackageEvent::Error {
                operation_info,
                message,
                error,
            } => {
                self.display.collect_error(ErrorDetail {
                    package_name: operation_info.package_name,
                    operation: operation_info.operation_type.to_string(),
                    command: None,
                    exit_code: None,
                    stderr: None,
                    message: format!("{message}: {error}"),
                });
                self.display.print_error(format!("{message}: {error}"));
                result.exit_code = 1;
                result.had_errors = true;
            }

            PackageEvent::Completed {
                operation_info,
                result: op_result,
            } => match op_result {
                // A completed operation that refused part of its work is not a
                // success to a script reading the exit code: `selfie apply` would
                // otherwise report 0 having deployed nothing (selfie-c28). The
                // library decides whether refusals happened; this only decides
                // what that means for a terminal.
                OperationResult::Success(success) if success.had_refusals() => {
                    self.display.collect_error(ErrorDetail {
                        package_name: operation_info.package_name,
                        operation: operation_info.operation_type.to_string(),
                        command: None,
                        exit_code: None,
                        stderr: None,
                        message: success.to_string(),
                    });
                    self.display.print_error(success.to_string());
                    result.exit_code = 1;
                    result.had_errors = true;
                }
                OperationResult::Success(success) => {
                    self.display.print_success(success.to_string());
                }
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
                        OperationFailure::PackageList(
                            PackageListError::PackageDirectoryNotFound(path),
                        ) => {
                            self.display.print_error(format!(
                                "Package directory not found: {}",
                                path.display()
                            ));
                            self.display.print_suggestion(format!(
                                "Create the directory with 'mkdir -p {}' or set a different path with 'selfie config --package-directory <path>'",
                                path.display()
                            ));
                        }
                        _ => {
                            self.display.print_error(err.to_string());
                        }
                    }
                    result.exit_code = 1;
                    result.had_errors = true;
                }
            },

            PackageEvent::Canceled { reason, .. } => {
                self.display
                    .print_warning(format!("Operation canceled: {reason}"));
                // 128 + 2 (SIGINT) is the Unix convention for Ctrl+C termination
                result.exit_code = 130;
                result.had_errors = true;
                return true; // Stop processing after cancellation
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

            PackageEvent::DotfileDeployed { source, target, .. } => {
                let short_source = crate::display_manager::shorten_path(&source);
                let short_target = crate::display_manager::shorten_path(&target);
                self.display
                    .print_success(format!("  {short_source} → {short_target}"));
            }

            PackageEvent::DotfileSkipped { source, reason, .. } => {
                let short_source = crate::display_manager::shorten_path(&source);
                self.display
                    .print_info(format!("  ⊘ {short_source} skipped: {reason}"));
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
            | PackageEvent::SyncRepoStatus { .. }
            | PackageEvent::SyncDriftSummary { .. }
            | PackageEvent::SyncCommitCreated { .. } => {
                // These structured events are handled by command-specific handlers
                // If no custom handler processed them, just continue
            }
        }

        false // Continue processing
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

        // Empty stream should return success
        assert_eq!(result.exit_code, 0);
        assert!(!result.had_errors);
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

        assert_eq!(result.exit_code, 0);
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
    async fn test_error_event_produces_failure_result() {
        use selfie::package::event::StreamedError;
        use selfie::package::event::{OperationFailure, OperationResult};
        use selfie::package::port::PackageRepoError;

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
            PackageEvent::Error {
                operation_info: op.clone(),
                error: StreamedError::PackageRepoError(PackageRepoError::FileSystemError(
                    selfie::fs::FileSystemError::IoError(std::sync::Arc::new(std::io::Error::new(
                        std::io::ErrorKind::NotFound,
                        "package not found",
                    ))),
                )),
                message: "Package not found".to_string(),
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
        assert!(result.had_errors);
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
        assert!(result.had_errors);
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
                    selfie::package::event::CheckResult::NoCheckCommand,
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
        assert!(result.had_errors);
        // Completed event after Canceled should not have been processed
        assert_eq!(events_after_cancel, 0);
    }

    #[tokio::test]
    async fn test_error_event_collects_error_detail() {
        use selfie::package::event::StreamedError;
        use selfie::package::port::PackageRepoError;

        let op = make_operation_info("broken-pkg");

        let events: Vec<PackageEvent> = vec![PackageEvent::Error {
            operation_info: op,
            error: StreamedError::PackageRepoError(PackageRepoError::FileSystemError(
                selfie::fs::FileSystemError::IoError(std::sync::Arc::new(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "file missing",
                ))),
            )),
            message: "Could not load".to_string(),
        }];

        let display = DisplayManager::new(false);
        let display_clone = display.clone();
        let processor = EventProcessor::new(display);
        let event_stream = Box::pin(stream::iter(events));
        processor.process_events(event_stream, |_event| false).await;

        let errors = display_clone.collected_errors();
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].package_name, "broken-pkg");
        assert_eq!(errors[0].operation, "package_check");
        assert!(errors[0].message.contains("Could not load"));
        assert!(errors[0].command.is_none());
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

    /// A completed apply that refused an entry exits non-zero.
    ///
    /// The library counts the refusal; this asserts the adapter acts on it. A
    /// script checking `$?` is the caller selfie-c28 is about.
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
        assert!(result.had_errors);
        assert_eq!(
            display_clone.collected_errors().len(),
            1,
            "the refusal belongs in the end-of-run summary too"
        );
    }

    /// Control: the same event with nothing refused still exits 0.
    ///
    /// Without this, an implementation that failed every completed apply would
    /// satisfy the test above.
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
                environment: "test".to_string(),
                steps_completed: StepCount::new(1, 1),
            }),
        }];

        let display = DisplayManager::new(false);
        let processor = EventProcessor::new(display);
        let event_stream = Box::pin(stream::iter(events));
        let result = processor.process_events(event_stream, |_event| false).await;

        assert_eq!(result.exit_code, 0);
        assert!(!result.had_errors);
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
