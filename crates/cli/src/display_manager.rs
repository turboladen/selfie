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
use selfie::package::event::{BaseKind, PlannedEntry, StepEnding, StepId};
use tokio_util::sync::CancellationToken;

use crate::entry_rows::{EntryColumns, EntryRow, entry_line};
use crate::event_processor::Exit;
use crate::source_paths::relative_text;
pub(crate) use crate::terminal_text::DisplayPath;
use crate::terminal_text::{
    Paint, Shown, escape, escape_prose, escape_prose_keeping_tabs, guard, printable, shown,
};

/// A path as the terminal shows it: the home directory replaced with `~`, and
/// its text escaped as [`DisplayPath`] escapes it.
///
/// Only a path inside the home directory is shortened.
pub(crate) fn shorten_path(path: &str) -> DisplayPath {
    match std::env::var("HOME") {
        Ok(home) => DisplayPath::new(&shorten_path_under(path, &home)),
        Err(_) => DisplayPath::new(path),
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
        Ok(rest) => format!("~/{}", rest.to_string_lossy()),
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
    type Answer: Send + 'static;

    /// Ask on the terminal and wait for the answer.
    ///
    /// # Errors
    ///
    /// When there is no terminal to ask on, or the terminal fails.
    #[cfg_attr(test, expect(dead_code, reason = "a test answers from a script"))]
    fn ask(self) -> dialoguer::Result<Self::Answer>;
}

/// A line of text the user types: printable characters only, with
/// line-editing keys. For free-form input, prompt with a bare
/// [`dialoguer::Input`].
pub(crate) struct TextLine<'a>(
    #[cfg_attr(test, expect(dead_code, reason = "a test answers from a script"))]
    pub(crate)  dialoguer::Input<'a, String>,
);

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

/// Why a prompt ended without an answer.
#[derive(Debug)]
pub(crate) enum PromptFailure {
    /// stderr is not a terminal, so nothing was asked.
    NoTerminal,
    /// The user pressed Ctrl+C.
    Interrupted,
    /// The terminal failed while the user answered.
    Unreadable(std::io::Error),
}

/// What a terminal that failed while the user answered is reported as.
fn unreadable_answer(error: &std::io::Error) -> String {
    format!("Could not read the answer: {error}")
}

impl PromptFailure {
    /// How a command that needed the answer ends.
    pub(crate) fn exit(&self) -> Exit {
        match self {
            Self::NoTerminal => Exit::Usage,
            Self::Interrupted => Exit::Canceled,
            Self::Unreadable(_) => Exit::Failed,
        }
    }
}

