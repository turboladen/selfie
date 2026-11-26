//! Turning a library [`EventStream`] into terminal output, the same way for every
//! CLI command.
//!
//! Every command goes through [`EventProcessor::process_events`], passing a
//! handler that returns `true` for an event it rendered itself and `false` to
//! take the default rendering.

use crate::received;
use crate::terminal_text::{Command, DisplayPath, Name, Output, Prose, Shown, lit, shown};
use futures::StreamExt;
use selfie::package::{
    event::{
        ConsoleOutput, DotfileSource, EventStream, OperationInfo, OperationResult, OperationType,
        Outcome, PackageEvent, RefusedPackage, Settled, StepId, StepKind,
    },
    port::{PackageError, PackageParseKind},
};
use selfie::parts::{Message, SuggestedCommand};

use crate::display_manager::Channel;
use crate::display_manager::{Diff, DisplayManager, ErrorDetail};
use crate::entry_rows::{EntryOutcome, EntryRow, under_row};
use crate::source_paths;

/// How a command's run ended, as the process reports it.
///
/// | code | meaning |
/// | ---- | ------- |
/// | 0    | clean |
/// | 1    | failed, or refused part of its work; for `package check` and `package audit`, also a command that timed out or could not be started |
/// | 2    | usage error: a bad command line, or a prompt with no terminal |
/// | 3    | finished with something to report: a finding, or a package it could not answer for |
/// | 130  | canceled (128 + SIGINT) |
///
/// Exits compare by how much they say about the run, so the `max` of several
/// results is the one a run made of them reports: clean, then found, failed,
/// usage, and canceled highest. A failure outranks a finding and a finding a
/// clean result. A usage error ranks above a failure because it stopped the
/// run before it could fail or finish, and a cancel above everything because it
/// is how the run ended.
// A new code takes the next free value in 3-63, never 64-78 (`sysexits.h`) or
// 126 and up (reserved by the shell), and no code is renumbered or reused. The
// README's exit-code table is the public contract and changes with this enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[must_use]
pub(crate) enum Exit {
    /// Did everything asked, and found nothing to report.
    Clean,
    /// Finished with something to report: what it was asked to look for, or a
    /// package it could not answer for.
    Found,
    /// Failed, refused part of its work, or ended without saying how it went.
    Failed,
    /// Needed an answer and had no terminal to ask on.
    Usage,
    /// Interrupted.
    Canceled,
}

impl Exit {
    // No wildcard, so a new variant has to be ranked here.
    fn rank(self) -> u8 {
        match self {
            Exit::Clean => 0,
            Exit::Found => 1,
            Exit::Failed => 2,
            Exit::Usage => 3,
            Exit::Canceled => 4,
        }
    }

    /// The process exit code.
    pub(crate) fn code(self) -> i32 {
        match self {
            Exit::Clean => 0,
            Exit::Failed => 1,
            Exit::Usage => 2,
            Exit::Found => 3,
            // 128 + 2 (SIGINT), the Unix convention for Ctrl+C termination.
            Exit::Canceled => 130,
        }
    }
}

impl Ord for Exit {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.rank().cmp(&other.rank())
    }
}

impl PartialOrd for Exit {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
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
    /// How the run ended.
    pub exit: Exit,
}

/// A reusable event processor for handling package operation events
///
/// This processor standardizes how events from the selfie library are handled
/// and displayed in the CLI, reducing boilerplate across different commands.
#[derive(Debug)]
pub struct EventProcessor {
    display: DisplayManager,
    partial: PartialLines,
}

impl EventProcessor {
    /// Create a new event processor with the given display manager
    pub fn new(display: DisplayManager) -> Self {
        Self {
            display,
            partial: PartialLines::default(),
        }
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
        mut self,
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
            // A step still open when the operation ends is cleared without a
            // result line, before anything renders the ending: only its own end
            // event says how it went. A step's unfinished lines print before
            // either, so they still carry the step's label and join its tail.
            let canceled = match &event {
                PackageEvent::Completed { result, .. } => {
                    ending = Some(Exit::from(result.outcome()));
                    self.flush(None);
                    self.display.clear_waiting();
                    false
                }
                PackageEvent::Canceled { .. } => {
                    ending = Some(Exit::Canceled);
                    self.flush(None);
                    self.display.clear_waiting();
                    true
                }
                PackageEvent::StepEnded { step, .. } => {
                    self.flush(Some(*step));
                    false
                }
                _ => false,
            };

            if !custom_handler(&event) {
                self.handle_event(event);
            }

            // Nothing after a cancellation is read, whoever rendered it.
            if canceled {
                break;
            }
        }

        self.flush(None);
        // A stream that ends without saying how the operation went, as one does when
        // the task running it panics, is a failure: nothing reported the work done.
        let exit = ending.unwrap_or_else(|| {
            self.display
                .print_error(lit!("The operation ended without reporting a result"));
            Exit::Failed
        });

        self.display.finish();

