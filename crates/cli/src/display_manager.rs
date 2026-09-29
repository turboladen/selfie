//! indicatif-powered display layer for CLI output
//!
//! Provides spinners, progress tracking, styled output, and structured error
//! summaries. Replaces `TerminalProgressReporter` with a unified display system
//! that handles both event-driven and static output consistently.

use std::collections::{BTreeMap, VecDeque};
use std::fmt::Display;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use console::style;
use indicatif::{MultiProgress, ProgressBar, ProgressDrawTarget, ProgressStyle};
use selfie::package::event::{StepEnding, StepId};

/// Shorten a path for display by replacing the home directory with `~`.
///
/// Only a path inside the home directory is shortened; anything else comes back
/// unchanged.
pub(crate) fn shorten_path(path: &str) -> String {
    match std::env::var("HOME") {
        Ok(home) => shorten_path_under(path, &home),
        Err(_) => path.to_string(),
    }
}

// Split out so a test can supply the home directory rather than set `HOME` for
// the whole test process.
//
// Component-wise, not a string prefix: with a home of `/Users/steve`, the path
// `/Users/steve2/x` is not inside it, and a string prefix renders it `~2/x`.
fn shorten_path_under(path: &str, home: &str) -> String {
    if home.is_empty() {
        return path.to_string();
    }
    match std::path::Path::new(path).strip_prefix(home) {
        Ok(rest) if rest.as_os_str().is_empty() => "~".to_string(),
        Ok(rest) => format!("~/{}", rest.display()),
        Err(_) => path.to_string(),
    }
}

/// Standard indentation for structured CLI output (e.g., section content,
/// result cards, list suggestions, and other indented fields).
pub(crate) const INDENT: &str = "   ";

/// A terminal prompt [`DisplayManager::prompt`] can ask.
///
/// Implemented for each dialoguer prompt the CLI uses. The call that reads the
/// answer happens inside `prompt`, so nothing else can draw on the terminal
/// while the user answers.
pub(crate) trait Prompt {
    /// What the user's answer is.
    type Answer;

    /// Ask on the terminal and wait for the answer.
    ///
    /// # Errors
    ///
    /// When there is no terminal to ask on, or the terminal fails.
    fn ask(self) -> dialoguer::Result<Self::Answer>;
}

/// A line of text the user types: printable characters only, with
/// line-editing keys. For free-form input, prompt with a bare
/// [`dialoguer::Input`].
pub(crate) struct TextLine<'a>(pub(crate) dialoguer::Input<'a, String>);

// The one place dialoguer's reading calls are allowed; crates/cli/clippy.toml
// forbids them everywhere else, so a prompt that bypasses `DisplayManager::prompt`
// does not build.
#[expect(
    clippy::disallowed_methods,
    reason = "every prompt is asked here, inside DisplayManager::prompt"
)]
mod ask {
    use super::{Prompt, TextLine};

    impl Prompt for dialoguer::Confirm<'_> {
        type Answer = bool;
        fn ask(self) -> dialoguer::Result<bool> {
            self.interact()
        }
    }

    impl Prompt for dialoguer::Select<'_> {
        type Answer = usize;
        fn ask(self) -> dialoguer::Result<usize> {
            self.interact()
        }
    }

    impl Prompt for dialoguer::MultiSelect<'_> {
        type Answer = Vec<usize>;
        fn ask(self) -> dialoguer::Result<Vec<usize>> {
            self.interact()
        }
    }

    // `interact_opt`: escape cancels rather than erroring.
    impl Prompt for dialoguer::FuzzySelect<'_> {
        type Answer = Option<usize>;
        fn ask(self) -> dialoguer::Result<Option<usize>> {
            self.interact_opt()
        }
    }

    impl Prompt for dialoguer::Input<'_, String> {
        type Answer = String;
        fn ask(self) -> dialoguer::Result<String> {
            self.interact()
        }
    }

    impl Prompt for TextLine<'_> {
        type Answer = String;
        fn ask(self) -> dialoguer::Result<String> {
            self.0.interact_text()
        }
    }
}

/// Structured error detail for the end-of-operation summary
#[derive(Debug, Clone)]
pub(crate) struct ErrorDetail {
    pub package_name: String,
    pub operation: String,
    pub command: Option<String>,
    pub exit_code: Option<i32>,
    /// Bounded by the library: the only producer copies it out of
    /// `CommandFailure::ExecutionFailed`, whose field is a `BoundedText` and so
    /// cannot hold unbounded text. There is deliberately no `stdout`
    /// counterpart: see `CommandFailure::ExecutionFailed`.
    pub stderr: Option<String>,
    pub message: String,
}

/// Collects errors during an operation for summary display at the end
#[derive(Debug, Clone, Default)]
pub(crate) struct ErrorCollector {
    errors: Vec<ErrorDetail>,
}

impl ErrorCollector {
    /// Add an error to the collection
    pub(crate) fn collect(&mut self, error: ErrorDetail) {
        self.errors.push(error);
    }

    /// Check if any errors have been collected
    #[cfg(test)]
    pub(crate) fn has_errors(&self) -> bool {
        !self.errors.is_empty()
    }

    /// Format and return the error summary as a string.
    ///
    /// Returns `None` if fewer than 2 errors were collected, since single
    /// errors are already shown inline by the event processor.
    pub(crate) fn format_summary(&self) -> Option<String> {
        if self.errors.len() <= 1 {
            return None;
        }

        let mut lines = Vec::new();
        lines.push(String::new());
        lines.push("── Errors ─────────────────────────────────────".to_string());

        for error in &self.errors {
            lines.push(format!("✗ {} ({})", error.package_name, error.operation));
            if !error.message.is_empty() {
                lines.push(format!("  {}", error.message));
            }
            if let Some(cmd) = &error.command {
                lines.push(format!("  Command: {cmd}"));
            }
            if let Some(code) = error.exit_code {
                lines.push(format!("  Exit code: {code}"));
            }
            if let Some(stderr) = &error.stderr {
                let stderr = stderr.trim();
                if !stderr.is_empty() {
                    lines.push("  stderr:".to_string());
                    for line in stderr.lines() {
                        lines.push(format!("    {line}"));
                    }
                }
            }
            lines.push(String::new());
        }

        lines.push("───────────────────────────────────────────────".to_string());
        Some(lines.join("\n"))
    }
}