impl From<dialoguer::Error> for PromptFailure {
    // dialoguer refuses a prompt with no terminal as `NotConnected`, and console
    // reports Ctrl+C at a key read as `Interrupted` after raising SIGINT.
    fn from(error: dialoguer::Error) -> Self {
        let dialoguer::Error::IO(error) = error;
        match error.kind() {
            std::io::ErrorKind::NotConnected => Self::NoTerminal,
            std::io::ErrorKind::Interrupted => Self::Interrupted,
            _ => Self::Unreadable(error),
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
            lines.push(format!(
                "✗ {} ({})",
                escape(&error.package_name),
                error.operation
            ));
            if !error.message.is_empty() {
                lines.push(format!("  {}", untrusted(&error.message, 2)));
            }
            if let Some(cmd) = &error.command {
                lines.push(format!("  Command: {}", escape(cmd)));
            }
            if let Some(code) = error.exit_code {
                lines.push(format!("  Exit code: {code}"));
            }
            if let Some(stderr) = &error.stderr {
                let stderr = stderr.trim();
                if !stderr.is_empty() {
                    lines.push("  stderr:".to_string());
                    // A tool's own stderr, cleaned as command output is: its
                    // colors stripped, not shown as escapes.
                    for line in stderr.lines() {
                        lines.push(format!("    {}", printable(line)));
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

/// The stream a line is printed on. A source line's stream gets its own
/// headings, so a reader of either stream alone can resolve every path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Channel {
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
    // The base directory whose heading each stream showed last, shared by every
    // clone so the conflict prompt and the event processor head against one
    // another.
    last_heading: Arc<Mutex<[Option<BaseKind>; 2]>>,
    // Held while a heading and the line under it print, by every clone.
    block: Arc<Mutex<()>>,
    // The entry table's columns, shared by every clone, so the conflict prompt
    // lays out its row as the event processor lays out the rest.
    entry_columns: Arc<Mutex<EntryColumns>>,
    // What the printing methods rendered, and to which stream, so a test can
    // assert what a run would have shown. Output goes straight to the terminal,
    // which a test in this process cannot read back.
    #[cfg(test)]
    printed: Arc<Mutex<Vec<(Channel, String)>>>,
    // How many times `draw` wrote, so a test can see a block written as one.
    #[cfg(test)]
    draws: Arc<std::sync::atomic::AtomicUsize>,
    // stderr is a terminal, so a prompt can be drawn and answered.
    #[cfg_attr(test, expect(dead_code, reason = "a test answers from `script`"))]
    can_prompt: bool,
    // The run's token. A signal that cancels the run ends a prompt, and a
    // prompt ended by Ctrl+C cancels the run. Outside a test, a display
    // without it never prompts.
    cancellation: Option<CancellationToken>,
    // What the next prompts answer, in a test.
    #[cfg(test)]
    script: Arc<Mutex<VecDeque<Scripted>>>,
    // This holds what the next prompt does while it is open, before its answer,
    // in a test.
    #[cfg(test)]
    while_asking: Arc<Mutex<Option<WhileAsking>>>,
}

// A test's action while a prompt is open, run before the scripted answer, has
// this type.
#[cfg(test)]
type WhileAsking = Box<dyn FnOnce() + Send>;

// One scripted prompt result: an answer, or the error dialoguer would return.
#[cfg(test)]
pub(crate) type Scripted = dialoguer::Result<Box<dyn std::any::Any + Send>>;

// A scripted answer.
#[cfg(test)]
pub(crate) fn answer<T: std::any::Any + Send>(answer: T) -> Scripted {
    Ok(Box::new(answer))
}

// A scripted Ctrl+C, as console reports it at a key read.
#[cfg(test)]
pub(crate) fn ctrl_c() -> Scripted {
    Err(std::io::Error::new(std::io::ErrorKind::Interrupted, "read interrupted").into())
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
        // Nothing styled may reach a stream once colors are off, whatever
        // `console` would decide from the environment. Process-wide, so a test,
        // which builds many displays, leaves it alone.
        if !use_colors && !cfg!(test) {
            console::set_colors_enabled(false);
            console::set_colors_enabled_stderr(false);
        }
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
            last_heading: Arc::new(Mutex::new([None, None])),
            block: Arc::new(Mutex::new(())),
            entry_columns: Arc::new(Mutex::new(EntryColumns::default())),
            #[cfg(test)]
            printed: Arc::new(Mutex::new(Vec::new())),
            #[cfg(test)]
            draws: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            can_prompt: draws_spinner,
            cancellation: None,
            #[cfg(test)]
            script: Arc::new(Mutex::new(VecDeque::new())),
            #[cfg(test)]
            while_asking: Arc::new(Mutex::new(None)),
        }
    }

    /// Set how much running commentary to show. The default is
    /// [`Verbosity::Normal`].
    #[must_use]
    pub(crate) fn with_verbosity(mut self, verbosity: Verbosity) -> Self {
        self.verbosity = verbosity;
        self
    }

    /// Tie prompts to the run's `token`: a signal that cancels the run ends a
    /// waiting prompt as interrupted, and a prompt the user interrupts cancels
    /// the run. A display that prompts on a terminal must be built with it.
    ///
    /// Installs a process-wide signal handler, so it must be called inside the
    /// tokio runtime. Warns on stderr when it cannot, since a signal then no
    /// longer ends a prompt.
    #[must_use]
    pub(crate) fn with_cancellation(mut self, token: CancellationToken) -> Self {
        #[cfg(not(test))]
        if let Err(error) = wakeup::install() {
            self.print_warning(format!(
                "A signal cannot end a prompt in this run, so it takes a second one to stop \
                 selfie while it asks: {error}"
            ));
        }
        self.cancellation = Some(token);
        self
    }

    /// Whether the run this display belongs to has been canceled, by a signal
    /// or by Ctrl+C at a prompt.
    pub(crate) fn is_canceled(&self) -> bool {
        self.cancellation
            .as_ref()
            .is_some_and(CancellationToken::is_cancelled)
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
    fn record(&self, stream: Channel, line: &str) {
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
        self.record(Channel::Stdout, line);
    }

    #[cfg(test)]
    fn record_stderr(&self, line: &str) {
        self.record(Channel::Stderr, line);
    }

    // Every line the printing methods printed, with its stream, in order.
    #[cfg(test)]
    pub(crate) fn printed(&self) -> Vec<(Channel, String)> {
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
        let subject = escape(&subject.to_string()).into_owned();
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
                // The refusal or cancellation that follows says why; a mark
                // would claim an outcome for commands that did not finish or start.
                StepEnding::Canceled | StepEnding::NotRun => None,
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

    /// Record that `channel` now heads its lines with `kind`, and return whether
    /// it headed them with another base, or none, before.
    pub(crate) fn swap_heading(&self, channel: Channel, kind: BaseKind) -> bool {
        let index = match channel {
            Channel::Stdout => 0,
            Channel::Stderr => 1,
        };
        self.last_heading
            .lock()
            .map(|mut last| last[index].replace(kind) != Some(kind))
            .unwrap_or(true)
    }

    /// Fix the entry table's columns for `entries`, the entries a run is about
    /// to walk, as their rows will show them. Prints nothing.
    pub(crate) fn plan_entry_columns(&self, entries: &[PlannedEntry]) {
        let shown: Vec<(DisplayPath, DisplayPath)> = entries
            .iter()
            .map(|entry| (relative_text(&entry.source), shorten_path(&entry.target)))
            .collect();
        let columns = EntryColumns::of(
            shown
                .iter()
                .map(|(source, target)| (source.as_str(), target.as_str())),
        );
        if let Ok(mut current) = self.entry_columns.lock() {
            *current = columns;
        }
    }

    /// Print `row` in the entry table's columns (stdout), with its outcome's
    /// label colored.
    pub(crate) fn print_entry_row(&self, row: &EntryRow) {
        let columns = self
            .entry_columns
            .lock()
            .map(|columns| *columns)
            .unwrap_or_default();
        self.println(entry_line(row, columns, self.use_colors));
    }

    /// Hold every clone's printing of source lines until the guard drops, so
    /// a heading and the lines under it print together. Not reentrant: a
    /// holder must not ask again.
    pub(crate) fn hold_block(&self) -> std::sync::MutexGuard<'_, ()> {
        self.block
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Print a failed command's stderr as context for its error (stderr),
    /// leaving out the lines its step already showed when it ended.
    pub(crate) fn print_command_stderr(&self, stderr: &str) {
        let shown = self
            .failed_tail
            .lock()
            .map(|mut failed| std::mem::take(&mut *failed))
            .unwrap_or_default();
        let rest: Vec<String> = stderr
            .lines()
            .map(printable)
            .filter(|line| !shown.contains(line))
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
        let message = untrusted(message, 2);
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
        let message = untrusted(message, 2);
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
        let lines: Vec<String> = block
            .lines()
            .map(|line| escape_prose_keeping_tabs(line).into_owned())
            .collect();
        for line in &lines {
            self.record_stderr(line);
        }
        self.mp.suspend(|| {
            for line in &lines {
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

        let message = untrusted(message, 2);
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
        let message = untrusted(message, 2);
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
        let message = untrusted(message, 2);
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
        let message = untrusted(message, 2);
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
        let message = untrusted(message, 2);
        self.record_stderr(&message);
        if self.use_colors {
            self.mp
                .suspend(|| eprintln!("{} {}", style("ℹ").blue(), style(message).blue()));
        } else {
            self.mp.suspend(|| eprintln!("ℹ {message}"));
        }
    }

    /// Print a suggestion (stderr): advice about the run, never the answer.
    pub(crate) fn print_suggestion(&self, message: impl Display) {
        let message = untrusted(message, 2);
        self.suggest(&message, &message);
    }

    /// Print a suggestion selfie styled itself, such as one naming a command
    /// to run in a highlight (stderr).
    pub(crate) fn print_styled_suggestion(&self, suggestion: Shown) {
        let plain = guard(suggestion.as_str(), false).into_owned();
        self.suggest(&plain, suggestion.as_str());
    }

    // `plain` is what a test reads back, `shown` what the terminal gets; both
    // are already escaped.
    fn suggest(&self, plain: &str, shown: &str) {
        self.record_stderr(plain);
        let shown = guard(shown, self.stderr_colors());
        if self.use_colors {
            self.mp.suspend(|| {
                eprintln!(
                    "{} {}: {}",
                    style("✨").bold(),
                    style("Suggestion").yellow().bold(),
                    shown
                )
            });
        } else {
            self.mp.suspend(|| eprintln!("✨ Suggestion: {shown}"));
        }
    }

    /// Print a section header (stdout)
    pub(crate) fn print_section_header(&self, title: impl Display) {
        self.print_section_header_to(Channel::Stdout, title);
    }

    /// Print a section header on `channel`.
    pub(crate) fn print_section_header_to(&self, channel: Channel, title: impl Display) {
        let title = escape(&title.to_string()).into_owned();
        let rendered = if self.use_colors {
            format!("── {} ──", style(&title).bold())
        } else {
            format!("── {title} ──")
        };
        match channel {
            Channel::Stdout => {
                self.record_stdout(&title);
                self.mp.suspend(|| println!("{rendered}"));
            }
            Channel::Stderr => {
                self.record_stderr(&title);
                self.mp.suspend(|| eprintln!("{rendered}"));
            }
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
        const SEPARATOR: &str =
            "──────────────────────────────────────────────────────────────────────";

        // Each line is file content, so a tab stays a tab and everything else
        // that could act on the terminal shows as an escape.
        let mut rows: Vec<(Shown, Paint)> = Vec::new();
        let mut printed_separator = false;
        for line in diff_lines(diff) {
            match line {
                DiffLine::OldFile(rest) => {
                    rows.push((shown!("--- {}", shorten_path(rest)), |s| s.red()));
                }
                DiffLine::NewFile(rest) => {
                    rows.push((shown!("+++ {}", shorten_path(rest)), |s| s.green()));
                    rows.push((Shown::from(SEPARATOR), |s| s.dim()));
                    printed_separator = true;
                }
                DiffLine::Hunk(line) => rows.push((Shown::content(line), |s| s.cyan())),
                DiffLine::Removed(line) => rows.push((Shown::content(line), |s| s.red())),
                DiffLine::Added(line) => rows.push((Shown::content(line), |s| s.green())),
                DiffLine::Context(line) => rows.push((Shown::content(line), |s| s.dim())),
            }
        }
        if printed_separator {
            rows.push((Shown::from(SEPARATOR), |s| s.dim()));
        }

        // Written in one suspend, so no other line lands between two rows of
        // the diff and a spinner does not redraw between them.
        let colors = self.stdout_colors();
        let rendered: Vec<String> = rows
            .into_iter()
            .map(|(text, paint)| {
                let row = shown!("  {}", text.paint(self.use_colors, paint));
                guard(row.as_str(), colors).into_owned()
            })
            .collect();
        for row in &rendered {
            self.record_stdout(row);
        }
        self.draw(|| {
            for row in &rendered {
                println!("{row}");
            }
        });
    }

    // Write with nothing else drawing on the terminal, counting the writes
    // under test so that a test can see a block written as one.
    fn draw<R>(&self, write: impl FnOnce() -> R) -> R {
        #[cfg(test)]
        self.draws
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.mp.suspend(write)
    }

    /// Ask `prompt` on the terminal, with nothing else drawing on it until the
    /// user answers.
    ///
    /// Without a terminal on stderr nothing is drawn or read.
    ///
    /// # Errors
    ///
    /// [`PromptFailure`] when there is no terminal, the user pressed Ctrl+C, or
    /// the terminal failed. [`PromptFailure::exit`] says how the command ends.
    pub(crate) fn prompt<P: Prompt>(&self, prompt: P) -> Result<P::Answer, PromptFailure> {
        // Checked here and not left to dialoguer: `FuzzySelect` makes no check of
        // its own, and with no terminal it redraws its menu forever.
        if !self.can_prompt() {
            return Err(PromptFailure::NoTerminal);
        }
        // A run already canceled would only have its question drawn and taken
        // away again.
        if self.is_canceled() {
            return Err(PromptFailure::Interrupted);
        }
        // Suspended, not cleared: a spinner would redraw over the question while
        // the user types, but a step still running, or whose end is already on
        // its way, keeps its line and gets its ✓ or ✗ once the prompt returns.
        let answer = self
            .mp
            .suspend(|| self.ask(prompt))
            .map_err(PromptFailure::from);
        if let (Err(PromptFailure::Interrupted), Some(token)) = (&answer, &self.cancellation) {
            token.cancel();
        }
        answer
    }

    /// Whether a prompt can be asked: stderr, where dialoguer draws and reads
    /// through, is a terminal, and the display carries the run's token.
    pub(crate) fn can_prompt(&self) -> bool {
        // A test answers from its script and never reaches a terminal, so no
        // signal can leave one of its prompts waiting, and the many tests that
        // script answers need no token to do it.
        #[cfg(test)]
        {
            self.script.lock().is_ok_and(|script| !script.is_empty())
        }
        // Refused rather than asked: without the token a signal cannot end the
        // prompt, and the run hangs until a second one. The assert keeps that
        // refusal from passing as "no terminal" in any debug build.
        #[cfg(not(test))]
        {
            debug_assert!(
                !self.can_prompt || self.cancellation.is_some(),
                "a display that prompts on a terminal needs `with_cancellation`"
            );
            self.can_prompt && self.cancellation.is_some()
        }
    }

    /// Report a prompt that got no answer, and say how the command ends.
    ///
    /// `needs` names what needed the answer ("Removing a spec") and
    /// `without_terminal` is the way to do it without asking, shown as the
    /// remedy when there is no terminal. Ctrl+C prints nothing.
    pub(crate) fn refuse_prompt(
        &self,
        failure: &PromptFailure,
        needs: &str,
        without_terminal: &str,
    ) -> Exit {
        match failure {
            PromptFailure::NoTerminal => {
                self.print_error(format!("{needs} needs a terminal to ask on."));
                self.print_suggestion(without_terminal);
            }
            PromptFailure::Interrupted => {}
            PromptFailure::Unreadable(error) => self.print_error(unreadable_answer(error)),
        }
        failure.exit()
    }

    /// Report a prompt that got no answer when the command carries on without
    /// it: a terminal that failed is a warning, and no terminal or Ctrl+C prints
    /// nothing.
    pub(crate) fn note_unanswered_prompt(&self, failure: &PromptFailure) {
        if let PromptFailure::Unreadable(error) = failure {
            self.print_warning(unreadable_answer(error));
        }
    }

    #[cfg(not(test))]
    fn ask<P: Prompt>(&self, prompt: P) -> dialoguer::Result<P::Answer> {
        let answer = match &self.cancellation {
            Some(token) => wakeup::ask(token, || prompt.ask()),
            None => prompt.ask(),
        };
        answer.inspect_err(|error| {
            let stderr = console::Term::stderr();
            // dialoguer hides the cursor while it asks and shows it again only
            // after an answer.
            let _ = stderr.show_cursor();
            // Ctrl+C typed at the question leaves the cursor on its line. A
            // signal has already ended that line: main prints "Canceling..."
            // before it cancels the run.
            let dialoguer::Error::IO(error) = error;
            if error.kind() == std::io::ErrorKind::Interrupted && !self.is_canceled() {
                let _ = stderr.write_line("");
            }
        })
    }

    // A test never reaches a terminal: it answers from the script, which holds
    // what `ask` would have returned, errors included, so `prompt` converts a
    // scripted error exactly as it converts a real one.
    #[cfg(test)]
    fn ask<P: Prompt>(&self, _prompt: P) -> dialoguer::Result<P::Answer> {
        let effect = self
            .while_asking
            .lock()
            .ok()
            .and_then(|mut slot| slot.take());
        if let Some(effect) = effect {
            effect();
        }
        let next = self
            .script
            .lock()
            .ok()
            .and_then(|mut script| script.pop_front())
            .expect("can_prompt saw a scripted answer");
        next.map(|answer| {
            *answer
                .downcast::<P::Answer>()
                .expect("the scripted answer has the prompt's answer type")
        })
    }

    // Run `effect` while the next prompt is open, before its scripted answer, as
    // something the user does in another window while selfie waits.
    #[cfg(test)]
    pub(crate) fn on_next_prompt(self, effect: impl FnOnce() + Send + 'static) -> Self {
        if let Ok(mut slot) = self.while_asking.lock() {
            *slot = Some(Box::new(effect));
        }
        self
    }

    // Answer the next prompts from `answers`, in order. A prompt asked after the
    // last answer finds no terminal.
    #[cfg(test)]
    pub(crate) fn answering(self, answers: Vec<Scripted>) -> Self {
        if let Ok(mut script) = self.script.lock() {
            script.extend(answers);
        }
        self
    }

    /// Print a block of lines to stdout, its line breaks kept. selfie's own
    /// styling is kept when the run and stdout have colors on, and removed
    /// otherwise.
    pub(crate) fn println(&self, message: impl Into<Shown>) {
        let message = guard(message.into().as_str(), self.stdout_colors()).into_owned();
        self.record_stdout(&message);
        self.mp.suspend(|| println!("{message}"));
    }

    /// Print a block of plain lines to stderr, as [`println`](Self::println)
    /// prints them: its stderr twin, for the lines a diagnostic lists.
    pub(crate) fn print_note(&self, message: impl Into<Shown>) {
        let message = guard(message.into().as_str(), self.stderr_colors()).into_owned();
        self.record_stderr(&message);
        self.mp.suspend(|| eprintln!("{message}"));
    }

    /// Print a plain line on `channel`.
    pub(crate) fn line_to(&self, channel: Channel, message: impl Into<Shown>) {
        match channel {
            Channel::Stdout => self.println(message),
            Channel::Stderr => self.print_note(message),
        }
    }

    // Whether selfie's own styling is written to stdout: colors are on for the
    // run and for the stream itself.
    fn stdout_colors(&self) -> bool {
        self.use_colors && console::colors_enabled()
    }

    // As `stdout_colors`, for stderr.
    fn stderr_colors(&self) -> bool {
        self.use_colors && console::colors_enabled_stderr()
    }

    /// Print a styled key-value pair (stdout, for config display, etc.)
    pub(crate) fn print_field(&self, key: impl Display, value: impl Display) {
        let key = escape(&key.to_string()).into_owned();
        let value = escape(&value.to_string()).into_owned();
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
    fields: Vec<(&'static str, Shown)>,
}

impl<'a> ResultCard<'a> {
    fn new(display: &'a DisplayManager, title: impl Display) -> Self {
        Self {
            display,
            title: title.to_string(),
            fields: Vec::new(),
        }
    }

    /// Add a key-value field to the card. The value is shown escaped, a
    /// newline included, so it may come from outside selfie.
    pub(crate) fn field(mut self, key: &'static str, value: impl Display) -> Self {
        self.fields.push((key, Shown::name(&value.to_string())));
        self
    }

    /// Add a field only if the value is `Some`, shown as [`field`](Self::field)
    /// shows it.
    pub(crate) fn field_if(self, key: &'static str, value: Option<impl Display>) -> Self {
        match value {
            Some(value) => self.field(key, value),
            None => self,
        }
    }

    /// Add a field whose value selfie styled itself.
    pub(crate) fn styled_field(mut self, key: &'static str, value: Shown) -> Self {
        self.fields.push((key, value));
        self
    }

    /// Render the card to the display
    pub(crate) fn print(self) {
        use crate::formatters::format_key;

        self.display.println("");
        self.display.print_section_header(&self.title);

        let use_colors = self.display.use_colors();
        for (key, value) in self.fields {
            self.display.println(shown!(
                "{}{}: {}",
                INDENT,
                format_key(key, use_colors),
                value
            ));
        }
    }
}

// `text` from outside selfie, as a message printer shows it: escaped as prose,
// its line breaks kept, and every line after the first indented by `indent`
// columns so that no line of it starts at the left margin.
fn untrusted(text: impl Display, indent: usize) -> String {
    let text = text.to_string();
    let shown = escape_prose(text.trim_end_matches(['\r', '\n']));
    shown.replace('\n', &format!("\n{}", " ".repeat(indent)))
}

// Each line of a unified diff is classified by where it stands as well as by
// how it starts.
#[derive(Debug, PartialEq, Eq)]
enum DiffLine<'a> {
    // These two carry the file names, with their `--- ` and `+++ ` stripped.
    OldFile(&'a str),
    NewFile(&'a str),
    Hunk(&'a str),
    Removed(&'a str),
    Added(&'a str),
    Context(&'a str),
}

// The lines of `diff`, classified. Only lines before the first hunk can name a
// file: after it, a removed line whose content starts `-- ` reads `--- `, and
// an added one starting `++ ` reads `+++ `, and both are content.
fn diff_lines(diff: &str) -> impl Iterator<Item = DiffLine<'_>> {
    let mut in_hunks = false;
    diff.lines().map(move |line| {
        if line.starts_with("@@") {
            in_hunks = true;
            return DiffLine::Hunk(line);
        }
        if !in_hunks {
            if let Some(rest) = line.strip_prefix("--- ") {
                return DiffLine::OldFile(rest);
            }
            if let Some(rest) = line.strip_prefix("+++ ") {
                return DiffLine::NewFile(rest);
            }
        }
        if line.starts_with('-') {
            DiffLine::Removed(line)
        } else if line.starts_with('+') {
            DiffLine::Added(line)
        } else {
            DiffLine::Context(line)
        }
    })
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

// Ending a terminal prompt when a signal cancels the run.
//
// console waits for a key in select(2) on a macOS terminal and in poll(2)
// elsewhere. Neither restarts after a signal handler, so a handled signal
// delivered to the waiting thread ends the wait with EINTR. console then
// restores the terminal mode it saved and returns `Interrupted`, whichever
// signal it was. The kernel hands a process's signals to the main thread while
// it does not block them, and apply's conflict prompt waits on a
// blocking-pool thread, so the run's cancellation is carried to the prompting
// thread as SIGURG, the one handled signal it leaves unblocked.
mod wakeup {
    use std::{pin::pin, thread, time::Duration};

    use nix::sys::{
        pthread::{pthread_kill, pthread_self},
        signal::{SigSet, SigmaskHow, Signal},
    };
    use tokio_util::sync::CancellationToken;

    // Nothing selfie runs sends it, and its default action is to ignore, so
    // the handler `install` adds changes nothing else. Go's runtime preempts
    // goroutines with it for the same reasons.
    const WAKEUP: Signal = Signal::SIGURG;

    // Left deliverable on the prompting thread. The faults are raised on the
    // thread that caused them, and blocking one makes it fatal. The job-control
    // three have no handler here, so they cannot end the wait, and blocking
    // them would let selfie in the background change a terminal it does not
    // own instead of stopping.
    const UNBLOCKED: [Signal; 11] = [
        WAKEUP,
        Signal::SIGSEGV,
        Signal::SIGBUS,
        Signal::SIGFPE,
        Signal::SIGILL,
        Signal::SIGTRAP,
        Signal::SIGSYS,
        Signal::SIGABRT,
        Signal::SIGTSTP,
        Signal::SIGTTIN,
        Signal::SIGTTOU,
    ];

    // A wakeup that lands before console starts waiting meets a call that
    // restarts (a write, `tcsetattr`) and is lost, so it is repeated until one
    // lands in the wait.
    const RETRY: Duration = Duration::from_millis(10);

    /// Install the handler a wakeup needs: a signal left at its default of
    /// ignore is discarded and interrupts nothing.
    ///
    /// # Errors
    ///
    /// When the handler cannot be installed; a signal then cannot end a
    /// prompt.
    #[cfg_attr(test, expect(dead_code, reason = "a test never prompts on a terminal"))]
    pub(super) fn install() -> std::io::Result<()> {
        // tokio keeps a handler for the life of the process, so the stream it
        // returns can be dropped.
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::from_raw(WAKEUP as i32))
            .map(drop)
    }

    // Puts the thread's signal mask back when the prompt ends, a panic
    // included. A child inherits the mask of the thread that spawns it, so a
    // pooled thread left blocking SIGINT would pass that on to every command it
    // later runs.
    struct RestoreMask(SigSet);

    impl Drop for RestoreMask {
        fn drop(&mut self) {
            let _ = self.0.thread_set_mask();
        }
    }

    // Every signal but `UNBLOCKED`.
    fn blocked_while_asking() -> SigSet {
        let mut blocked = SigSet::all();
        for signal in UNBLOCKED {
            blocked.remove(signal);
        }
        blocked
    }

    /// Run `ask` on this thread, ending it as interrupted once `token` is
    /// canceled.
    pub(super) fn ask<T>(
        token: &CancellationToken,
        ask: impl FnOnce() -> dialoguer::Result<T>,
    ) -> dialoguer::Result<T> {
        // Blocked for the prompt, so any other handled signal (SIGINT, SIGTERM,
        // tokio's SIGCHLD) is handled on another thread rather than ending the
        // wait as a Ctrl+C would. The SIGINT console raises at Ctrl+C stays
        // pending here instead of reaching main's handler as a second signal,
        // which force-quits.
        let Ok(previous) = blocked_while_asking().thread_swap_mask(SigmaskHow::SIG_BLOCK) else {
            return ask();
        };
        let _restore = RestoreMask(previous);

        let prompting = pthread_self();
        let finished = CancellationToken::new();
        let answer = thread::scope(|scope| {
            scope.spawn(|| {
                futures::executor::block_on(futures::future::select(
                    pin!(token.cancelled()),
                    pin!(finished.cancelled()),
                ));
                while !finished.is_cancelled() {
                    let _ = pthread_kill(prompting, WAKEUP);
                    thread::sleep(RETRY);
                }
            });
            // Set on a panic too, or the scope would wait on the watcher forever.
            let _finished = finished.clone().drop_guard();
            ask()
        });

        if let Err(dialoguer::Error::IO(error)) = &answer
            && error.kind() == std::io::ErrorKind::Interrupted
        {
            // Taken here, so restoring the mask does not deliver the SIGINT
            // console raised. Raised once more first, so the wait finds one
            // whatever console did: standard signals do not queue.
            let _ = pthread_kill(prompting, Signal::SIGINT);
            let _ = SigSet::from(Signal::SIGINT).wait();
        }
        answer
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        // While it asks, the thread blocks every signal that has a handler
        // except the wakeup, and gets its own mask back afterwards.
        #[test]
        fn asking_blocks_every_handled_signal_but_the_wakeup() {
            let before = SigSet::thread_get_mask().unwrap();

            let during = ask(&CancellationToken::new(), || {
                Ok(SigSet::thread_get_mask().unwrap())
            })
            .unwrap();

            for signal in [
                Signal::SIGINT,
                Signal::SIGTERM,
                Signal::SIGCHLD,
                Signal::SIGWINCH,
                Signal::SIGHUP,
                Signal::SIGUSR1,
            ] {
                assert!(during.contains(signal), "{signal} reaches the prompt");
            }
            for signal in UNBLOCKED {
                assert!(!during.contains(signal), "{signal} is blocked");
            }
            assert_eq!(SigSet::thread_get_mask().unwrap(), before);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Each message printer shows what it is handed escaped as prose: a joiner
    // renders, and a line break is kept with the line after it indented under
    // the message, so no line of it starts at the left margin.
    #[test]
    fn every_message_printer_escapes_what_it_prints() {
        use selfie::package::event::Outcome;

        let text = "a\u{1b}[2J\u{202E}b\u{200D}\n✓ c\n";
        let shown = "a\\u{1b}[2J\\u{202e}b\u{200D}\n  ✓ c";
        let display = DisplayManager::new(false);
        display.print_info(text);
        display.print_error(text);
        display.print_result(Outcome::Clean, text);
        display.print_success(text);
        display.print_warning(text);
        display.print_progress(text);
        display.print_run_note(text);
        display.print_suggestion(text);

        let printed = display.printed();
        assert_eq!(printed.len(), 8, "{printed:?}");
        assert!(printed.iter().all(|(_, line)| line == shown), "{printed:?}");
    }

    // A message written with Windows line endings ends at its text, with no
    // carriage return shown after it.
    #[test]
    fn a_message_ending_in_a_carriage_return_shows_none() {
        let display = DisplayManager::new(false);
        display.print_info("one\r\n");

        assert_eq!(display.printed(), [(Channel::Stdout, "one".to_string())]);
    }

    // A section header is one line: a line break in its title is escaped.
    #[test]
    fn a_section_header_stays_on_one_line() {
        let display = DisplayManager::new(false);
        display.print_section_header("a\n✓ b");

        assert_eq!(
            display.printed(),
            [(Channel::Stdout, "a\\n✓ b".to_string())]
        );
    }

    // The plain-line printers keep line breaks and tabs, remove selfie's own
    // styling when colors are off, and escape every other sequence.
    #[test]
    fn the_plain_line_printers_escape_all_but_line_breaks() {
        let display = DisplayManager::new(false);
        display.println("one\u{1b}[31m\ntwo\u{202E}\t");
        display.print_note("three\u{1b}]0;t\u{7}");

        assert_eq!(
            display.printed(),
            [
                (Channel::Stdout, "one\ntwo\\u{202e}\t".to_string()),
                (Channel::Stderr, "three\\u{1b}]0;t\\u{7}".to_string()),
            ]
        );
    }

    // A run without colors prints no styling, whatever console decides about
    // the stream: the printers do not read the environment's color settings
    // alone.
    #[test]
    fn a_run_without_colors_prints_no_styling() {
        let display = DisplayManager::new(false);
        display.println(Shown::from("x").paint(true, |s| s.red()));

        assert_eq!(display.printed(), [(Channel::Stdout, "x".to_string())]);
    }

    // A waiting step's subject can name a command from a spec. It shows escaped
    // while the step runs and on the line it collapses to when it ends.
    #[test]
    fn a_waiting_subject_is_escaped() {
        let shown = "Running `printf \\u{1b}[2J`";
        let plain = DisplayManager::new(false);
        plain.start_waiting(ONE, "Running `printf \u{1b}[2J`");
        assert_eq!(plain.printed(), [(Channel::Stderr, format!("{shown}..."))]);

        let drawn = DisplayManager::new(false).drawing_as_a_terminal();
        drawn.start_waiting(ONE, "Running `printf \u{1b}[2J`");
        assert_eq!(drawn.waiting_messages(), [shown]);
        drawn.end_waiting(ONE, StepEnding::Succeeded);
        let ended = drawn.printed();
        assert!(
            ended
                .iter()
                .any(|(_, line)| line.starts_with(&format!("✓ {shown} ("))),
            "{ended:?}"
        );
    }

    // A diff is written in one draw, so no other line lands between its rows.
    #[test]
    fn a_diff_is_written_in_one_draw() {
        let display = DisplayManager::new(false);

        display.print_diff("--- a\n+++ b\n@@ -1 +1 @@\n-x\n+y\n");

        assert_eq!(display.draws.load(std::sync::atomic::Ordering::Relaxed), 1);
        assert_eq!(display.printed().len(), 7, "{:?}", display.printed());
    }

    // A content line that starts like a file header is content once the first
    // hunk has begun.
    #[test]
    fn a_diff_names_files_only_before_its_first_hunk() {
        let diff = "--- a\n+++ b\n@@ -1 +1 @@\n--- gone\n+++ added\n x\n";

        let lines: Vec<DiffLine<'_>> = diff_lines(diff).collect();

        assert_eq!(
            lines,
            [
                DiffLine::OldFile("a"),
                DiffLine::NewFile("b"),
                DiffLine::Hunk("@@ -1 +1 @@"),
                DiffLine::Removed("--- gone"),
                DiffLine::Added("+++ added"),
                DiffLine::Context(" x"),
            ]
        );
    }

    // Command output loses its colors, shows a bidi override as an escape, and
    // keeps a variation selector, which renders.
    #[test]
    fn command_output_shows_format_characters_as_escapes() {
        assert_eq!(printable("\u{1b}[31mred\u{1b}[0m"), "red");
        assert_eq!(printable("a\u{202E}b\u{FE0F}c"), "a\\u{202e}b\u{FE0F}c");
    }

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
                Channel::Stderr,
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
                Channel::Stderr,
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
                Channel::Stderr,
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

    // A prompt for the door tests; the script supplies its answer.
    struct Answered;

    impl Prompt for Answered {
        type Answer = ();

        fn ask(self) -> dialoguer::Result<()> {
            Ok(())
        }
    }

    fn io_error(kind: std::io::ErrorKind) -> Scripted {
        Err(std::io::Error::new(kind, "scripted").into())
    }

    // With no terminal the prompt is refused before anything is drawn, and the
    // command ends as a usage error.
    #[test]
    fn a_prompt_without_a_terminal_is_refused_as_usage() {
        let failure = DisplayManager::new(false).prompt(Answered).unwrap_err();

        assert!(matches!(failure, PromptFailure::NoTerminal), "{failure:?}");
        assert_eq!(failure.exit(), Exit::Usage);
    }

    #[test]
    fn ctrl_c_at_a_prompt_ends_the_command_canceled() {
        let dm =
            DisplayManager::new(false).answering(vec![io_error(std::io::ErrorKind::Interrupted)]);

        let failure = dm.prompt(Answered).unwrap_err();

        assert!(matches!(failure, PromptFailure::Interrupted), "{failure:?}");
        assert_eq!(failure.exit(), Exit::Canceled);
    }

    // Ctrl+C at a prompt cancels the whole run, not only the question; any
    // other answer leaves the run going.
    #[test]
    fn ctrl_c_at_a_prompt_cancels_the_run() {
        let token = CancellationToken::new();
        let dm = DisplayManager::new(false)
            .with_cancellation(token.clone())
            .answering(vec![io_error(std::io::ErrorKind::BrokenPipe), ctrl_c()]);

        dm.prompt(Answered).unwrap_err();
        assert!(!token.is_cancelled());
        dm.prompt(Answered).unwrap_err();
        assert!(token.is_cancelled());
    }

    // Once the run is canceled a prompt is not asked at all.
    #[test]
    fn a_prompt_after_the_run_is_canceled_is_interrupted_unasked() {
        let token = CancellationToken::new();
        let dm = DisplayManager::new(false)
            .with_cancellation(token.clone())
            .answering(vec![answer(())]);
        token.cancel();

        let failure = dm.prompt(Answered).unwrap_err();

        assert!(matches!(failure, PromptFailure::Interrupted), "{failure:?}");
        assert!(dm.can_prompt(), "the scripted answer was used up");
    }

    // dialoguer's own no-terminal refusal reads as no terminal; any other
    // terminal error is a failure, not a usage error.
    #[test]
    fn dialoguer_errors_convert_by_kind() {
        let dm = DisplayManager::new(false).answering(vec![
            io_error(std::io::ErrorKind::NotConnected),
            io_error(std::io::ErrorKind::BrokenPipe),
        ]);

        let first = dm.prompt(Answered).unwrap_err();
        let second = dm.prompt(Answered).unwrap_err();

        assert_eq!(first.exit(), Exit::Usage, "{first:?}");
        assert!(matches!(second, PromptFailure::Unreadable(_)), "{second:?}");
        assert_eq!(second.exit(), Exit::Failed);
    }

    // No terminal names what needed one and the way round it, both on stderr;
    // Ctrl+C adds nothing.
    #[test]
    fn refusing_a_prompt_reports_on_stderr_by_kind() {
        let dm = DisplayManager::new(false);

        let exit = dm.refuse_prompt(&PromptFailure::NoTerminal, "Removing a spec", "Pass --yes.");
        let canceled = dm.refuse_prompt(
            &PromptFailure::Interrupted,
            "Removing a spec",
            "Pass --yes.",
        );

        assert_eq!((exit, canceled), (Exit::Usage, Exit::Canceled));
        assert_eq!(
            dm.printed(),
            vec![
                (
                    Channel::Stderr,
                    "Removing a spec needs a terminal to ask on.".to_string()
                ),
                (Channel::Stderr, "Pass --yes.".to_string()),
            ]
        );
    }

    // A prompt suspends the spinners and keeps their steps: a step whose end
    // arrives after the prompt still prints its line.
    #[test]
    fn a_step_keeps_its_end_line_across_a_prompt() {
        let dm = DisplayManager::new(false)
            .drawing_as_a_terminal()
            .answering(vec![Ok(Box::new(()))]);
        dm.start_waiting(ONE, "Running the commands that produce ~/.creds");

        dm.prompt(Answered).unwrap();
        assert_eq!(
            dm.waiting_messages(),
            vec!["Running the commands that produce ~/.creds"]
        );
        dm.end_waiting(ONE, StepEnding::Succeeded);

        assert_eq!(
            dm.printed(),
            vec![(
                Channel::Stderr,
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
                Channel::Stderr,
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
                Channel::Stderr,
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

    // A canceled step is cleared with no result line.
    #[test]
    fn a_canceled_step_prints_nothing() {
        let dm = DisplayManager::new(false).drawing_as_a_terminal();
        dm.start_waiting(ONE, "Running the install command for a");

        dm.end_waiting(ONE, StepEnding::Canceled);

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
            vec![(Channel::Stderr, "Loading specs...".to_string())]
        );
    }
}