        EventProcessingResult { exit }
    }

    /// Print the unfinished lines of step `only`, or of every step when `only`
    /// is `None`.
    fn flush(&mut self, only: Option<StepId>) {
        for (step, line) in self.partial.take(only) {
            self.display.command_output(step, Output::line(&line));
        }
    }

    /// Render one event the default way.
    fn handle_event(&mut self, event: PackageEvent) {
        // A line that names a source prints with its heading as one block, so a
        // conflict prompt printing on the service's thread cannot land between
        // them.
        let _block = matches!(
            event,
            PackageEvent::DotfileDeployed { .. }
                | PackageEvent::DotfileSkipped { .. }
                | PackageEvent::DotfileConflict { .. }
                | PackageEvent::DotfileOrphaned { .. }
                | PackageEvent::DotfileDriftDetected { .. }
        )
        .then(|| self.display.hold_block());
        match event {
            // The header and every local step are commentary on a run, shown only
            // under `--verbose`. A step waiting on something outside selfie shows
            // at every verbosity. No command changes this for itself.
            PackageEvent::Started { operation_info } => {
                if self.display.is_verbose() {
                    self.display.print_run_note(started_line(&operation_info));
                }
            }

            PackageEvent::Progress { kind, message, .. } => match kind {
                StepKind::Waiting(step) => {
                    self.display.start_waiting(step, received::step(&message));
                }
                StepKind::Local => self.display.print_status(received::step(&message)),
            },

            PackageEvent::StepEnded { step, ending, .. } => {
                self.display.end_waiting(step, ending);
            }

            // A configured command's own output is not the answer: stderr, and at
            // default verbosity only as the waiting spinner's latest line. A read
            // ends wherever the pipe did, so only its complete lines print here.
            PackageEvent::Info { step, output, .. } => {
                for piece in self.partial.push(step, Pipe::of(&output), output.text()) {
                    match piece {
                        Piece::Line(line) => self.display.command_output(step, Output::line(&line)),
                        Piece::Frame(frame) => {
                            self.display.command_frame(step, Output::line(&frame));
                        }
                    }
                }
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
                self.display.print_warning(received::skipped_spec(&error));
            }

            PackageEvent::CollisionsUnchecked { sources, .. } => {
                self.display
                    .print_warning(received::collisions_unchecked(&sources));
            }

            PackageEvent::PackagesRefused {
                reason, packages, ..
            } => {
                for line in refused_packages_lines(&reason, &packages, self.display.is_verbose()) {
                    self.display.print_warning(line);
                }
            }

            PackageEvent::Warning { message, .. } => {
                self.display.print_warning(received::message(message));
                // Warnings don't set failure exit code by default
            }
            PackageEvent::ComposedWarning { warning, .. } => {
                self.display.print_warning(received::composed(&warning));
            }

            // The run's result counts each, so the exit code comes from there.
            PackageEvent::DotfileRefused { refusal, .. } => {
                self.display
                    .print_warning(received::composed(&refusal.message));
            }
            PackageEvent::DotfileFailed { failure, .. } => {
                self.display
                    .print_error(received::composed(&failure.message));
            }

            PackageEvent::Completed {
                operation_info,
                result: op_result,
            } => match op_result {
                // The level follows the library's verdict, so a run that refused
                // part of its work reads as an error and one that found drift as
                // a warning. The exit code was scored in `process_events`.
                // The summary is the answer, so it goes to stdout at every outcome,
                // marked by it. A run that failed on a success, as a refusal does,
                // also joins the error summary. An `OperationFailure` is an error,
                // not an answer, and stays on stderr below.
                OperationResult::Success(success) => {
                    let outcome = success.outcome();
                    let line = received::success(&success);
                    if outcome == Outcome::Failed {
                        self.display
                            .collect_error(ErrorDetail::of(&operation_info, line.clone()));
                    }
                    self.display.print_result(outcome, line);
                }
                OperationResult::Failure(err) => {
                    use selfie::package::event::{CommandFailure, OperationFailure};

                    // Collect structured error detail for end-of-operation summary
                    // The summary collects the line printed below, so the
                    // two name a spec alike.
                    let message = received::failure(&err);
                    let detail = ErrorDetail::of(&operation_info, message.clone());
                    let stderr_lines: Vec<Output> = match &err {
                        OperationFailure::CommandError(CommandFailure::ExecutionFailed {
                            stderr,
                            ..
                        }) => stderr.as_str().lines().map(Output::line).collect(),
                        _ => Vec::new(),
                    };
                    // The command as the sentence bounds it, so the summary carries
                    // no more of a long command than the error does.
                    let error_detail = match &err {
                        OperationFailure::CommandError(
                            failure @ CommandFailure::ExecutionFailed { exit_code, .. },
                        ) => ErrorDetail {
                            command: Some(Command::written(failure.command_bounded().as_str())),
                            exit_code: *exit_code,
                            stderr: Some(stderr_lines.clone()),
                            ..detail
                        },
                        _ => detail,
                    };
                    self.display.collect_error(error_detail);

                    match err {
                        // The library's sentence, so its hint about a `name:` field
                        // reaches the terminal.
                        OperationFailure::Package(PackageError::PackageNotFound { .. }) => {
                            self.display.print_error(message.clone());
                        }
                        OperationFailure::PackageList(listing) => {
                            // The reason, not "not found". A plain file or a dangling
                            // link at the path is not a missing directory, and saying
                            // it is sends the user to a `mkdir -p` that fails with
                            // "File exists" or "No such file or directory". The
                            // repository classified the path once and the error carries
                            // the answer, so this renders it rather than looking again.
                            self.display.print_error(shown!(
                                "Package directory at {}",
                                received::composed(&listing)
                            ));
                            // `selfie config` requires a subcommand and has only
                            // one, `validate`, so it is not a way to set the
                            // directory — naming it here sends a reader whose
                            // directory is already missing to a usage error.
                            // The global flag is per-run and the file is the
                            // durable fix, so both are named.
                            let settings = lit!(
                                "Edit 'package_directory' in your config file, or name another for this run with the global flag 'selfie --package-directory <path> …'"
                            );
                            // `mkdir -p` answers an empty path and nothing else, so
                            // the remedy comes from the reason rather than being
                            // offered for every state. The two settings are named
                            // either way, since they are the fix when no command is.
                            let mkdir = match listing.state() {
                                selfie::fs::DirectoryState::Absent(reason)
                                    if reason.creatable() =>
                                {
                                    SuggestedCommand::make_directory(listing.path())
                                }
                                _ => None,
                            };
                            let suggestion = match mkdir {
                                // The command ends the sentence, and nothing may
                                // follow it on its line. A shell word ends at
                                // whitespace, so a comma or a period touching the
                                // closing quote is inside the word, and the pasted
                                // command creates a directory whose name carries it.
                                Some(mkdir) => {
                                    let shown = received::suggested(&mkdir);
                                    received::noted_suggestion(
                                        shown!("{}. Or create it with: {}", settings, shown),
                                        &mkdir,
                                        &shown,
                                    )
                                }
                                None => settings.into(),
                            };
                            self.display.print_suggestion(suggestion);
                        }
                        OperationFailure::Uncreatable { .. } => {
                            self.display.print_error(message.clone());
                        }
                        OperationFailure::Privilege(refusal) => {
                            self.display
                                .print_error(received::message(refusal.message()));
                            self.display
                                .print_suggestion(received::message(refusal.suggestion()));
                        }
                        // A parse failure reaches only this arm. The three
                        // commands that render a failure themselves --
                        // commands/package/check.rs, audit.rs and install.rs --
                        // gate on `failure.is_environment_error()`, which a
                        // `ParseError` does not satisfy, so none of them sees one.
                        OperationFailure::Package(PackageError::ParseError {
                            name,
                            failed_file,
                            cause,
                            ..
                        }) => {
                            // Built from the parts rather than from `Display`,
                            // which interpolates the whole chain including the
                            // "at line N, column M" suffix. The location header
                            // below states it once, so `Display` would say it
                            // twice.
                            match cause.kind() {
                                PackageParseKind::Yaml { cause: failure } => {
                                    self.display.print_error(shown!(
                                        "Parse error in package `{}`: {}",
                                        Name::new(&name),
                                        received::message(failure.reason())
                                    ));

                                    if let Some(at) = failure.location() {
                                        self.display.print_error_context(shown!(
                                            "{}:{}:{}",
                                            DisplayPath::of(&failed_file),
                                            at.line(),
                                            at.column()
                                        ));
                                        if let Some(window) = crate::snippet::window(
                                            &failed_file,
                                            at.line(),
                                            at.column(),
                                        ) {
                                            self.display.print_error_context(window);
                                        }
                                    }
                                }
                                // Every kind but `Yaml` is routed to
                                // another variant before it gets here, so this
                                // is growth insurance rather than a live case.
                                other => {
                                    self.display.print_error(shown!(
                                        "Parse error in package `{}`: {}",
                                        Name::new(&name),
                                        received::composed(other)
                                    ));
                                }
                            }
                        }
                        // The command's own stderr, bounded, at every verbosity: its
                        // output is hidden while it runs, and the tail its step showed
                        // may be stdout alone. Lines the tail already showed are left
                        // out.
                        OperationFailure::CommandError(CommandFailure::ExecutionFailed {
                            ..
                        }) => {
                            self.display.print_error(message.clone());
                            self.display.print_command_stderr(&stderr_lines);
                        }
                        OperationFailure::InvalidSpec { ref issues, .. } => {
                            self.display.print_error(message.clone());
                            crate::commands::validation_display::print_issues(
                                &self.display,
                                issues,
                            );
                        }
                        _ => {
                            self.display.print_error(message.clone());
                        }
                    }
                }
            },

            PackageEvent::Canceled { reason, .. } => {
                self.display
                    .print_warning(shown!("Operation canceled: {}", received::message(reason)));
            }

            // The recommended package's install command is its own waiting step,
            // which names it; this announcement is detail.
            PackageEvent::RecommendStarted { recommend_name, .. } => {
                self.display.print_status(shown!(
                    "Installing recommended: {}",
                    Name::new(&recommend_name)
                ));
            }

            // `print_success` and `print_warning` add the line's mark, so the
            // message carries none of its own.
            PackageEvent::RecommendSucceeded { recommend_name, .. } => {
                self.display.print_success(Name::new(&recommend_name));
            }

            PackageEvent::RecommendFailed {
                recommend_name,
                error,
                ..
            } => {
                self.display.print_warning(shown!(
                    "{} failed: {}",
                    Name::new(&recommend_name),
                    received::failure(&error)
                ));
            }

            // Shown at every verbosity, not through `print_status`: a run that
            // skips a check or an install says why, by the maintainer's choice
            // that refines what the output contract hides by default.
            PackageEvent::SettledEarlier {
                package_name,
                settled,
                ..
            } => {
                self.display
                    .print_progress(settled_earlier_line(&package_name, settled));
            }

            PackageEvent::RecommendsUntried { names, .. } => {
                self.display.print_warning(shown!(
                    "Canceled before installing recommended packages: {}",
                    crate::formatters::comma_joined(names.iter().map(|name| Name::new(name)))
                ));
            }

            // Each entry's row prints as its event arrives, in columns fixed by
            // the announcement before the first.
            PackageEvent::DotfileEntriesPlanned { entries, .. } => {
                self.display.plan_entry_columns(&entries);
            }

            PackageEvent::DotfileDeployed {
                source,
                target,
                backup,
                ..
            } => {
                self.print_entry_row(EntryOutcome::Deployed, &source, &target, None);
                // Only when something was kept. Saying so on every deploy would
                // train the reader to skip the line that matters.
                if let Some(backup) = backup {
                    self.display.println(shown!(
                        "{}previous content copied to {}",
                        under_row(),
                        crate::display_manager::shorten_path(&backup)
                    ));
                }
            }

            PackageEvent::DotfileSkipped {
                source,
                target,
                reason,
                ..
            } => {
                self.print_entry_row(
                    EntryOutcome::from(&reason),
                    &source,
                    &target,
                    Some(received::skip_reason(&reason)),
                );
            }

            PackageEvent::DotfileConflict {
                source,
                target,
                detail,
                ..
            } => {
                // A blank line first, so the diff under the row does not read as
                // part of the row above.
                self.display.println(lit!(""));
                self.print_entry_row(EntryOutcome::Conflict, &source, &target, None);
                self.display.print_diff(Diff::new(&detail.to_string()));
            }

            // Drift is what `dotfiles drift` was asked to find: its answer.
            PackageEvent::DotfileDriftDetected {
                source,
                target,
                drift_type,
                ..
            } => {
                self.print_entry_row(
                    EntryOutcome::Drifted,
                    &source,
                    &target,
                    Some(Shown::from(Name::new(&drift_type.to_string()))),
                );
            }

            // A warning: the file is one the user may still want, and nothing
            // will manage it again unless they act.
            PackageEvent::DotfileOrphaned {
                operation_info,
                source,
                target,
                package,
            } => {
                // An orphan is part of what `dotfiles drift` is asked to find, so
                // there it is the answer. Elsewhere it is a warning about the run.
                let answer = matches!(operation_info.operation_type, OperationType::DotfileDrift);
                let channel = if answer {
                    Channel::Stdout
                } else {
                    Channel::Stderr
                };
                let source = source_paths::label(&self.display, channel, &source);
                let short_target = crate::display_manager::shorten_path(&target);
                let by = package
                    .map(|package| shown!(" by package '{}'", Name::new(&package)))
                    .unwrap_or_default();
                let line = source.noted(shown!(
                    "  Orphaned {}: deployed from {}{}, and no entry deploys to it now. selfie \
                     leaves it in place; check whether you still need it",
                    short_target,
                    source.text(),
                    by
                ));
                if answer {
                    self.display.print_result(Outcome::Found, line);
                } else {
                    self.display.print_warning(line);
                }
            }

            // One message: the printer keeps the note's line breaks and indents
            // the lines after the first under it.
            PackageEvent::PostInstallNote { note, .. } => {
                self.display.println(lit!(""));
                self.display
                    .print_info(shown!("📋 {}", received::message(note)));
            }

            PackageEvent::DotfileCleanupInfo {
                package_name,
                dotfile_targets,
                ..
            } => {
                self.display.println(lit!(""));
                self.display.print_info(shown!(
                    "Package '{}' has deployed dotfiles:",
                    Name::new(&package_name)
                ));
                for target in &dotfile_targets {
                    self.display.print_info(shown!(
                        "  - {}",
                        crate::display_manager::shorten_path(target)
                    ));
                }
                self.display.print_info(lit!(
                    "  These files were NOT removed. Delete them manually if no longer needed."
                ));
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

            // The entry's deployed row names it, so announcing it first would print
            // the entry twice.
            PackageEvent::DotfileDeploying { .. } => {}
        }
    }

    /// Print the row of an entry `outcome` describes, its source's heading
    /// first when the base directory changed.
    fn print_entry_row(
        &self,
        outcome: EntryOutcome,
        source: &DotfileSource,
        target: &str,
        detail: Option<Shown>,
    ) {
        let source = source_paths::label(&self.display, Channel::Stdout, source);
        self.display.print_entry_row(&EntryRow {
            outcome,
            source,
            target: crate::display_manager::shorten_path(target),
            detail,
        });
    }
}

/// Returns where the escape sequence that `buffer` ends inside of begins, if
/// it ends inside one.
// A control sequence (`ESC [`) ends at its final byte, an operating-system
// command or device control string (`ESC ]`, `ESC P`) at BEL or `ESC \`, and
// any other escape at the byte after ESC. `ESC \` finishes a string because
// its ESC is the last one, followed by a byte. A control sequence unfinished
// past 32 bytes, or a string past 512, is not a real one, so the cut goes where
// it falls. 512 covers a hyperlink's URL and keeps the carry small beside the
// cap.
fn unfinished_escape(buffer: &str) -> Option<usize> {
    let esc = buffer.rfind('\x1b')?;
    let rest = &buffer.as_bytes()[esc + 1..];
    let (finished, limit) = match rest.split_first() {
        None => (false, 1),
        Some((b'[', params)) => (params.iter().any(|b| (0x40..=0x7e).contains(b)), 32),
        Some((b']' | b'P', body)) => (body.contains(&0x07), 512),
        Some(_) => (true, 0),
    };
    (!finished && rest.len() < limit).then_some(esc)
}