/// How much of the CLI's running commentary to show.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum Verbosity {
    /// Results, warnings and errors, and a line for each step that waits on
    /// something outside selfie.
    #[default]
    Normal,
    /// Everything `Normal` shows, plus the operation header, every local step,
    /// a configured command's own output, and debug logs.
    Verbose,
}

impl From<bool> for Verbosity {
    /// `true` is [`Verbosity::Verbose`], as a `--verbose` flag reads.
    fn from(verbose: bool) -> Self {
        if verbose { Self::Verbose } else { Self::Normal }
    }
}

/// The stream a line was printed to (test capture).
#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Stream {
    Stdout,
    Stderr,
}

// A step the run is waiting on: its spinner on a terminal, what it waits on,
// and the last lines its command wrote.
struct WaitingLine {
    bar: Option<ProgressBar>,
    subject: String,
    started: Instant,
    tail: VecDeque<String>,
}

// How many of a step's last output lines a failure shows.
const TAIL_LINES: usize = 10;

/// Central display manager for all CLI output
///
/// Provides two API layers:
/// 1. **Static output**: `print_info()`, `print_error()`, etc. for simple styled messages
/// 2. **Waiting steps**: `start_waiting()` shows a step that waits on something
///    outside selfie, as a spinner on a terminal
///
/// All output goes through `MultiProgress` when spinners are active, preventing
/// interleaving and visual corruption.
#[derive(Clone)]
pub struct DisplayManager {
    mp: MultiProgress,
    use_colors: bool,
    // stderr is a terminal: the waiting spinner draws there.
    draws_spinner: bool,
    // Both streams are terminals.
    is_tty: bool,
    verbosity: Verbosity,
    errors: Arc<Mutex<ErrorCollector>>,
    // Steps overlap when a service runs them concurrently, so each is kept by
    // its own id; a `BTreeMap` keeps them in the order they started.
    waiting: Arc<Mutex<BTreeMap<StepId, WaitingLine>>>,
    // The lines the last failed step showed when it ended, so the failure that
    // reports it does not show them again.
    failed_tail: Arc<Mutex<Vec<String>>>,
    // What the printing methods rendered, and to which stream, so a test can
    // assert what a run would have shown. Output goes straight to the terminal,
    // which a test in this process cannot read back.
    #[cfg(test)]
    printed: Arc<Mutex<Vec<(Stream, String)>>>,
}

impl DisplayManager {
    /// Create a new display manager
    pub fn new(use_colors: bool) -> Self {
        // A test never probes: `cargo test` started from a terminal leaves both
        // descriptors on it, and a test must neither draw there nor prompt.
        let (draws_spinner, is_tty) = if cfg!(test) {
            (false, false)
        } else {
            let stderr = console::Term::stderr().is_term();
            (stderr, stderr && console::Term::stdout().is_term())
        };
        let mp = if draws_spinner {
            MultiProgress::new()
        } else {
            let mp = MultiProgress::new();
            mp.set_draw_target(ProgressDrawTarget::hidden());
            mp
        };

        Self {
            mp,
            use_colors,
            draws_spinner,
            is_tty,
            verbosity: Verbosity::Normal,
            errors: Arc::new(Mutex::new(ErrorCollector::default())),
            waiting: Arc::new(Mutex::new(BTreeMap::new())),
            failed_tail: Arc::new(Mutex::new(Vec::new())),
            #[cfg(test)]
            printed: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Set how much running commentary to show. The default is
    /// [`Verbosity::Normal`].
    #[must_use]
    pub(crate) fn with_verbosity(mut self, verbosity: Verbosity) -> Self {
        self.verbosity = verbosity;
        self
    }

    /// Whether the operation header, local steps and command output are shown.
    pub(crate) fn is_verbose(&self) -> bool {
        self.verbosity == Verbosity::Verbose
    }

    /// Whether colors are enabled
    pub fn use_colors(&self) -> bool {
        self.use_colors
    }

    #[cfg(test)]
    fn record(&self, stream: Stream, line: &str) {
        if let Ok(mut printed) = self.printed.lock() {
            printed.push((stream, line.to_string()));
        }
    }

    #[cfg(not(test))]
    fn record_stdout(&self, _line: &str) {}

    #[cfg(not(test))]
    fn record_stderr(&self, _line: &str) {}

    #[cfg(test)]
    fn record_stdout(&self, line: &str) {
        self.record(Stream::Stdout, line);
    }

    #[cfg(test)]
    fn record_stderr(&self, line: &str) {
        self.record(Stream::Stderr, line);
    }

    // Every line the printing methods printed, with its stream, in order.
    #[cfg(test)]
    pub(crate) fn printed(&self) -> Vec<(Stream, String)> {
        self.printed
            .lock()
            .map(|printed| printed.clone())
            .unwrap_or_default()
    }

    // Behave as though both streams were a terminal, drawing nowhere, so a test
    // can read the waiting spinner's message.
    #[cfg(test)]
    pub(crate) fn drawing_as_a_terminal(mut self) -> Self {
        self.draws_spinner = true;
        self.is_tty = true;
        self
    }

    // ── Waiting steps ─────────────────────────────────────────────────

    /// Show that the run now waits on `subject`, something outside selfie: a
    /// spinner on a terminal, otherwise one status line (stderr). Steps may
    /// overlap; each lasts until [`end_waiting`](Self::end_waiting) with its id.
    pub(crate) fn start_waiting(&self, step: StepId, subject: impl Display) {
        let subject = subject.to_string();
        let bar = if self.draws_spinner {
            Some(self.spinner(&subject))
        } else {
            self.print_progress(format!("{subject}..."));
            None
        };
        if let Ok(mut waiting) = self.waiting.lock() {
            waiting.insert(
                step,
                WaitingLine {
                    bar,
                    subject,
                    started: Instant::now(),
                    tail: VecDeque::new(),
                },
            );
        }
    }

    /// End the waiting step `step` (stderr). On a terminal a step that
    /// succeeded collapses to "✓ … (time)" and one that failed to "✗ … (time)".
    /// A failed step's last output lines are printed at normal verbosity, where
    /// they were hidden while it ran.
    pub(crate) fn end_waiting(&self, step: StepId, ending: StepEnding) {
        let Some(line) = self.waiting.lock().ok().and_then(|mut w| w.remove(&step)) else {
            return;
        };
        if let Some(bar) = &line.bar {
            bar.finish_and_clear();
            let text = format!("{} ({})", line.subject, elapsed(line.started.elapsed()));
            let mark = match ending {
                StepEnding::Succeeded => Some(("✓", style("✓").green())),
                StepEnding::Failed => Some(("✗", style("✗").red())),
                StepEnding::Cancelled => None,
            };
            if let Some((plain, styled)) = mark {
                self.record_stderr(&format!("{plain} {text}"));
                let rendered = if self.use_colors {
                    format!("{styled} {}", style(text).dim())
                } else {
                    format!("{plain} {text}")
                };
                let _ = self.mp.println(rendered);
            }
        }
        if ending == StepEnding::Failed && !line.tail.is_empty() {
            // Under `--verbose` the lines were printed as they came.
            if !self.is_verbose() {
                let tail: Vec<&str> = line.tail.iter().map(String::as_str).collect();
                self.print_error_context(&tail.join("\n"));
            }
            if let Ok(mut failed) = self.failed_tail.lock() {
                *failed = line.tail.into_iter().collect();
            }
        }
    }

    /// Clear every waiting step without a result line, for a run that has
    /// ended.
    pub(crate) fn clear_waiting(&self) {
        let lines = self
            .waiting
            .lock()
            .map(|mut w| std::mem::take(&mut *w))
            .unwrap_or_default();
        for line in lines.into_values() {
            if let Some(bar) = line.bar {
                bar.finish_and_clear();
            }
        }
    }

    /// Print a failed command's stderr as context for its error (stderr),
    /// leaving out the lines its step already showed when it ended.
    pub(crate) fn print_command_stderr(&self, stderr: &str) {
        let shown = self
            .failed_tail
            .lock()
            .map(|mut failed| std::mem::take(&mut *failed))
            .unwrap_or_default();
        let rest: Vec<&str> = stderr
            .lines()
            .filter(|line| !shown.contains(&printable(line)))
            .collect();
        if rest.iter().any(|line| !line.trim().is_empty()) {
            self.print_error_context(&rest.join("\n"));
        }
    }

    /// Render one line of the output of the command step `step` runs (stderr).
    ///
    /// At normal verbosity it becomes that step's spinner message on a
    /// terminal, and is not shown otherwise. Under `--verbose` every line is
    /// printed: indented and dimmed on a terminal, and prefixed with what the
    /// step waits on without one.
    pub(crate) fn command_output(&self, step: StepId, line: &str) {
        let line = printable(line);
        if line.trim().is_empty() {
            return;
        }
        let Ok(mut waiting) = self.waiting.lock() else {
            return;
        };
        // With one step open its lines need no label on a terminal; with more,
        // each says whose it is.
        let concurrent = waiting.len() > 1;
        let current = waiting.get_mut(&step);
        let subject = current
            .as_ref()
            .map(|w| w.subject.clone())
            .unwrap_or_default();
        if let Some(current) = current {
            if current.tail.len() == TAIL_LINES {
                current.tail.pop_front();
            }
            current.tail.push_back(line.clone());
            if !self.is_verbose()
                && let Some(bar) = &current.bar
            {
                bar.set_message(fit_to_terminal(&line));
            }
        }
        drop(waiting);
        if !self.is_verbose() {
            return;
        }
        let rendered = if self.draws_spinner {
            let text = if concurrent {
                format!("{subject} │ {line}")
            } else {
                line
            };
            if self.use_colors {
                format!("    {}", style(&text).dim())
            } else {
                format!("    {text}")
            }
        } else {
            format!("  {subject} │ {line}")
        };
        self.record_stderr(&rendered);
        self.mp.suspend(|| eprintln!("{rendered}"));
    }

    // The message each waiting spinner shows, in the order the steps started
    // (test-only).
    #[cfg(test)]
    pub(crate) fn waiting_messages(&self) -> Vec<String> {
        self.waiting
            .lock()
            .map(|w| {
                w.values()
                    .filter_map(|w| w.bar.as_ref().map(|bar| bar.message()))
                    .collect()
            })
            .unwrap_or_default()
    }

    fn spinner(&self, message: &str) -> ProgressBar {
        let template = if self.use_colors {
            "{spinner:.cyan} {msg}"
        } else {
            "{spinner} {msg}"
        };
        let bar = self.mp.add(ProgressBar::new_spinner());
        bar.set_style(
            ProgressStyle::with_template(template)
                .unwrap()
                .tick_strings(&["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"]),
        );
        bar.set_message(message.to_string());
        bar.enable_steady_tick(Duration::from_millis(80));
        bar
    }

    // ── Static output methods ──────────────────────────────────────────
    //
    // Canonical symbol set:
    //   ✓  success (green)      ✗  error/failure (red)
    //   ⚠  warning (yellow)     ℹ  info (blue)
    //   ✨ suggestion (yellow)   ── title ──  section header (bold)
    //
    // All output routes through mp.suspend() to avoid interleaving with
    // active spinners. This pauses the draw target, writes to the correct
    // stream (stdout or stderr), then resumes. Safe when no spinners are active.

    /// Print an informational message (stdout)
    pub(crate) fn print_info(&self, message: impl Display) {
        let message = message.to_string();
        self.record_stdout(&message);
        if self.use_colors {
            self.mp
                .suspend(|| println!("{} {}", style("ℹ").blue(), style(message).blue()));
        } else {
            self.mp.suspend(|| println!("ℹ {message}"));
        }
    }

    /// Print an error message (stderr)
    pub(crate) fn print_error(&self, message: impl Display) {
        let message = message.to_string();
        self.record_stderr(&message);
        if self.use_colors {
            self.mp.suspend(|| {
                eprintln!(
                    "{} {}",
                    style("✗").red().bold(),
                    style(message).red().bold()
                )
            });
        } else {
            self.mp.suspend(|| eprintln!("✗ {message}"));
        }
    }

    /// Print supporting lines under an error (stderr), one per line.
    ///
    /// For a block a reader looks at rather than reads — a source window under a
    /// parse failure. `block` is split on its own line breaks, so a trailing
    /// newline costs nothing. Each line is indented and dimmed and carries no `✗`;
    /// the marker belongs to the sentence above, not to every line of its evidence.
    pub(crate) fn print_error_context(&self, block: &str) {
        for line in block.lines() {
            self.record_stderr(line);
        }
        self.mp.suspend(|| {
            for line in block.lines() {
                if self.use_colors {
                    eprintln!("  {}", style(line).dim());
                } else {
                    eprintln!("  {line}");
                }
            }
        });
    }

    /// Print a command's summary line (stdout), marked by how the run came out:
    /// ✓ for Clean, ⚠ for Found, ✗ for Failed.
    ///
    /// The summary is the answer, so it goes to stdout whatever the outcome.
    pub(crate) fn print_result(
        &self,
        outcome: selfie::package::event::Outcome,
        message: impl Display,
    ) {
        use selfie::package::event::Outcome;

        let message = message.to_string();
        self.record_stdout(&message);
        let (mark, line) = match outcome {
            Outcome::Clean => ("✓", style(message.as_str()).green()),
            Outcome::Found => ("⚠", style(message.as_str()).yellow()),
            Outcome::Failed => ("✗", style(message.as_str()).red()),
        };
        if self.use_colors {
            let mark = match outcome {
                Outcome::Clean => style(mark).green().bold(),
                Outcome::Found => style(mark).yellow().bold(),
                Outcome::Failed => style(mark).red().bold(),
            };
            self.mp.suspend(|| println!("{mark} {line}"));
        } else {
            self.mp.suspend(|| println!("{mark} {message}"));
        }
    }

    /// Print a success message (stdout)
    pub(crate) fn print_success(&self, message: impl Display) {
        let message = message.to_string();
        self.record_stdout(&message);
        if self.use_colors {
            self.mp
                .suspend(|| println!("{} {}", style("✓").green().bold(), style(message).green()));
        } else {
            self.mp.suspend(|| println!("✓ {message}"));
        }
    }

    /// Print a warning message (stderr)
    pub(crate) fn print_warning(&self, message: impl Display) {
        let message = message.to_string();
        self.record_stderr(&message);
        if self.use_colors {
            self.mp.suspend(|| {
                eprintln!(
                    "{} {}",
                    style("⚠").yellow().bold(),
                    style(message).yellow().bold()
                )
            });
        } else {
            self.mp.suspend(|| eprintln!("⚠ {message}"));
        }
    }

    /// Print a line about the run's progress (stderr).
    pub(crate) fn print_progress(&self, message: impl Display) {
        let message = message.to_string();
        self.record_stderr(&message);
        if self.use_colors {
            self.mp.suspend(|| eprintln!("  {}", style(message).dim()));
        } else {
            self.mp.suspend(|| eprintln!("  {message}"));
        }
    }

    /// Print a line saying what the command is doing (stderr), only under
    /// [`Verbosity::Verbose`].
    ///
    /// Use it for work selfie does itself. A step that waits on something
    /// outside selfie uses [`start_waiting`](Self::start_waiting), which shows
    /// at every verbosity.
    pub(crate) fn print_status(&self, message: impl Display) {
        if self.is_verbose() {
            self.print_progress(message);
        }
    }

    /// Print a note about the run (stderr), such as the operation header.
    pub(crate) fn print_run_note(&self, message: impl Display) {
        let message = message.to_string();
        self.record_stderr(&message);
        if self.use_colors {
            self.mp
                .suspend(|| eprintln!("{} {}", style("ℹ").blue(), style(message).blue()));
        } else {
            self.mp.suspend(|| eprintln!("ℹ {message}"));
        }
    }

    /// Print a suggestion message (stdout)
    pub(crate) fn print_suggestion(&self, message: impl Display) {
        let message = message.to_string();
        self.record_stdout(&message);
        if self.use_colors {
            self.mp.suspend(|| {
                println!(
                    "{} {}: {}",
                    style("✨").bold(),
                    style("Suggestion").yellow().bold(),
                    message
                )
            });
        } else {
            self.mp.suspend(|| println!("✨ Suggestion: {message}"));
        }
    }

    /// Print a section header (stdout)
    pub(crate) fn print_section_header(&self, title: impl Display) {
        let title = title.to_string();
        self.record_stdout(&title);
        if self.use_colors {
            self.mp
                .suspend(|| println!("── {} ──", style(&title).bold()));
        } else {
            self.mp.suspend(|| println!("── {title} ──"));
        }
    }

    /// Print a unified diff with per-line coloring and visual framing (stdout)
    ///
    /// Layout:
    /// ```text
    /// --- old/path
    /// +++ new/path
    /// ──────────────────────────────────────────
    ///  context line
    /// -removed line
    /// +added line
    /// ──────────────────────────────────────────
    /// ```
    ///
    /// Colors: `---` red, `+++` green, `@@` cyan, `-` red, `+` green,
    /// context dim, separator dim. Paths shortened with `~`.
    pub(crate) fn print_diff(&self, diff: &str) {
        for line in diff.lines() {
            self.record_stdout(line);
        }
        const SEPARATOR: &str =
            "──────────────────────────────────────────────────────────────────────";

        self.mp.suspend(|| {
            let mut printed_separator = false;

            for line in diff.lines() {
                if self.use_colors {
                    if let Some(rest) = line.strip_prefix("--- ") {
                        println!("  {}", style(format!("--- {}", shorten_path(rest))).red());
                    } else if let Some(rest) = line.strip_prefix("+++ ") {
                        println!("  {}", style(format!("+++ {}", shorten_path(rest))).green());
                        println!("  {}", style(SEPARATOR).dim());
                        printed_separator = true;
                    } else if line.starts_with("@@") {
                        println!("  {}", style(line).cyan());
                    } else if line.starts_with('-') {
                        println!("  {}", style(line).red());
                    } else if line.starts_with('+') {
                        println!("  {}", style(line).green());
                    } else {
                        println!("  {}", style(line).dim());
                    }
                } else if let Some(rest) = line.strip_prefix("--- ") {
                    println!("  --- {}", shorten_path(rest));
                } else if let Some(rest) = line.strip_prefix("+++ ") {
                    println!("  +++ {}", shorten_path(rest));
                    println!("  {SEPARATOR}");
                    printed_separator = true;
                } else {
                    println!("  {line}");
                }
            }

            if printed_separator {
                if self.use_colors {
                    println!("  {}", style(SEPARATOR).dim());
                } else {
                    println!("  {SEPARATOR}");
                }
            }
        });
    }

    /// Ask `prompt` on the terminal, with nothing else drawing on it until the
    /// user answers.
    ///
    /// # Errors
    ///
    /// When there is no terminal to ask on, or the terminal fails.
    pub(crate) fn prompt<P: Prompt>(&self, prompt: P) -> dialoguer::Result<P::Answer> {
        // Suspended, not cleared: a spinner would redraw over the question while
        // the user types, but a step still running, or whose end is already on
        // its way, keeps its line and gets its ✓ or ✗ once the prompt returns.
        self.mp.suspend(|| prompt.ask())
    }

    /// Print a plain line to stdout
    pub(crate) fn println(&self, message: impl Display) {
        let message = message.to_string();
        self.record_stdout(&message);
        self.mp.suspend(|| println!("{message}"));
    }

    /// Print a styled key-value pair (stdout, for config display, etc.)
    pub(crate) fn print_field(&self, key: impl Display, value: impl Display) {
        self.record_stdout(&format!("{key} {value}"));
        if self.use_colors {
            self.mp
                .suspend(|| println!("  {} {}", style(key).italic().dim(), style(value).bold()));
        } else {
            self.mp.suspend(|| println!("  {key} {value}"));
        }
    }

    /// Whether stdout and stderr are both terminals.
    pub(crate) fn is_tty(&self) -> bool {
        self.is_tty
    }

    // ── Result card builder ─────────────────────────────────────────────

    /// Create a structured result card (section header + key-value pairs)
    ///
    /// Usage:
    /// ```ignore
    /// display.result_card("Check Results")
    ///     .field("Package", &package_name)
    ///     .field("Environment", &environment)
    ///     .field_if("Command", check_command.as_deref())
    ///     .print();
    /// ```
    pub(crate) fn result_card(&self, title: impl Display) -> ResultCard<'_> {
        ResultCard::new(self, title)
    }

    // ── Error collection ───────────────────────────────────────────────

    /// Collect a structured error for the end-of-operation summary
    pub(crate) fn collect_error(&self, error: ErrorDetail) {
        if let Ok(mut collector) = self.errors.lock() {
            collector.collect(error);
        }
    }

    /// Print the error summary if any errors were collected
    ///
    /// Call this after all operations are complete (e.g., at the end of
    /// `EventProcessor::process_events`).
    pub(crate) fn finish(&self) {
        // A stream that ended without a result still has its spinner drawn.
        self.clear_waiting();
        // Extract summary while holding the lock, then drop it before
        // doing terminal I/O to avoid blocking concurrent collect_error() calls.
        let summary = {
            if let Ok(collector) = self.errors.lock() {
                collector.format_summary()
            } else {
                None
            }
        };

        if let Some(summary) = summary {
            self.mp.suspend(|| eprintln!("{summary}"));
        }
    }

    /// Check if any errors have been collected
    #[cfg(test)]
    pub(crate) fn has_errors(&self) -> bool {
        self.errors.lock().map(|c| c.has_errors()).unwrap_or(false)
    }

    /// Return a snapshot of all collected errors (test-only)
    #[cfg(test)]
    pub(crate) fn collected_errors(&self) -> Vec<ErrorDetail> {
        self.errors
            .lock()
            .map(|c| c.errors.clone())
            .unwrap_or_default()
    }
}

/// Builder for structured result cards (section header + key-value pairs)
///
/// Created by [`DisplayManager::result_card()`]. Call `.field()` / `.field_if()`
/// to add rows, then `.print()` to render.
pub(crate) struct ResultCard<'a> {
    display: &'a DisplayManager,
    title: String,
    fields: Vec<(String, String)>,
}