/// Names the pipe a piece of a command's output came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pipe {
    Stdout,
    Stderr,
}

impl Pipe {
    fn of(output: &ConsoleOutput) -> Self {
        match output {
            ConsoleOutput::Stdout(_) => Pipe::Stdout,
            ConsoleOutput::Stderr(_) => Pipe::Stderr,
        }
    }
}

/// Sets how many bytes an unfinished line may hold before it prints as a line
/// anyway.
const LINE_CAP: usize = 4096;

/// Holds what one read of a command's output completed.
#[derive(Debug)]
enum Piece {
    /// A line, ended by a newline or by the cap.
    Line(String),
    /// The unfinished line, for the spinner to show as a terminal would. It is
    /// never printed.
    // The next read may redraw it, so a printed frame would show text the
    // terminal no longer has.
    Frame(String),
}

/// Holds the unfinished last line of each open step's pipes.
#[derive(Debug, Default)]
struct PartialLines {
    open: Vec<(StepId, Pipe, String)>,
}

// Each buffer belongs to one step's pipe, so output another step or pipe writes
// between the two halves of a line joins neither. A `Vec` keeps the order the
// pipes first wrote in, so a run that ends prints its fragments in a fixed order.
impl PartialLines {
    /// Add `text`, the next read from `step`'s `pipe`, and return what it
    /// completed: its lines, then the unfinished line as a frame if the read
    /// added to it. An unfinished line that reaches [`LINE_CAP`] bytes is
    /// returned as a line instead, unless it holds a carriage return: then the
    /// frames already redrawn are dropped, and only a frame that alone reaches
    /// the cap is returned as a line.
    fn push(&mut self, step: StepId, pipe: Pipe, text: &str) -> Vec<Piece> {
        let index = match self
            .open
            .iter()
            .position(|(s, p, _)| *s == step && *p == pipe)
        {
            Some(index) => index,
            None => {
                self.open.push((step, pipe, String::new()));
                self.open.len() - 1
            }
        };
        let buffer = &mut self.open[index].2;
        buffer.push_str(text);
        // Lines split as `str::lines` splits a failure's whole stderr, so a line
        // the step's tail showed matches its copy there. A line keeps any
        // carriage returns, and `Output::line` keeps the text after the last.
        let mut pieces: Vec<Piece> = match buffer.rfind('\n') {
            Some(end) => {
                let lines = buffer[..end]
                    .lines()
                    .map(|line| Piece::Line(line.to_owned()))
                    .collect();
                buffer.drain(..=end);
                lines
            }
            None => Vec::new(),
        };
        // A progress bar redraws with `\r` and may end no line until it is
        // done. Once its frames reach the cap, those already redrawn are dropped,
        // keeping the frame being drawn, or the last complete one when the next
        // has not begun. The cap then prints a frame only when one frame alone
        // reaches it.
        if buffer.len() >= LINE_CAP
            && let Some(last) = buffer.trim_end_matches('\r').rfind('\r')
        {
            buffer.drain(..=last);
        }
        // A command that never ends a line still streams, and the buffer stays
        // bounded. The cut never lands inside a character, and an escape
        // sequence it would split moves on to the next line whole.
        if buffer.len() >= LINE_CAP {
            let cut = unfinished_escape(buffer).unwrap_or(buffer.len());
            let rest = buffer.split_off(cut);
            pieces.push(Piece::Line(std::mem::replace(buffer, rest)));
        }
        // The spinner shows what a terminal would show now: `Output::line` keeps
        // the frame being drawn, or the last complete one before the next
        // begins. A read that only ended lines leaves the spinner on its last.
        // A buffer holding only an escape the cap carried shows as blank, since
        // `Output::line` strips a cut sequence, and the spinner skips a blank.
        let added = &text[text.rfind('\n').map_or(0, |end| end + 1)..];
        if !added.is_empty() && !buffer.is_empty() {
            pieces.push(Piece::Frame(buffer.clone()));
        }
        pieces
    }

    /// Remove and return the unfinished lines of step `only`, or of every step
    /// when `only` is `None`.
    fn take(&mut self, only: Option<StepId>) -> Vec<(StepId, String)> {
        let (taken, kept) = std::mem::take(&mut self.open)
            .into_iter()
            .partition(|(step, _, _)| only.is_none_or(|only| *step == only));
        self.open = kept;
        taken
            .into_iter()
            .map(|(step, _, line)| (step, line))
            .collect()
    }
}

/// The line saying that `package_name` settled earlier in the install run, and
/// how.
fn settled_earlier_line(package_name: &str, settled: Settled) -> Shown {
    let name = Name::new(package_name);
    match settled {
        Settled::Installed => shown!("{}: installed earlier in this run", name),
        Settled::AlreadyInstalled => {
            shown!("{}: already installed, as found earlier in this run", name)
        }
        Settled::Unconfirmed => shown!(
            "{}: installed earlier in this run, unconfirmed: its check gave no answer",
            name
        ),
    }
}