impl<'a> ResultCard<'a> {
    fn new(display: &'a DisplayManager, title: impl Display) -> Self {
        Self {
            display,
            title: title.to_string(),
            fields: Vec::new(),
        }
    }

    /// Add a key-value field to the card
    pub(crate) fn field(mut self, key: &str, value: impl Display) -> Self {
        self.fields.push((key.to_string(), value.to_string()));
        self
    }

    /// Add a field only if the value is `Some`
    pub(crate) fn field_if(mut self, key: &str, value: Option<impl Display>) -> Self {
        if let Some(v) = value {
            self.fields.push((key.to_string(), v.to_string()));
        }
        self
    }

    /// Render the card to the display
    pub(crate) fn print(self) {
        use crate::formatters::format_key;

        self.display.println("");
        self.display.print_section_header(&self.title);

        let use_colors = self.display.use_colors();
        for (key, value) in &self.fields {
            self.display.println(format!(
                "{}{}: {}",
                INDENT,
                format_key(key, use_colors),
                value
            ));
        }
    }
}

// `line` as a terminal would leave it, with no control characters left, so a
// command's colors or carriage returns cannot redraw the spinner line.
fn printable(line: &str) -> String {
    let line = console::strip_ansi_codes(line);
    // A carriage return redraws the line in place, so only the text after the
    // last one is still on screen: a progress meter shows its final state.
    let shown = line
        .rsplit('\r')
        .find(|part| !part.trim().is_empty())
        .unwrap_or_default();
    shown
        .chars()
        .map(|c| if c == '\t' { ' ' } else { c })
        .filter(|c| !c.is_control())
        .collect()
}

// `line` cut to fit beside the spinner on the terminal, measured in columns.
fn fit_to_terminal(line: &str) -> String {
    let (_, columns) = console::Term::stderr().size();
    // The spinner glyph and its space.
    let width = usize::from(columns).saturating_sub(2).max(10);
    console::truncate_str(line, width, "…").into_owned()
}

// "12s", or "2m 05s" past a minute.
fn elapsed(duration: Duration) -> String {
    let seconds = duration.as_secs();
    if seconds < 60 {
        format!("{seconds}s")
    } else {
        format!("{}m {:02}s", seconds / 60, seconds % 60)
    }
}

impl std::fmt::Debug for DisplayManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DisplayManager")
            .field("use_colors", &self.use_colors)
            .field("is_tty", &self.is_tty)
            .field("verbosity", &self.verbosity)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ONE: StepId = StepId::from_raw(1);

    #[test]
    fn a_path_inside_home_is_shortened() {
        assert_eq!(
            shorten_path_under("/Users/steve/.config/x", "/Users/steve"),
            "~/.config/x"
        );
        assert_eq!(shorten_path_under("/Users/steve", "/Users/steve"), "~");
    }

    // A sibling whose name starts with the home directory's is not inside it.
    #[test]
    fn a_sibling_of_home_is_left_alone() {
        assert_eq!(
            shorten_path_under("/Users/steve2/x", "/Users/steve"),
            "/Users/steve2/x"
        );
        assert_eq!(
            shorten_path_under("/etc/hosts", "/Users/steve"),
            "/etc/hosts"
        );
    }

    #[test]
    fn test_display_manager_creation() {
        let dm = DisplayManager::new(false);
        assert!(!dm.use_colors());
    }

    #[test]
    fn test_display_manager_clone() {
        let dm = DisplayManager::new(false);
        let _dm2 = dm.clone();
    }

    #[test]
    fn test_display_manager_debug() {
        let dm = DisplayManager::new(false);
        let debug = format!("{dm:?}");
        assert!(debug.contains("DisplayManager"));
    }

    #[test]
    fn test_static_output_methods_dont_panic() {
        let dm = DisplayManager::new(false);
        dm.print_info("test info");
        dm.print_error("test error");
        dm.print_success("test success");
        dm.print_warning("test warning");
        dm.print_progress("test progress");
        dm.print_suggestion("test suggestion");
        dm.print_section_header("test section");
        dm.println("test println");
        dm.print_field("key:", "value");
    }

    #[test]
    fn test_static_output_with_colors() {
        let dm = DisplayManager::new(true);
        dm.print_info("test info");
        dm.print_error("test error");
        dm.print_success("test success");
        dm.print_warning("test warning");
    }

    #[test]
    fn test_error_collector_empty() {
        let collector = ErrorCollector::default();
        assert!(!collector.has_errors());
        assert!(collector.format_summary().is_none());
    }

    #[test]
    fn test_error_collector_single_error_skips_summary() {
        let mut collector = ErrorCollector::default();
        collector.collect(ErrorDetail {
            package_name: "test-pkg".to_string(),
            operation: "install".to_string(),
            command: Some("brew install test-pkg".to_string()),
            exit_code: Some(1),
            stderr: Some("Error: not found".to_string()),
            message: "Installation failed".to_string(),
        });

        assert!(collector.has_errors());
        assert!(
            collector.format_summary().is_none(),
            "Single error should not produce a summary (inline message covers it)"
        );
    }

    #[test]
    fn test_error_collector_multiple_errors_shows_summary() {
        let mut collector = ErrorCollector::default();
        collector.collect(ErrorDetail {
            package_name: "test-pkg".to_string(),
            operation: "install".to_string(),
            command: Some("brew install test-pkg".to_string()),
            exit_code: Some(1),
            stderr: Some("Error: not found".to_string()),
            message: "Installation failed".to_string(),
        });
        collector.collect(ErrorDetail {
            package_name: "other-pkg".to_string(),
            operation: "install".to_string(),
            command: Some("brew install other-pkg".to_string()),
            exit_code: Some(1),
            stderr: None,
            message: "Also failed".to_string(),
        });

        assert!(collector.has_errors());
        let summary = collector.format_summary().unwrap();
        assert!(summary.contains("test-pkg"));
        assert!(summary.contains("brew install test-pkg"));
        assert!(summary.contains("Exit code: 1"));
        assert!(summary.contains("Error: not found"));
        assert!(summary.contains("other-pkg"));
        assert!(summary.contains("── Errors"));
    }

    #[test]
    fn test_display_manager_error_collection() {
        let dm = DisplayManager::new(false);
        assert!(!dm.has_errors());

        dm.collect_error(ErrorDetail {
            package_name: "test".to_string(),
            operation: "check".to_string(),
            command: None,
            exit_code: None,
            stderr: None,
            message: "test error".to_string(),
        });

        assert!(dm.has_errors());
        // finish() with a single error should not print summary (just verify no panic)
        dm.finish();
    }

    #[test]
    fn test_display_manager_error_collection_across_clones() {
        let dm = DisplayManager::new(false);
        let dm2 = dm.clone();

        dm.collect_error(ErrorDetail {
            package_name: "test".to_string(),
            operation: "install".to_string(),
            command: None,
            exit_code: None,
            stderr: None,
            message: "shared error".to_string(),
        });

        // Clone should see the same errors (Arc<Mutex<>>)
        assert!(dm2.has_errors());
    }

    #[test]
    fn test_result_card_basic() {
        let dm = DisplayManager::new(false);
        // Verify the builder API works without panicking
        dm.result_card("Test Results")
            .field("Package", "test-pkg")
            .field("Environment", "macos")
            .print();
    }

    #[test]
    fn test_result_card_with_colors() {
        let dm = DisplayManager::new(true);
        dm.result_card("Test Results")
            .field("Package", "test-pkg")
            .field("Status", "valid")
            .print();
    }

    #[test]
    fn test_result_card_field_if() {
        let dm = DisplayManager::new(false);
        let cmd: Option<&str> = Some("brew install foo");
        let missing: Option<&str> = None;

        dm.result_card("Test Results")
            .field("Package", "test-pkg")
            .field_if("Command", cmd)
            .field_if("Missing", missing)
            .print();
    }

    #[test]
    fn test_static_output_during_a_waiting_step() {
        let dm = DisplayManager::new(false).drawing_as_a_terminal();
        dm.start_waiting(ONE, "Running the install command for bat");
        // These should not panic even with a spinner running
        dm.print_info("info during spinner");
        dm.print_warning("warning during spinner");
        dm.print_error("error during spinner");
        dm.print_success("success during spinner");
        dm.println("plain during spinner");
        dm.print_section_header("header during spinner");
        dm.print_progress("progress during spinner");
        dm.print_suggestion("suggestion during spinner");
        dm.print_field("key:", "value");
        // An ordinary line does not end the wait.
        assert!(!dm.waiting_messages().is_empty());
        dm.end_waiting(ONE, StepEnding::Succeeded);
    }

    #[test]
    fn a_command_line_becomes_the_spinner_message_on_a_terminal() {
        let dm = DisplayManager::new(false).drawing_as_a_terminal();
        dm.start_waiting(ONE, "Running the install command for ripgrep");

        dm.command_output(ONE, "\x1b[32mDownloading\x1b[0m ripgrep\r");
        assert_eq!(dm.waiting_messages(), vec!["Downloading ripgrep"]);

        dm.command_output(ONE, "Pouring ripgrep");
        assert_eq!(dm.waiting_messages(), vec!["Pouring ripgrep"]);
        // Nothing from the command is printed as a line of its own.
        assert!(dm.printed().is_empty(), "{:?}", dm.printed());
    }

    // Without a terminal and at default verbosity, a command's output is not
    // shown: the one status line stands for it.
    #[test]
    fn a_command_line_is_hidden_without_a_terminal() {
        let dm = DisplayManager::new(false);
        dm.start_waiting(ONE, "Running the install command for ripgrep");
        dm.command_output(ONE, "Downloading ripgrep");

        assert_eq!(
            dm.printed(),
            vec![(
                Stream::Stderr,
                "Running the install command for ripgrep...".to_string()
            )]
        );
    }

    // Under `--verbose` without a terminal, every line prints on stderr,
    // prefixed with what the step waits on.
    #[test]
    fn a_command_line_is_prefixed_when_verbose() {
        let dm = DisplayManager::new(false).with_verbosity(Verbosity::Verbose);
        dm.start_waiting(ONE, "Running the install command for ripgrep");
        dm.command_output(ONE, "Downloading ripgrep");

        assert_eq!(
            dm.printed().last(),
            Some(&(
                Stream::Stderr,
                "  Running the install command for ripgrep │ Downloading ripgrep".to_string()
            ))
        );
    }

    // A finished step collapses to one line with its elapsed time.
    #[test]
    fn a_finished_wait_collapses_to_one_line_with_its_time() {
        let dm = DisplayManager::new(false).drawing_as_a_terminal();
        dm.start_waiting(ONE, "Running the install command for ripgrep");
        dm.end_waiting(ONE, StepEnding::Succeeded);

        assert_eq!(
            dm.printed(),
            vec![(
                Stream::Stderr,
                "✓ Running the install command for ripgrep (0s)".to_string()
            )]
        );
        assert!(dm.waiting_messages().is_empty());
    }

    #[test]
    fn elapsed_time_reads_in_seconds_then_minutes() {
        assert_eq!(elapsed(Duration::from_secs(12)), "12s");
        assert_eq!(elapsed(Duration::from_secs(125)), "2m 05s");
    }

    // A prompt that answers without a terminal.
    struct Answered;

    impl Prompt for Answered {
        type Answer = ();

        fn ask(self) -> dialoguer::Result<()> {
            Ok(())
        }
    }

    // A prompt suspends the spinners and keeps their steps: a step whose end
    // arrives after the prompt still prints its line.
    #[test]
    fn a_step_keeps_its_end_line_across_a_prompt() {
        let dm = DisplayManager::new(false).drawing_as_a_terminal();
        dm.start_waiting(ONE, "Running the commands that produce ~/.creds");

        let _ = dm.prompt(Answered);
        assert_eq!(
            dm.waiting_messages(),
            vec!["Running the commands that produce ~/.creds"]
        );
        dm.end_waiting(ONE, StepEnding::Succeeded);

        assert_eq!(
            dm.printed(),
            vec![(
                Stream::Stderr,
                "✓ Running the commands that produce ~/.creds (0s)".to_string()
            )]
        );
    }

    // A progress meter redraws with carriage returns; only its last state is
    // on screen. A tab separates fields.
    #[test]
    fn a_redrawn_line_shows_its_last_state_and_a_tab_separates() {
        assert_eq!(printable("10%\r50%\r100%\r"), "100%");
        assert_eq!(printable("Name\tVersion"), "Name Version");
    }

    // A stream that ends without a result leaves no spinner behind.
    #[test]
    fn finishing_clears_a_waiting_spinner() {
        let dm = DisplayManager::new(false).drawing_as_a_terminal();
        dm.start_waiting(ONE, "Running the install command for bat");

        dm.finish();

        assert!(dm.waiting_messages().is_empty());
    }

    const TWO: StepId = StepId::from_raw(2);

    // Concurrent steps each keep their own spinner: ending one leaves the
    // other running, and its line names only itself.
    #[test]
    fn overlapping_steps_end_one_at_a_time() {
        let dm = DisplayManager::new(false).drawing_as_a_terminal();
        dm.start_waiting(ONE, "Running the audit command for bat");
        dm.start_waiting(TWO, "Running the audit command for fd");

        dm.end_waiting(ONE, StepEnding::Succeeded);

        assert_eq!(
            dm.waiting_messages(),
            vec!["Running the audit command for fd"]
        );
        assert_eq!(
            dm.printed(),
            vec![(
                Stream::Stderr,
                "✓ Running the audit command for bat (0s)".to_string()
            )]
        );
    }

    // Under `--verbose` without a terminal, a line is labeled with the step whose
    // command wrote it, not the step that started last.
    #[test]
    fn a_line_is_labeled_with_its_own_step() {
        let dm = DisplayManager::new(false).with_verbosity(Verbosity::Verbose);
        dm.start_waiting(ONE, "Running the install command for a");
        dm.start_waiting(TWO, "Running the install command for b");

        dm.command_output(ONE, "from a");

        assert_eq!(
            dm.printed().last(),
            Some(&(
                Stream::Stderr,
                "  Running the install command for a │ from a".to_string()
            ))
        );
    }

    // On a terminal under `--verbose`, a line needs no label while its step is
    // the only one open, and names its step while others run beside it.
    #[test]
    fn a_terminal_labels_lines_only_while_steps_overlap() {
        let dm = DisplayManager::new(false)
            .drawing_as_a_terminal()
            .with_verbosity(Verbosity::Verbose);
        dm.start_waiting(ONE, "Running the install command for a");
        dm.command_output(ONE, "alone");
        dm.start_waiting(TWO, "Running the install command for b");
        dm.command_output(ONE, "beside b");

        let lines: Vec<String> = dm.printed().into_iter().map(|(_, l)| l).collect();
        assert_eq!(
            lines,
            vec![
                "    alone".to_string(),
                "    Running the install command for a │ beside b".to_string()
            ]
        );
    }

    // A failed step is marked failed, never done, and its last lines are
    // shown, since they were hidden while it ran.
    #[test]
    fn a_failed_step_is_marked_and_shows_its_last_lines() {
        let dm = DisplayManager::new(false).drawing_as_a_terminal();
        dm.start_waiting(ONE, "Running the commands that produce ~/.creds");
        dm.command_output(ONE, "no session");

        dm.end_waiting(ONE, StepEnding::Failed);

        let printed: Vec<String> = dm.printed().into_iter().map(|(_, l)| l).collect();
        assert!(
            printed
                .iter()
                .any(|l| l.starts_with("✗ Running the commands")),
            "{printed:?}"
        );
        assert!(!printed.iter().any(|l| l.contains('✓')), "{printed:?}");
        assert!(
            printed.iter().any(|l| l.contains("no session")),
            "{printed:?}"
        );
    }

    // A failure prints the command's stderr even when the step's tail was
    // stdout alone, leaving out only the lines the tail already showed.
    #[test]
    fn a_failure_shows_the_stderr_its_tail_left_out() {
        let dm = DisplayManager::new(false);
        dm.start_waiting(ONE, "Running the install command for foo");
        dm.command_output(ONE, "Error: no bottle");
        dm.command_output(ONE, "==> Caveats");
        dm.end_waiting(ONE, StepEnding::Failed);

        dm.print_command_stderr("Error: checksum mismatch\nError: no bottle");

        let lines: Vec<String> = dm.printed().into_iter().map(|(_, l)| l).collect();
        let count = |needle: &str| lines.iter().filter(|l| l.contains(needle)).count();
        assert_eq!(count("checksum mismatch"), 1, "{lines:?}");
        assert_eq!(count("no bottle"), 1, "{lines:?}");
    }

    // Under `--verbose` the lines were shown as they came, so a failure does
    // not repeat them.
    #[test]
    fn a_failed_step_does_not_repeat_lines_shown_verbosely() {
        let dm = DisplayManager::new(false).with_verbosity(Verbosity::Verbose);
        dm.start_waiting(ONE, "Running the install command for a");
        dm.command_output(ONE, "no bottle");

        dm.end_waiting(ONE, StepEnding::Failed);

        let lines = dm
            .printed()
            .into_iter()
            .filter(|(_, l)| l.contains("no bottle"))
            .count();
        assert_eq!(lines, 1);
    }

    // Each step's time is its own: one that started while another had been
    // running for a second reports its own time, not the other's.
    #[test]
    fn elapsed_time_covers_only_its_own_step() {
        let dm = DisplayManager::new(false).drawing_as_a_terminal();
        dm.start_waiting(ONE, "first");
        std::thread::sleep(Duration::from_millis(1100));
        dm.start_waiting(TWO, "second");

        dm.end_waiting(TWO, StepEnding::Succeeded);
        dm.end_waiting(ONE, StepEnding::Succeeded);

        let printed: Vec<String> = dm.printed().into_iter().map(|(_, l)| l).collect();
        assert_eq!(
            printed,
            vec!["✓ second (0s)".to_string(), "✓ first (1s)".to_string()]
        );
    }

    // A cancelled step is cleared with no result line.
    #[test]
    fn a_cancelled_step_prints_nothing() {
        let dm = DisplayManager::new(false).drawing_as_a_terminal();
        dm.start_waiting(ONE, "Running the install command for a");

        dm.end_waiting(ONE, StepEnding::Cancelled);

        assert!(dm.printed().is_empty(), "{:?}", dm.printed());
        assert!(dm.waiting_messages().is_empty());
    }

    #[test]
    fn a_status_line_prints_only_when_verbose() {
        let normal = DisplayManager::new(false);
        normal.print_status("Loading specs...");
        assert!(normal.printed().is_empty());

        let verbose = DisplayManager::new(false).with_verbosity(Verbosity::Verbose);
        verbose.print_status("Loading specs...");
        assert_eq!(
            verbose.printed(),
            vec![(Stream::Stderr, "Loading specs...".to_string())]
        );
    }
}