/// The header line for an operation: what it is, on which package, in which
/// environment.
pub(crate) fn started_line(operation_info: &OperationInfo) -> Shown {
    let operation = Prose::new(&operation_info.operation_type.to_string().to_title_case());
    let names = received::Operation::from(operation_info);
    // An operation over every package has no package name to give.
    if operation_info.package_name.is_empty() {
        shown!("{} in environment '{}'", operation, names.environment)
    } else {
        shown!(
            "{} package '{}' in environment '{}'",
            operation,
            names.package,
            names.environment
        )
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

/// The warning lines for packages refused for `reason`: one line naming them
/// all, or under `verbose` one line per package.
pub(crate) fn refused_packages_lines(
    reason: &Message,
    packages: &[RefusedPackage],
    verbose: bool,
) -> Vec<Shown> {
    let reason = received::composed(reason);
    let one = |package: &RefusedPackage| {
        shown!(
            "Skipping package '{}': {}",
            Name::new(&package.name),
            reason
        )
    };
    match packages {
        [] => Vec::new(),
        [package] => vec![one(package)],
        _ if verbose => packages.iter().map(one).collect(),
        _ => vec![shown!(
            "Skipping {} packages ({}): {}",
            packages.len(),
            crate::formatters::comma_joined(packages.iter().map(|p| Name::new(&p.name))),
            reason
        )],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::stream;
    use selfie::package::event::{ConsoleOutput, StepEnding, StepId};

    // A cancel outranks a usage error, a usage error a failure, a failure a
    // finding, and a finding a clean result. Each pair is compared in both
    // orders, so a tie in the ranking fails one of them whichever way `max`
    // breaks it.
    #[test]
    fn exits_rank_canceled_usage_failed_found_clean() {
        let ranked = [
            Exit::Canceled,
            Exit::Usage,
            Exit::Failed,
            Exit::Found,
            Exit::Clean,
        ];
        for (i, higher) in ranked.iter().enumerate() {
            for lower in &ranked[i + 1..] {
                assert_eq!(higher.max(lower), higher, "{higher:?} over {lower:?}");
                assert_eq!(lower.max(higher), higher, "{higher:?} over {lower:?}");
            }
        }
    }

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
        assert_eq!(result.exit, Exit::Failed);
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

        assert_eq!(result.exit, Exit::Failed);
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
                unjudged_count: 0,
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
            (drift_checked(0, 0), Exit::Clean),
            (drift_checked(1, 0), Exit::Found),
            (drift_checked(1, 1), Exit::Failed),
        ] {
            let processor = EventProcessor::new(DisplayManager::new(false));
            let result = processor
                .process_events(Box::pin(stream::iter(vec![event])), |_event| true)
                .await;
            assert_eq!(result.exit, expected);
        }
    }

    // A cancellation a handler claims still exits 130, and nothing after it is
    // read.
    #[tokio::test]
    async fn a_claimed_cancellation_exits_130_and_stops() {
        let events = vec![
            PackageEvent::Canceled {
                operation_info: make_operation_info("canceled"),
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

        assert_eq!(result.exit, Exit::Canceled);
        assert_eq!(completions_seen, 0);
    }

    // Control: a custom handler claiming the completion still counts as a result,
    // so the check above cannot be satisfied by failing every claimed stream.
    #[tokio::test]
    async fn a_claimed_completion_is_still_a_result() {
        use selfie::package::event::{OperationResult, OperationSuccess};

        let events = vec![PackageEvent::Completed {
            operation_info: make_operation_info("claimed"),
            result: OperationResult::Success(OperationSuccess::Generic("done")),
        }];

        let processor = EventProcessor::new(DisplayManager::new(false));
        let result = processor
            .process_events(Box::pin(stream::iter(events)), |_event| true)
            .await;

        assert_eq!(result.exit, Exit::Clean);
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

        assert_eq!(result.exit, Exit::Failed);
        // Handler should not be called for empty stream
        assert!(!handler_called);
    }

    fn step(kind: StepKind, message: &str) -> PackageEvent {
        PackageEvent::Progress {
            operation_info: make_operation_info("bat"),
            step: 1,
            total_steps: 2,
            percent_complete: 0.5,
            kind,
            message: message.to_string(),
        }
    }

    // What the shared handler printed for `events` on `display`. A stream with no
    // completion also gets "ended without reporting a result", which is left out
    // here: these streams are cut short on purpose.
    async fn printed_for(
        display: DisplayManager,
        events: Vec<PackageEvent>,
    ) -> Vec<(crate::display_manager::Channel, String)> {
        let processor = EventProcessor::new(display.clone());
        processor
            .process_events(Box::pin(stream::iter(events)), |_event| false)
            .await;
        display
            .printed()
            .into_iter()
            .filter(|(_, line)| line != "The operation ended without reporting a result")
            .collect()
    }

    fn verbose() -> DisplayManager {
        DisplayManager::new(false).with_verbosity(crate::display_manager::Verbosity::Verbose)
    }

    #[tokio::test]
    async fn a_waiting_step_prints_at_normal_verbosity_on_stderr() {
        use crate::display_manager::Channel;

        let printed = printed_for(
            DisplayManager::new(false),
            vec![step(
                StepKind::Waiting(StepId::from_raw(1)),
                "Running the check command for bat",
            )],
        )
        .await;
        assert_eq!(
            printed,
            vec![(
                Channel::Stderr,
                "Running the check command for bat...".to_string()
            )]
        );
    }

    // A step's message is escaped as a name, on both routes: a waiting step's
    // progress line and a local step's status line. One row per kind of
    // source: a target path, a package name, a count, a remote's name, and the
    // local install and commit steps.
    #[tokio::test]
    async fn a_progress_message_shows_on_one_line_escaped() {
        let rows = [
            (
                "Running the commands that produce ~/.a\u{A0}b\nc",
                r"Running the commands that produce ~/.a\u{a0}b\nc",
            ),
            (
                "Running the install command for z\u{200B}w",
                r"Running the install command for z\u{200b}w",
            ),
            ("Checking 3 packages", "Checking 3 packages"),
            (
                "Pushing to remote 'or\u{2800}igin'",
                r"Pushing to remote 'or\u{2800}igin'",
            ),
            (
                "Checking whether t\u{200B}x is already installed",
                r"Checking whether t\u{200b}x is already installed",
            ),
            ("Committing: fix\nbody", r"Committing: fix\nbody"),
            ("Committing: fix\r\n", "Committing: fix"),
        ];
        for (message, shown) in rows {
            let waiting = printed_for(
                verbose(),
                vec![step(StepKind::Waiting(StepId::from_raw(1)), message)],
            )
            .await;
            let local = printed_for(verbose(), vec![step(StepKind::Local, message)]).await;

            let lines = |printed: &[(crate::display_manager::Channel, String)]| -> Vec<String> {
                printed
                    .iter()
                    .map(|(_, line)| line.trim().to_string())
                    .collect()
            };
            assert_eq!(lines(&waiting), [format!("{shown}...")], "{message:?}");
            assert_eq!(lines(&local), [shown.to_string()], "{message:?}");
        }
    }

    #[tokio::test]
    async fn a_local_step_is_hidden_unless_verbose() {
        use crate::display_manager::Channel;

        let normal = printed_for(
            DisplayManager::new(false),
            vec![step(StepKind::Local, "Loading packages")],
        )
        .await;
        assert!(normal.is_empty(), "{normal:?}");

        let shown = printed_for(verbose(), vec![step(StepKind::Local, "Loading packages")]).await;
        assert_eq!(
            shown,
            vec![(Channel::Stderr, "Loading packages".to_string())]
        );
    }

    #[tokio::test]
    async fn the_header_prints_only_when_verbose_on_stderr() {
        use crate::display_manager::Channel;

        let started = || PackageEvent::Started {
            operation_info: make_operation_info("bat"),
        };
        let normal = printed_for(DisplayManager::new(false), vec![started()]).await;
        assert!(normal.is_empty(), "{normal:?}");

        let shown = printed_for(verbose(), vec![started()]).await;
        assert_eq!(
            shown,
            vec![(
                Channel::Stderr,
                "Package check package 'bat' in environment 'test'".to_string()
            )]
        );
    }

    fn refused(names: &[&str]) -> PackageEvent {
        PackageEvent::PackagesRefused {
            operation_info: make_operation_info(""),
            kind: selfie::package::event::RefusalKind::UnknownTopLevelKeys,
            reason: test_common::message("unknown field 'version'"),
            packages: names
                .iter()
                .map(|name| RefusedPackage {
                    name: (*name).to_string(),
                    paths: vec![std::path::PathBuf::from(format!("/p/{name}.yml"))],
                })
                .collect(),
        }
    }

    // Packages refused for one reason are one line on stderr, a single package
    // is named in a sentence of its own, `--verbose` gives each its own line, and
    // an empty group prints nothing.
    #[tokio::test]
    async fn refused_packages_print_one_line_unless_verbose() {
        use crate::display_manager::Channel;

        let lines = |printed: Vec<(Channel, String)>| -> Vec<String> {
            assert!(
                printed
                    .iter()
                    .all(|(channel, _)| *channel == Channel::Stderr),
                "{printed:?}"
            );
            printed.into_iter().map(|(_, line)| line).collect()
        };

        let grouped =
            lines(printed_for(DisplayManager::new(false), vec![refused(&["a", "b", "c"])]).await);
        assert_eq!(grouped.len(), 1, "{grouped:?}");
        assert!(
            grouped[0].contains("Skipping 3 packages (a, b, c): unknown field 'version'"),
            "{grouped:?}"
        );

        let single = lines(printed_for(DisplayManager::new(false), vec![refused(&["a"])]).await);
        assert_eq!(single.len(), 1, "{single:?}");
        assert!(
            single[0].contains("Skipping package 'a': unknown field 'version'"),
            "{single:?}"
        );

        let empty = lines(printed_for(DisplayManager::new(false), vec![refused(&[])]).await);
        assert!(empty.is_empty(), "{empty:?}");

        let each = lines(printed_for(verbose(), vec![refused(&["a", "b", "c"])]).await);
        let each: Vec<&String> = each.iter().filter(|l| l.contains("Skipping")).collect();
        assert_eq!(each.len(), 3, "{each:?}");
        for (line, name) in each.iter().zip(["a", "b", "c"]) {
            assert!(
                line.contains(&format!(
                    "Skipping package '{name}': unknown field 'version'"
                )),
                "{each:?}"
            );
        }
    }

    // The recommends a cancel left untried are named together, on stderr.
    #[tokio::test]
    async fn untried_recommends_are_named_on_stderr() {
        use crate::display_manager::Channel;

        let printed = printed_for(
            DisplayManager::new(false),
            vec![PackageEvent::RecommendsUntried {
                operation_info: make_operation_info("root"),
                names: vec!["r2".to_string(), "r3".to_string()],
            }],
        )
        .await;

        assert_eq!(printed.len(), 1, "{printed:?}");
        assert_eq!(printed[0].0, Channel::Stderr);
        assert!(
            printed[0]
                .1
                .contains("Canceled before installing recommended packages: r2, r3"),
            "{printed:?}"
        );
    }

    // A recommend's verdict line is marked once, by the display: the message
    // the display marks carries no mark of its own.
    #[tokio::test]
    async fn a_recommend_verdict_carries_one_mark() {
        use crate::display_manager::Channel;

        let info = || make_operation_info("root");
        for (event, channel, expected) in [
            (
                PackageEvent::RecommendSucceeded {
                    operation_info: info(),
                    recommend_name: "rec".to_string(),
                },
                Channel::Stdout,
                "rec",
            ),
            (
                PackageEvent::RecommendFailed {
                    operation_info: info(),
                    recommend_name: "rec".to_string(),
                    error: Box::new(selfie::package::event::OperationFailure::Generic(
                        "no such formula",
                    )),
                },
                Channel::Stderr,
                "rec failed: no such formula",
            ),
        ] {
            let printed = printed_for(DisplayManager::new(false), vec![event]).await;

            assert_eq!(printed.len(), 1, "{printed:?}");
            assert_eq!(printed[0].0, channel, "{printed:?}");
            assert_eq!(printed[0].1, expected, "{printed:?}");
            assert!(
                !printed[0].1.contains('✓') && !printed[0].1.contains('⚠'),
                "{printed:?}"
            );
        }
    }

    // A package settled earlier in the run gets one line at default verbosity,
    // on stderr, saying how it settled. None claims success, and an
    // unconfirmed install never reads as a confirmed one.
    #[tokio::test]
    async fn a_settled_earlier_line_names_the_package_on_stderr() {
        use crate::display_manager::Channel;
        use selfie::package::event::Settled;

        for (settled, expected) in [
            (Settled::Installed, "dep: installed earlier in this run"),
            (
                Settled::AlreadyInstalled,
                "dep: already installed, as found earlier in this run",
            ),
            (
                Settled::Unconfirmed,
                "dep: installed earlier in this run, unconfirmed: its check gave no answer",
            ),
        ] {
            let printed = printed_for(
                DisplayManager::new(false),
                vec![PackageEvent::SettledEarlier {
                    operation_info: make_operation_info("root"),
                    package_name: "dep".to_string(),
                    recommend_name: "rec".to_string(),
                    settled,
                }],
            )
            .await;

            assert_eq!(printed.len(), 1, "{settled:?}: {printed:?}");
            assert_eq!(printed[0].0, Channel::Stderr, "{settled:?}");
            assert_eq!(printed[0].1.trim(), expected, "{settled:?}");
            assert!(!printed[0].1.contains("successfully"), "{printed:?}");
        }
    }

    // A name the CLI puts in its own sentence is escaped as a name, so an
    // invisible character or a line break in a recommend or a settled package
    // shows as an escape and the line stays one line.
    #[tokio::test]
    async fn names_in_recommend_and_settled_lines_are_escaped_as_names() {
        use selfie::package::event::Settled;

        let events = vec![
            PackageEvent::RecommendSucceeded {
                operation_info: make_operation_info("root"),
                recommend_name: "r\u{200B}a\nb".to_string(),
            },
            PackageEvent::RecommendFailed {
                operation_info: make_operation_info("root"),
                recommend_name: "r\u{200B}a\nb".to_string(),
                error: Box::new(selfie::package::event::OperationFailure::Generic(
                    "no such formula",
                )),
            },
            PackageEvent::SettledEarlier {
                operation_info: make_operation_info("root"),
                package_name: "d\u{200B}e\nf".to_string(),
                recommend_name: "rec".to_string(),
                settled: Settled::Installed,
            },
        ];
        let printed = printed_for(DisplayManager::new(false), events).await;

        let lines: Vec<&str> = printed.iter().map(|(_, line)| line.trim()).collect();
        assert_eq!(
            lines,
            [
                r"r\u{200b}a\nb",
                r"r\u{200b}a\nb failed: no such formula",
                r"d\u{200b}e\nf: installed earlier in this run",
            ],
            "{printed:?}"
        );
    }

    // A parse failure's location line names the file as a path, so an
    // invisible character in it shows as its escape.
    #[tokio::test]
    async fn a_parse_failures_location_names_the_file_escaped() {
        use selfie::package::event::OperationFailure;
        use selfie::package::port::{PackageError, PackageParseError, PackageParseKind};

        let source = selfie::yaml::parse::<selfie::package::Package>("name: [\n")
            .expect_err("the fixture must not parse");
        let failed = PackageEvent::Completed {
            operation_info: make_operation_info("broken"),
            result: OperationResult::Failure(OperationFailure::Package(PackageError::ParseError {
                name: "broken".to_string(),
                packages_path: "/p".into(),
                failed_file: "/p/a\u{200B}b.yml".into(),
                cause: PackageParseError::new(
                    "/p/a\u{200B}b.yml",
                    PackageParseKind::Yaml { cause: source },
                ),
            })),
        };

        let printed = printed_for(DisplayManager::new(false), vec![failed]).await;

        let location = printed
            .iter()
            .map(|(_, line)| line.as_str())
            .find(|line| line.starts_with("/p/a"))
            .unwrap_or_else(|| panic!("no location line in {printed:?}"));
        assert!(location.starts_with(r"/p/a\u{200b}b.yml:"), "{location}");
    }

    // The error summary collects the line printed for a failure, so a refusal
    // the CLI composes names its spec there as it does on the error line.
    #[tokio::test]
    async fn the_error_summary_collects_the_composed_refusal() {
        use selfie::package::event::{OperationFailure, Uncreatable};

        let events: Vec<PackageEvent> = ["a\u{200B}b", "c\u{200B}d"]
            .into_iter()
            .map(|name| PackageEvent::Completed {
                operation_info: make_operation_info(name),
                result: OperationResult::Failure(OperationFailure::Uncreatable {
                    package_name: name.to_string(),
                    reason: Uncreatable::InvalidName,
                }),
            })
            .collect();
        let display = DisplayManager::new(false);
        let display_clone = display.clone();
        EventProcessor::new(display)
            .process_events(Box::pin(stream::iter(events)), |_event| false)
            .await;

        let messages: Vec<String> = display_clone
            .collected_errors()
            .iter()
            .map(|error| error.message.to_string())
            .collect();
        assert!(
            messages[0].starts_with(r"Refusing to create 'a\u{200b}b'"),
            "{messages:?}"
        );
        assert!(
            messages.iter().all(|m| !m.contains('\u{200B}')),
            "{messages:?}"
        );
    }

    // The error summary names each package as a name, so one holding an
    // invisible character shows it as an escape.
    #[tokio::test]
    async fn an_error_summary_names_each_package_escaped() {
        use selfie::package::event::{OperationFailure, OperationResult};

        let events: Vec<PackageEvent> = ["a\u{200B}b", "c\nd"]
            .into_iter()
            .map(|name| PackageEvent::Completed {
                operation_info: make_operation_info(name),
                result: OperationResult::Failure(OperationFailure::Generic("failed")),
            })
            .collect();
        let display = DisplayManager::new(false);
        let display_clone = display.clone();
        EventProcessor::new(display)
            .process_events(Box::pin(stream::iter(events)), |_event| false)
            .await;

        let names: Vec<String> = display_clone
            .collected_errors()
            .iter()
            .map(|error| error.package_name.to_string())
            .collect();
        assert_eq!(names, [r"a\u{200b}b", r"c\nd"]);
    }

    #[test]
    fn the_header_names_the_package_only_when_there_is_one() {
        assert_eq!(
            started_line(&make_operation_info("")),
            "Package check in environment 'test'"
        );
    }

    // A command's own output is hidden at default verbosity without a terminal,
    // and never printed on stdout.
    #[tokio::test]
    async fn command_output_is_hidden_at_normal_verbosity() {
        let info = PackageEvent::Info {
            operation_info: make_operation_info("bat"),
            step: StepId::from_raw(1),
            output: ConsoleOutput::Stdout("==> Pouring bat".to_string()),
        };
        let printed = printed_for(DisplayManager::new(false), vec![info]).await;
        assert!(printed.is_empty(), "{printed:?}");
    }

    fn info(id: u64, output: ConsoleOutput) -> PackageEvent {
        PackageEvent::Info {
            operation_info: make_operation_info("bat"),
            step: StepId::from_raw(id),
            output,
        }
    }

    fn out(text: &str) -> ConsoleOutput {
        ConsoleOutput::Stdout(text.to_string())
    }

    fn err(text: &str) -> ConsoleOutput {
        ConsoleOutput::Stderr(text.to_string())
    }

    // Returns the waiting step `id`, which runs `package`'s install command.
    fn installing(id: u64, package: &str) -> PackageEvent {
        step(
            StepKind::Waiting(StepId::from_raw(id)),
            &format!("Running the install command for {package}"),
        )
    }

    // Returns `line` as `--verbose` prints it without a terminal, labeled with
    // `package`'s install step.
    fn labeled(package: &str, line: &str) -> String {
        format!("  Running the install command for {package} │ {line}")
    }

    // Returns the command output lines in `printed`, which are the labeled ones.
    fn output_lines(printed: &[(crate::display_manager::Channel, String)]) -> Vec<String> {
        printed
            .iter()
            .filter(|(_, line)| line.contains('│'))
            .map(|(_, line)| line.clone())
            .collect()
    }

    // Returns a processor on `display` whose step 1 runs bat's install command.
    fn running_bat(display: &DisplayManager) -> EventProcessor {
        let mut processor = EventProcessor::new(display.clone());
        processor.handle_event(installing(1, "bat"));
        processor
    }

    // Returns the text of each line in `printed`.
    fn texts(printed: &[(crate::display_manager::Channel, String)]) -> Vec<&str> {
        printed.iter().map(|(_, line)| line.as_str()).collect()
    }

    // Returns where the first line containing `needle` is in `lines`.
    fn index_of(lines: &[&str], needle: &str) -> usize {
        lines
            .iter()
            .position(|line| line.contains(needle))
            .unwrap_or_else(|| panic!("no line containing {needle:?} in {lines:?}"))
    }

    fn failed_with_stderr(stderr: &[u8]) -> PackageEvent {
        use selfie::package::event::{CommandFailure, OperationFailure};

        PackageEvent::Completed {
            operation_info: make_operation_info("bat"),
            result: OperationResult::Failure(OperationFailure::CommandError(
                CommandFailure::ExecutionFailed {
                    command: "brew install bat".to_string(),
                    exit_code: Some(1),
                    stdout: selfie::commands::BoundedText::bound(b""),
                    stderr: selfie::commands::BoundedText::bound(stderr),
                },
            )),
        }
    }

    // A line the command wrote in two reads prints once, whole.
    #[tokio::test]
    async fn a_line_split_across_two_reads_prints_once() {
        let printed = printed_for(
            verbose(),
            vec![
                installing(1, "bat"),
                info(1, out("Downloading foo")),
                info(1, out(" done\n")),
            ],
        )
        .await;
        assert_eq!(
            output_lines(&printed),
            [labeled("bat", "Downloading foo done")]
        );
    }

    // A read holding several lines prints each as it arrives.
    #[tokio::test]
    async fn several_lines_in_one_read_print_separately() {
        let display = verbose();
        let mut processor = running_bat(&display);
        processor.handle_event(info(1, out("a\nb\n")));
        assert_eq!(
            output_lines(&display.printed()),
            [labeled("bat", "a"), labeled("bat", "b")]
        );
    }

    // A last line with no newline prints when its step ends, still labeled
    // with the step.
    #[tokio::test]
    async fn a_final_fragment_prints_when_its_step_ends() {
        let printed = printed_for(
            verbose(),
            vec![
                installing(1, "bat"),
                info(1, out("x\n")),
                info(1, out("tail")),
                ended(1, StepEnding::Succeeded),
            ],
        )
        .await;
        assert_eq!(
            output_lines(&printed),
            [labeled("bat", "x"), labeled("bat", "tail")]
        );
    }

    // A failed step's tail holds its whole last line, even one with no newline
    // that reached stderr in two reads. The tail prints when the step ends,
    // before the failure, and the failure's stderr leaves it out.
    #[tokio::test]
    async fn a_failed_steps_tail_holds_the_whole_last_line() {
        let display = DisplayManager::new(false).drawing_as_a_terminal();
        let printed = printed_for(
            display,
            vec![
                installing(1, "bat"),
                info(1, err("Error: no ")),
                info(1, err("bottle")),
                ended(1, StepEnding::Failed),
                failed_with_stderr(b"Error: no bottle"),
            ],
        )
        .await;
        let lines = texts(&printed);
        let bottle: Vec<usize> = (0..lines.len())
            .filter(|&i| lines[i].contains("bottle"))
            .collect();
        assert_eq!(bottle.len(), 1, "{lines:?}");
        assert_eq!(lines[bottle[0]].trim(), "Error: no bottle", "{lines:?}");
        assert!(
            bottle[0] < index_of(&lines, "brew install bat"),
            "{lines:?}"
        );
    }

    // A fragment still open when the operation completes prints before the
    // result, labeled with its step.
    #[tokio::test]
    async fn a_fragment_open_at_the_completion_prints_before_it() {
        let printed = printed_for(
            verbose(),
            vec![installing(1, "bat"), info(1, out("frag")), checked()],
        )
        .await;
        assert_eq!(output_lines(&printed), [labeled("bat", "frag")]);
        let lines = texts(&printed);
        assert!(
            index_of(&lines, "frag") < index_of(&lines, "check completed"),
            "{lines:?}"
        );
    }

    // A fragment still open when the operation is canceled prints, labeled
    // with its step.
    #[tokio::test]
    async fn a_fragment_open_at_cancel_prints() {
        let printed = printed_for(
            verbose(),
            vec![
                installing(1, "bat"),
                info(1, out("frag")),
                PackageEvent::Canceled {
                    operation_info: make_operation_info("bat"),
                    reason: "Ctrl+C".to_string(),
                },
            ],
        )
        .await;
        assert_eq!(output_lines(&printed), [labeled("bat", "frag")]);
    }

    // A fragment still open when the stream ends without a result prints,
    // labeled with its step.
    #[tokio::test]
    async fn a_fragment_open_when_the_stream_ends_prints() {
        let printed =
            printed_for(verbose(), vec![installing(1, "bat"), info(1, out("frag"))]).await;
        assert_eq!(output_lines(&printed), [labeled("bat", "frag")]);
    }

    // A line another step writes between the halves of one step's line joins
    // neither, and each prints under its own step.
    #[tokio::test]
    async fn interleaved_steps_keep_their_own_lines() {
        let printed = printed_for(
            verbose(),
            vec![
                installing(1, "bat"),
                installing(2, "fd"),
                info(1, out("Down")),
                info(2, out("other\n")),
                info(1, out("load\n")),
            ],
        )
        .await;
        assert_eq!(
            output_lines(&printed),
            [labeled("fd", "other"), labeled("bat", "Download")]
        );
    }

    // A stderr line written between the halves of a stdout line joins neither.
    #[tokio::test]
    async fn stdout_and_stderr_of_one_step_keep_their_own_lines() {
        let printed = printed_for(
            verbose(),
            vec![
                installing(1, "bat"),
                info(1, out("a")),
                info(1, err("err\n")),
                info(1, out("b\n")),
            ],
        )
        .await;
        assert_eq!(
            output_lines(&printed),
            [labeled("bat", "err"), labeled("bat", "ab")]
        );
    }

    // A progress bar that starts each frame with a carriage return shows the
    // frame being drawn in its step's spinner, and prints nothing.
    #[tokio::test]
    async fn a_bar_that_leads_with_a_carriage_return_shows_in_the_spinner() {
        let display = DisplayManager::new(false).drawing_as_a_terminal();
        let mut processor = running_bat(&display);
        processor.handle_event(info(1, out("\r 10%")));
        assert_eq!(display.waiting_messages(), [" 10%"]);
        processor.handle_event(info(1, out("\r 55%\r 70%")));
        assert_eq!(display.waiting_messages(), [" 70%"]);
        assert!(
            !display.printed().iter().any(|(_, line)| line.contains('%')),
            "{:?}",
            display.printed()
        );
    }

    // A progress bar that ends each frame with a carriage return shows each
    // frame in its step's spinner as it arrives.
    #[tokio::test]
    async fn a_bar_that_ends_with_a_carriage_return_shows_in_the_spinner() {
        let display = DisplayManager::new(false).drawing_as_a_terminal();
        let mut processor = running_bat(&display);
        processor.handle_event(info(1, out(" 10%\r")));
        assert_eq!(display.waiting_messages(), [" 10%"]);
        processor.handle_event(info(1, out(" 55%\r")));
        assert_eq!(display.waiting_messages(), [" 55%"]);
    }

    // Under `--verbose` a progress bar prints one line, its last frame, however
    // many frames it drew.
    #[tokio::test]
    async fn a_bar_under_verbose_prints_one_line() {
        let mut events = vec![installing(1, "bat")];
        events.extend((0..100).map(|percent| info(1, out(&format!("\r{percent}%")))));
        events.push(info(1, out("\r100%\n")));
        let printed = printed_for(verbose(), events).await;
        assert_eq!(output_lines(&printed), [labeled("bat", "100%")]);
    }

    // A failed bar's frames stay out of its step's tail, so its last frame
    // prints once, from the tail, and the failure's stderr repeats none of it.
    #[tokio::test]
    async fn a_failed_bar_prints_once() {
        let display = DisplayManager::new(false).drawing_as_a_terminal();
        let printed = printed_for(
            display,
            vec![
                installing(1, "bat"),
                info(1, err("\r10%")),
                info(1, err("\r55%")),
                info(1, err("\r100%\nboom\n")),
                ended(1, StepEnding::Failed),
                failed_with_stderr(b"\r10%\r55%\r100%\nboom\n"),
            ],
        )
        .await;
        let shown: Vec<&str> = texts(&printed)
            .into_iter()
            .map(str::trim)
            .filter(|line| line.contains('%') || line.contains("boom"))
            .collect();
        assert_eq!(shown, ["100%", "boom"], "{printed:?}");
    }

    // A fragment with no line end shows in its step's spinner while the
    // command works, and prints nothing until its line ends.
    #[tokio::test]
    async fn a_fragment_with_no_line_end_shows_in_the_spinner() {
        let display = DisplayManager::new(false).drawing_as_a_terminal();
        let mut processor = running_bat(&display);
        processor.handle_event(info(1, out("Compiling foo")));
        assert_eq!(display.waiting_messages(), ["Compiling foo"]);
        assert!(
            !display
                .printed()
                .iter()
                .any(|(_, line)| line.contains("Compiling")),
            "{:?}",
            display.printed()
        );
    }

    // Under `--verbose` a bar shows in the spinner after its step's subject,
    // prints nothing while it draws, and leaves the spinner on the subject once
    // its line prints.
    #[tokio::test]
    async fn a_bar_shows_after_the_subject_under_verbose() {
        let display = verbose().drawing_as_a_terminal();
        let mut processor = running_bat(&display);
        processor.handle_event(info(1, out("\r 10%")));
        assert_eq!(
            display.waiting_messages(),
            ["Running the install command for bat │  10%"]
        );
        assert!(
            !display.printed().iter().any(|(_, line)| line.contains('%')),
            "{:?}",
            display.printed()
        );
        processor.handle_event(info(1, out("\r100%\n")));
        assert_eq!(
            display.waiting_messages(),
            ["Running the install command for bat"]
        );
    }

    // Under `--verbose` two steps drawing at once each show their own frame
    // after their own subject.
    #[tokio::test]
    async fn concurrent_bars_keep_their_subjects_under_verbose() {
        let display = verbose().drawing_as_a_terminal();
        let mut processor = running_bat(&display);
        processor.handle_event(installing(2, "fd"));
        processor.handle_event(info(1, out("\r 10%")));
        processor.handle_event(info(2, out("\r 20%")));
        assert_eq!(
            display.waiting_messages(),
            [
                "Running the install command for bat │  10%",
                "Running the install command for fd │  20%",
            ]
        );
    }

    // A bar that redraws past the cap without ever ending a line prints only
    // its last frame, when its step ends, at either verbosity, and whether its
    // frames start or end with the carriage return.
    #[tokio::test]
    async fn a_long_bar_with_no_line_end_prints_only_its_last_frame() {
        let frames = || (0..300).map(|i| info(1, err(&format!("\r{i:04} frames drawn so far"))));
        assert!(300 * "\r0000 frames drawn so far".len() > LINE_CAP);

        let mut events = vec![installing(1, "bat")];
        events.extend(frames());
        events.push(ended(1, StepEnding::Succeeded));
        let printed = printed_for(verbose(), events).await;
        assert_eq!(
            output_lines(&printed),
            [labeled("bat", "0299 frames drawn so far")]
        );

        let mut events = vec![installing(1, "bat")];
        // One read, so the cap meets a buffer that ends at a carriage return.
        let trailing: String = (0..300)
            .map(|i| format!("{i:04} frames drawn so far\r"))
            .collect();
        events.push(info(1, err(&trailing)));
        events.push(ended(1, StepEnding::Succeeded));
        let printed = printed_for(verbose(), events).await;
        assert_eq!(
            output_lines(&printed),
            [labeled("bat", "0299 frames drawn so far")]
        );

        let mut events = vec![installing(1, "bat")];
        events.extend(frames());
        events.push(ended(1, StepEnding::Failed));
        let printed = printed_for(DisplayManager::new(false).drawing_as_a_terminal(), events).await;
        let drawn: Vec<&str> = texts(&printed)
            .into_iter()
            .filter(|line| line.contains("frames drawn"))
            .map(str::trim)
            .collect();
        assert_eq!(drawn, ["0299 frames drawn so far"], "{printed:?}");
    }

    // An escape sequence the cap would cut moves whole onto the next line, so
    // neither line shows a piece of it.
    #[tokio::test]
    async fn an_escape_sequence_cut_by_the_cap_stays_whole() {
        let long = "x".repeat(LINE_CAP - 3);
        let printed = printed_for(
            verbose(),
            vec![
                installing(1, "bat"),
                info(1, out(&format!("{long}\u{1b}[3"))),
                info(1, out("2mred\u{1b}[0m\n")),
            ],
        )
        .await;
        assert_eq!(
            output_lines(&printed),
            [labeled("bat", &long), labeled("bat", "red")]
        );
    }

    // A bar whose frames are too wide for two to fit under the cap still prints
    // only its last frame.
    #[tokio::test]
    async fn a_bar_of_wide_frames_prints_only_its_last_frame() {
        let mut events = vec![installing(1, "bat")];
        let frames: Vec<String> = ['a', 'b', 'c', 'd', 'e']
            .into_iter()
            .map(|c| c.to_string().repeat(2100))
            .collect();
        events.extend(
            frames
                .iter()
                .map(|frame| info(1, out(&format!("\r{frame}")))),
        );
        events.push(ended(1, StepEnding::Succeeded));
        let printed = printed_for(verbose(), events).await;
        assert_eq!(output_lines(&printed), [labeled("bat", &frames[4])]);
    }

    // A finished escape sequence before the cap is no reason to carry
    // anything: the line goes out whole.
    #[tokio::test]
    async fn a_finished_escape_at_the_cap_goes_out_with_its_line() {
        let long = format!("{}\u{1b}[31mred", "x".repeat(LINE_CAP - 8));
        let printed = printed_for(verbose(), vec![installing(1, "bat"), info(1, out(&long))]).await;
        let lines = output_lines(&printed);
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(lines[0].ends_with("xred"), "{lines:?}");
    }

    // Each kind of escape sequence counts as unfinished until its own end, and
    // only within its bound.
    #[test]
    fn unfinished_escape_finds_only_a_sequence_the_end_cuts() {
        let csi = |params: usize| format!("x\u{1b}[{}", "1".repeat(params));
        let osc = |body: usize| format!("x\u{1b}]{}", "a".repeat(body));
        let rows: [(String, Option<usize>); 10] = [
            ("x\u{1b}".to_string(), Some(1)),
            ("x\u{1b}[31mred".to_string(), None),
            ("x\u{1b}(".to_string(), None),
            (csi(30), Some(1)),
            (csi(31), None),
            (osc(510), Some(1)),
            (osc(511), None),
            ("x\u{1b}]8;;https://e.example\u{7}".to_string(), None),
            ("x\u{1b}]8;;https://e.example\u{1b}\\".to_string(), None),
            ("x\u{1b}Pq".to_string(), Some(1)),
        ];
        for (buffer, expected) in rows {
            assert_eq!(unfinished_escape(&buffer), expected, "{buffer:?}");
        }
    }

    // A bar whose frames total exactly the cap drops its redrawn frame rather
    // than printing a frame while it draws.
    #[tokio::test]
    async fn a_bar_at_exactly_the_cap_prints_nothing_while_it_draws() {
        let display = verbose();
        let mut processor = running_bat(&display);
        let frame = format!("\r{}", "a".repeat(LINE_CAP / 2 - 1));
        processor.handle_event(info(1, out(&frame)));
        processor.handle_event(info(1, out(&frame)));
        assert_eq!(2 * frame.len(), LINE_CAP);
        assert!(
            output_lines(&display.printed()).is_empty(),
            "{:?}",
            display.printed()
        );
    }

    // Output that never ends a line prints once it reaches the cap, while the
    // step is still running, and not a byte before.
    #[tokio::test]
    async fn a_flood_with_no_line_end_streams_at_the_cap() {
        for (bytes, expected) in [(LINE_CAP, 1), (LINE_CAP - 1, 0)] {
            let display = verbose();
            let mut processor = running_bat(&display);
            let flood = ".".repeat(bytes);
            for piece in flood.as_bytes().chunks(512) {
                let piece = std::str::from_utf8(piece).unwrap();
                processor.handle_event(info(1, out(piece)));
            }
            let printed = output_lines(&display.printed());
            assert_eq!(printed.len(), expected, "{bytes} bytes");
            if expected == 1 {
                assert_eq!(printed[0], labeled("bat", &flood));
            }
        }
    }

    // A failed command's bounded stderr prints with its error, at every
    // verbosity: while it ran, its output was hidden.
    #[tokio::test]
    async fn a_failed_command_shows_its_stderr_at_normal_verbosity() {
        use crate::display_manager::Channel;
        use selfie::package::event::{CommandFailure, OperationFailure};

        let failed = PackageEvent::Completed {
            operation_info: make_operation_info("bat"),
            result: OperationResult::Failure(OperationFailure::CommandError(
                CommandFailure::ExecutionFailed {
                    command: "brew install bat".to_string(),
                    exit_code: Some(1),
                    stdout: selfie::commands::BoundedText::bound(b"==> Pouring bat"),
                    stderr: selfie::commands::BoundedText::bound(b"Error: no bottle"),
                },
            )),
        };
        let printed = printed_for(DisplayManager::new(false), vec![failed]).await;
        assert!(
            printed.contains(&(Channel::Stderr, "Error: no bottle".to_string())),
            "{printed:?}"
        );
        // The stdout tail is for an adapter that did not stream the command's
        // output, and the terminal already showed it.
        assert!(
            !printed.iter().any(|(_, line)| line.contains("Pouring")),
            "{printed:?}"
        );
    }

    // An orphaned target deployed from a command that reads as another names
    // the command in its warning, and the escaped command follows on the
    // warning's next line.
    #[tokio::test]
    async fn an_orphan_from_a_command_that_reads_as_another_notes_the_command() {
        let orphaned = PackageEvent::DotfileOrphaned {
            operation_info: make_operation_info("bat"),
            source: DotfileSource::Command("echo a\u{200B}b".to_string()),
            target: "/x/.batrc".to_string(),
            package: None,
        };

        let printed = printed_for(DisplayManager::new(false), vec![orphaned]).await;

        let warning = printed
            .iter()
            .map(|(_, line)| line.as_str())
            .find(|line| line.contains("Orphaned"))
            .unwrap_or_else(|| panic!("{printed:?}"));
        let lines: Vec<&str> = warning.lines().collect();
        assert_eq!(lines.len(), 2, "{warning}");
        assert!(
            lines[0].contains("deployed from command: echo a\u{200B}b,"),
            "{warning}"
        );
        assert_eq!(
            lines[1].trim_start(),
            "Escaped: echo a\\u{200b}b",
            "{warning}"
        );
    }

    fn checked() -> PackageEvent {
        use selfie::package::event::{CheckVerdict, OperationSuccess};

        PackageEvent::Completed {
            operation_info: make_operation_info("bat"),
            result: OperationResult::Success(OperationSuccess::PackageChecked {
                package_name: "bat".to_string(),
                environment: "test".to_string(),
                verdict: CheckVerdict::Installed,
                steps_completed: (1, 1).into(),
            }),
        }
    }

    fn ended(id: u64, ending: StepEnding) -> PackageEvent {
        PackageEvent::StepEnded {
            operation_info: make_operation_info("bat"),
            step: StepId::from_raw(id),
            ending,
        }
    }

    // A step that ended collapses to its line when its end arrives, so the line
    // comes before the completion.
    #[tokio::test]
    async fn an_ended_step_collapses_before_the_completion_prints() {
        let display = DisplayManager::new(false).drawing_as_a_terminal();
        let events = vec![
            step(
                StepKind::Waiting(StepId::from_raw(1)),
                "Running the check command for bat",
            ),
            ended(1, StepEnding::Succeeded),
            checked(),
        ];
        let printed = printed_for(display.clone(), events).await;

        let lines: Vec<&str> = printed.iter().map(|(_, line)| line.as_str()).collect();
        assert!(
            lines
                .first()
                .is_some_and(|l| l.starts_with("✓ Running the check command for bat (")),
            "{lines:?}"
        );
        assert!(display.waiting_messages().is_empty());
    }

    // The completion says nothing about a step still open: it is cleared
    // without a line, since only the step's own end says how it went.
    #[tokio::test]
    async fn a_step_open_at_the_completion_is_cleared_without_a_line() {
        let display = DisplayManager::new(false).drawing_as_a_terminal();
        let events = vec![
            step(
                StepKind::Waiting(StepId::from_raw(1)),
                "Running the check command for bat",
            ),
            checked(),
        ];
        let printed = printed_for(display.clone(), events).await;

        assert!(
            !printed.iter().any(|(_, l)| l.contains("Running the check")),
            "{printed:?}"
        );
        assert!(display.waiting_messages().is_empty());
    }

    // A line naming a source waits for a block the conflict prompt holds, so it
    // cannot print between the prompt's heading and its line.
    #[tokio::test]
    async fn a_source_line_waits_for_the_prompts_block() {
        use selfie::package::event::{BaseKind, DotfileSource, RepoPath, SourceBase};

        let display = DisplayManager::new(false);
        let mut processor = EventProcessor::new(display.clone());
        let skipped = PackageEvent::DotfileSkipped {
            operation_info: make_operation_info("bat"),
            source: DotfileSource::File(RepoPath {
                base: Some(SourceBase {
                    kind: BaseKind::PackageDirectory,
                    directory: "/r/p".into(),
                }),
                path: "bat/config".into(),
            }),
            target: "/h/.config/bat/config".to_string(),
            reason: selfie::package::event::SkipReason::DryRun,
        };

        let held = display.hold_block();
        let printer = std::thread::spawn(move || processor.handle_event(skipped));
        std::thread::sleep(std::time::Duration::from_millis(50));
        assert!(display.printed().is_empty(), "{:?}", display.printed());
        drop(held);
        printer.join().unwrap();

        assert_eq!(display.printed().len(), 2, "{:?}", display.printed());
    }

    // A provider command that fails is never shown as done, even though apply
    // itself completes.
    #[tokio::test]
    async fn a_failed_provider_step_never_shows_done() {
        use selfie::package::event::OperationSuccess;

        let display = DisplayManager::new(false).drawing_as_a_terminal();
        let events = vec![
            step(
                StepKind::Waiting(StepId::from_raw(1)),
                "Running the commands that produce ~/.creds",
            ),
            ended(1, StepEnding::Failed),
            PackageEvent::Warning {
                operation_info: make_operation_info("bat"),
                message: "Failed to resolve '~/.creds': no session".to_string(),
            },
            PackageEvent::Completed {
                operation_info: make_operation_info("bat"),
                result: OperationResult::Success(OperationSuccess::DotfilesApplied {
                    deployed_count: 0,
                    skipped_count: 0,
                    conflict_count: 0,
                    refused_count: 1,
                    failed_count: 0,
                    orphan_count: 0,
                    environment: "test".to_string(),
                    steps_completed: (1, 1).into(),
                }),
            },
        ];
        let printed = printed_for(display, events).await;

        let lines: Vec<&str> = printed.iter().map(|(_, line)| line.as_str()).collect();
        assert!(
            lines
                .iter()
                .any(|l| l.starts_with("✗ Running the commands")),
            "{lines:?}"
        );
        assert!(!lines.iter().any(|l| l.starts_with('✓')), "{lines:?}");
    }

    // A refused or failed entry prints the library's sentence on stderr.
    #[tokio::test]
    async fn a_refused_or_failed_entry_prints_its_sentence_on_stderr() {
        use crate::display_manager::Channel;
        use selfie::package::event::{Condition, EntryOperation, Failure, Location, Refusal};

        let refused = PackageEvent::DotfileRefused {
            operation_info: make_operation_info("bat"),
            package: "bat".to_string(),
            spec_target: "~/.batrc".to_string(),
            refusal: Refusal {
                condition: Condition::Symlink,
                at: Location::Target,
                message: test_common::message("Skipping 'bat/config': a symlink is there"),
            },
        };
        let failed = PackageEvent::DotfileFailed {
            operation_info: make_operation_info("bat"),
            package: "bat".to_string(),
            spec_target: "~/.creds".to_string(),
            failure: Failure {
                operation: EntryOperation::Resolve,
                error: test_common::message("no session"),
                message: test_common::message("Failed to resolve '~/.creds': no session"),
            },
        };
        let printed = printed_for(DisplayManager::new(false), vec![refused, failed]).await;

        for sentence in [
            "Skipping 'bat/config': a symlink is there",
            "Failed to resolve '~/.creds': no session",
        ] {
            assert!(
                printed
                    .iter()
                    .any(|(stream, line)| *stream == Channel::Stderr && line.contains(sentence)),
                "{sentence}: {printed:?}"
            );
        }
    }

    // A summary is the answer at every outcome, so it goes to stdout; an error
    // is not, and stays on stderr.
    #[tokio::test]
    async fn a_found_summary_goes_to_stdout_and_a_failure_to_stderr() {
        use crate::display_manager::Channel;
        use selfie::package::event::{CheckVerdict, OperationFailure, OperationSuccess};

        let found = PackageEvent::Completed {
            operation_info: make_operation_info("bat"),
            result: OperationResult::Success(OperationSuccess::PackageChecked {
                package_name: "bat".to_string(),
                environment: "test".to_string(),
                verdict: CheckVerdict::NotInstalled {
                    command: "false".to_string(),
                    exit_code: Some(1),
                    stderr: selfie::commands::BoundedText::bound(b""),
                },
                steps_completed: (1, 1).into(),
            }),
        };
        let printed = printed_for(DisplayManager::new(false), vec![found]).await;
        assert!(
            printed
                .iter()
                .any(|(stream, line)| *stream == Channel::Stdout && line.contains("'bat'")),
            "{printed:?}"
        );
        assert!(
            printed.iter().all(|(stream, _)| *stream == Channel::Stdout),
            "{printed:?}"
        );

        let failed = PackageEvent::Completed {
            operation_info: make_operation_info("bat"),
            result: OperationResult::Failure(OperationFailure::Generic("broken")),
        };
        let printed = printed_for(DisplayManager::new(false), vec![failed]).await;
        assert_eq!(printed, vec![(Channel::Stderr, "broken".to_string())]);
    }

    // A spec refused as invalid is an error, not an answer: the summary and every
    // issue go to stderr, and nothing goes to stdout.
    #[tokio::test]
    async fn invalid_spec_prints_each_issue_on_stderr() {
        use crate::display_manager::Channel;
        use selfie::package::event::{OperationFailure, ValidationIssueData};
        use selfie::validation::ValidationLevel;

        let issue = |level, field: &str| ValidationIssueData {
            category: selfie::validation::ValidationErrorCategory::CommandSyntax,
            field: field.to_string(),
            message: "Unmatched double quote in command".into(),
            level,
            suggestion: Some("Add a closing double quote.".to_string()),
            location: None,
        };
        let failed = PackageEvent::Completed {
            operation_info: make_operation_info("tool"),
            result: OperationResult::Failure(OperationFailure::InvalidSpec {
                action: selfie::package::event::SpecWrite::Create,
                package_name: "tool".to_string(),
                issues: vec![
                    issue(ValidationLevel::Error, "environments.work.install"),
                    issue(ValidationLevel::Warning, "environments.work.check"),
                ],
            }),
        };

        let printed = printed_for(DisplayManager::new(false), vec![failed]).await;

        assert_eq!(
            printed,
            vec![
                (
                    Channel::Stderr,
                    "Refusing to create 'tool': it would not pass spec validate (1 error), so \
                     nothing was written"
                        .to_string()
                ),
                (
                    Channel::Stderr,
                    "ERROR environments.work.install: Unmatched double quote in command. Add a \
                     closing double quote."
                        .to_string()
                ),
                (
                    Channel::Stderr,
                    "WARN environments.work.check: Unmatched double quote in command. Add a \
                     closing double quote."
                        .to_string()
                ),
            ]
        );
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
                kind: selfie::package::event::StepKind::Local,
                message: "Loading package file".to_string(),
            },
            PackageEvent::Completed {
                operation_info: op,
                result: OperationResult::Failure(OperationFailure::Generic("Package not found")),
            },
        ];

        let display = DisplayManager::new(false);
        let processor = EventProcessor::new(display);
        let event_stream = Box::pin(stream::iter(events));
        let result = processor.process_events(event_stream, |_event| false).await;

        assert_eq!(result.exit, Exit::Failed);
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

        assert_eq!(result.exit, Exit::Failed);

        // The collected detail carries both halves, so `--verbose` summaries and
        // the MCP server's JSON still say what to do about it.
        let collected = display_for_assert.collected_errors();
        let message = collected
            .first()
            .expect("an error was collected")
            .message
            .to_string();
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
                kind: selfie::package::event::StepKind::Local,
                message: "Step 1".to_string(),
            },
            PackageEvent::Progress {
                operation_info: op.clone(),
                step: 2,
                total_steps: 3,
                percent_complete: 2.0 / 3.0,
                kind: selfie::package::event::StepKind::Local,
                message: "Step 2".to_string(),
            },
            PackageEvent::Completed {
                operation_info: op,
                result: OperationResult::Failure(OperationFailure::Generic("Failed")),
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
        assert_eq!(result.exit, Exit::Failed);
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
        assert_eq!(result.exit, Exit::Canceled);
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
                    stdout: selfie::commands::BoundedText::bound(b""),
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
        assert_eq!(errors[0].package_name.to_string(), "fail-pkg");
        assert_eq!(
            errors[0]
                .command
                .as_ref()
                .map(ToString::to_string)
                .as_deref(),
            Some("brew install fail-pkg")
        );
        assert_eq!(errors[0].exit_code, Some(1));
        assert_eq!(
            errors[0]
                .stderr
                .iter()
                .flatten()
                .map(Output::as_str)
                .collect::<Vec<_>>(),
            ["not found"]
        );
    }

    // A long install command is named in the summary as the error sentence
    // bounds it, keeping both ends, never whole.
    #[tokio::test]
    async fn a_failure_summary_names_a_long_command_in_its_bounded_form() {
        use selfie::package::event::{CommandFailure, OperationFailure, OperationResult};

        let command = format!("head-{}-tail", "z".repeat(3_000));
        let events = vec![PackageEvent::Completed {
            operation_info: make_operation_info("bat"),
            result: OperationResult::Failure(OperationFailure::CommandError(
                CommandFailure::ExecutionFailed {
                    command: command.clone(),
                    exit_code: Some(1),
                    stdout: selfie::commands::BoundedText::bound(b""),
                    stderr: selfie::commands::BoundedText::bound(b"e"),
                },
            )),
        }];
        let display = DisplayManager::new(false);

        let _ = printed_for(display.clone(), events).await;

        let errors = display.collected_errors();
        let named = errors[0]
            .command
            .as_ref()
            .map(ToString::to_string)
            .unwrap_or_default();
        assert!(named.len() < 2_100, "{} bytes", named.len());
        assert!(
            named.starts_with("head-") && named.ends_with("-tail"),
            "{named}"
        );
        assert!(named.contains("bytes elided"), "{named}");
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
                failed_count: 0,
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

        assert_eq!(result.exit, Exit::Failed);
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
                failed_count: 0,
                orphan_count: 0,
                environment: "test".to_string(),
                steps_completed: StepCount::new(1, 1),
            }),
        }];

        let display = DisplayManager::new(false);
        let processor = EventProcessor::new(display);
        let event_stream = Box::pin(stream::iter(events));
        let result = processor.process_events(event_stream, |_event| false).await;

        assert_eq!(result.exit, Exit::Clean);
    }

    #[tokio::test]
    async fn test_completed_generic_failure_collects_error_detail() {
        use selfie::package::event::{OperationFailure, OperationResult};

        let op = make_operation_info("generic-fail");

        let events: Vec<PackageEvent> = vec![PackageEvent::Completed {
            operation_info: op,
            result: OperationResult::Failure(OperationFailure::Generic("something went wrong")),
        }];

        let display = DisplayManager::new(false);
        let display_clone = display.clone();
        let processor = EventProcessor::new(display);
        let event_stream = Box::pin(stream::iter(events));
        processor.process_events(event_stream, |_event| false).await;

        let errors = display_clone.collected_errors();
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].package_name.to_string(), "generic-fail");
        assert_eq!(errors[0].message.to_string(), "something went wrong");
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

    fn file_under(
        kind: selfie::package::event::BaseKind,
        directory: &str,
        path: &str,
    ) -> selfie::package::event::DotfileSource {
        use selfie::package::event::{DotfileSource, RepoPath, SourceBase};

        DotfileSource::File(RepoPath {
            base: Some(SourceBase {
                kind,
                directory: directory.into(),
            }),
            path: path.into(),
        })
    }

    fn package_file(path: &str) -> selfie::package::event::DotfileSource {
        file_under(
            selfie::package::event::BaseKind::PackageDirectory,
            "/r/p",
            path,
        )
    }

    fn dotfiles_file(path: &str) -> selfie::package::event::DotfileSource {
        file_under(
            selfie::package::event::BaseKind::DotfilesDirectory,
            "/r/d",
            path,
        )
    }

    fn skipped(source: selfie::package::event::DotfileSource, target: &str) -> PackageEvent {
        PackageEvent::DotfileSkipped {
            operation_info: make_operation_info("p"),
            source,
            target: target.to_string(),
            reason: selfie::package::event::SkipReason::UpToDate,
        }
    }

    fn announced(entries: &[(selfie::package::event::DotfileSource, &str)]) -> PackageEvent {
        PackageEvent::DotfileEntriesPlanned {
            operation_info: make_operation_info("p"),
            entries: entries
                .iter()
                .map(|(source, target)| selfie::package::event::PlannedEntry {
                    source: source.clone(),
                    target: target.to_string(),
                })
                .collect(),
        }
    }

    fn lines(display: &DisplayManager) -> Vec<String> {
        display.printed().into_iter().map(|(_, l)| l).collect()
    }

    // The announced entries fix one set of columns for the whole run, so rows
    // under different headings line up with each other.
    #[test]
    fn rows_under_every_heading_share_the_announced_columns() {
        let display = DisplayManager::new(false);
        let mut processor = EventProcessor::new(display.clone());
        let entries = [
            (package_file("a"), "/h/.a"),
            (package_file("bat/config"), "/h/.config/bat/config"),
            (dotfiles_file("zshrc"), "/h/.zshrc"),
        ];

        processor.handle_event(announced(&entries));
        for (source, target) in &entries {
            processor.handle_event(skipped(source.clone(), target));
        }

        assert_eq!(
            lines(&display),
            vec![
                "Packages: /r/p",
                "skipped     a          → /h/.a                  already up to date",
                "skipped     bat/config → /h/.config/bat/config  already up to date",
                "Dotfiles: /r/d",
                "skipped     zshrc      → /h/.zshrc              already up to date",
            ]
        );
    }

    // A conflict's block, its row and the diff under it, starts with a blank
    // line, so the diff does not read as part of the row above.
    #[test]
    fn a_conflict_block_is_set_off_from_the_row_above() {
        let display = DisplayManager::new(false);
        let mut processor = EventProcessor::new(display.clone());
        processor.handle_event(skipped(package_file("a"), "/h/.a"));

        processor.handle_event(PackageEvent::DotfileConflict {
            operation_info: make_operation_info("p"),
            source: package_file("b"),
            target: "/h/.b".to_string(),
            detail: selfie::package::event::ConflictReport::Diff(String::new()),
            declined: false,
        });

        let lines = lines(&display);
        assert_eq!(lines[1], "skipped     a → /h/.a  already up to date");
        assert_eq!(lines[2], "", "{lines:?}");
        assert_eq!(lines[3], "conflict    b → /h/.b", "{lines:?}");
    }

    // With no announcement, columns have no width and a row reads single-spaced.
    #[test]
    fn a_row_with_no_announcement_reads_single_spaced() {
        let display = DisplayManager::new(false);
        let mut processor = EventProcessor::new(display.clone());

        processor.handle_event(skipped(package_file("bat/config"), "/h/.b"));

        assert_eq!(
            lines(&display),
            vec![
                "Packages: /r/p",
                "skipped     bat/config → /h/.b  already up to date"
            ]
        );
    }

    // `process_events`, the path a command takes, prints each row while the
    // stream is still open and before the next event exists, so nothing is
    // held until the run ends.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn rows_stream_through_process_events() {
        use selfie::package::event::{OperationResult, OperationSuccess};
        use std::time::{Duration, Instant};

        let display = DisplayManager::new(false);
        let (send, receive) = futures::channel::mpsc::unbounded::<PackageEvent>();
        let processing = tokio::spawn(
            EventProcessor::new(display.clone()).process_events(Box::pin(receive), |_| false),
        );
        let printed = |row: &str| {
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                if lines(&display).iter().any(|line| line.contains(row)) {
                    return true;
                }
                if Instant::now() > deadline {
                    return false;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
        };

        send.unbounded_send(announced(&[
            (package_file("a"), "/h/.a"),
            (package_file("b"), "/h/.b"),
        ]))
        .unwrap();
        send.unbounded_send(skipped(package_file("a"), "/h/.a"))
            .unwrap();
        assert!(
            printed("a → /h/.a"),
            "the first row was held: {:?}",
            lines(&display)
        );
        send.unbounded_send(skipped(package_file("b"), "/h/.b"))
            .unwrap();
        assert!(
            printed("b → /h/.b"),
            "the second row was held: {:?}",
            lines(&display)
        );
        send.unbounded_send(PackageEvent::Completed {
            operation_info: make_operation_info("p"),
            result: OperationResult::Success(OperationSuccess::Generic("done")),
        })
        .unwrap();
        drop(send);
        processing.await.unwrap();
    }
}
