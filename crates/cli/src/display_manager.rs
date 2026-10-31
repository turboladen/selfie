//! indicatif-powered display layer for CLI output
//!
//! Provides spinners, progress tracking, styled output, and structured error
//! summaries. Replaces `TerminalProgressReporter` with a unified display system
//! that handles both event-driven and static output consistently.

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use indicatif::{MultiProgress, ProgressBar, ProgressDrawTarget, ProgressStyle};
use selfie::package::event::{BaseKind, PlannedEntry, StepEnding, StepId};
use tokio_util::sync::CancellationToken;

use crate::entry_rows::{EntryColumns, EntryRow, entry_line};
use crate::event_processor::Exit;
use crate::source_paths::relative_text;
pub(crate) use crate::terminal_text::DisplayPath;
use crate::terminal_text::{
    Command, Content, ESCAPED, Literal, Name, Output, Paint, Prose, Shown, guard, lit, shown,
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
pub(crate) const INDENT: Literal = lit!("   ");

/// A terminal prompt [`DisplayManager::prompt`] can ask.
///
/// Implemented for each dialoguer prompt the CLI uses. The call that reads the
/// answer happens inside `prompt`, so nothing else can draw on the terminal
/// while the user answers.
// Private, with `prompt`, so a widget built anywhere else cannot be asked.
trait Prompt {
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

/// One entry of a menu: the text it shows, and the value picking it returns.
pub(crate) struct Choice<T> {
    label: Shown,
    value: T,
}

impl<T> Choice<T> {
    /// A choice showing `name`, escaped as a [`Name`]. Picking it returns
    /// `value`, which may hold the raw name.
    pub(crate) fn named(name: &str, value: T) -> Self {
        Self {
            label: Name::new(name).into(),
            value,
        }
    }

    /// A choice showing selfie's own `label`.
    pub(crate) fn labeled(label: Literal, value: T) -> Self {
        Self {
            label: label.into(),
            value,
        }
    }
}

/// What an [`input`](DisplayManager::input) prompt suggests.
///
/// Either kind of suggestion is pre-filled when it shows as written, and the
/// user's edit of it is the answer. The two differ only for a suggestion that
/// can only be shown escaped, which is noted above an empty prompt instead.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Suggestion {
    /// Nothing is suggested.
    Nothing,
    /// Noted escaped, Enter on the empty prompt returns its bytes.
    Keepable(String),
    /// Noted escaped, its bytes are never returned: escaping changed them,
    /// so they could not be a valid answer.
    Unkeepable(String),
}

/// What an [`input`](DisplayManager::input) prompt does with a blank answer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Blank {
    /// Returned, for the caller to say what it means.
    Allowed,
    /// Refused with this warning, such as "Install command cannot be
    /// empty.", and asked again. An answer is blank when nothing in it shows,
    /// as [`Name::is_blank`] decides; a kept suggestion that is blank is
    /// refused too.
    Refused(Literal),
}

// How `input` offers a suggestion: pre-filled when it shows as written,
// otherwise escaped in a note above an empty prompt, with the bytes Enter
// keeps when they could be an answer. No prompt pre-fills escaped text.
#[derive(Debug, PartialEq, Eq)]
enum Offer<'a> {
    Bare,
    Prefill(&'a str),
    Note { shown: Name, keep: Option<&'a str> },
}

fn offer(suggested: &Suggestion) -> Offer<'_> {
    let (text, keepable) = match suggested {
        Suggestion::Nothing => return Offer::Bare,
        Suggestion::Keepable(text) => (text, true),
        Suggestion::Unkeepable(text) => (text, false),
    };
    if Name::is_plain(text) {
        Offer::Prefill(text)
    } else {
        Offer::Note {
            shown: Name::new(text),
            keep: keepable.then_some(text.as_str()),
        }
    }
}

// Which dialoguer theme a prompt draws with. An enum so a test can read the
// decision without drawing.
#[derive(Debug, PartialEq, Eq)]
enum PromptTheme {
    Colorful,
    Simple,
}

// The one place dialoguer's widgets are built and asked; crates/cli/clippy.toml
// forbids their constructors and reading calls everywhere else, so a prompt
// that bypasses these methods does not build.
#[expect(
    clippy::disallowed_methods,
    reason = "every prompt is built here and asked inside DisplayManager::prompt"
)]
mod prompts {
    use dialoguer::theme::{ColorfulTheme, SimpleTheme, Theme};

    use super::{
        Blank, Choice, DisplayManager, Name, Offer, Prompt, PromptFailure, PromptTheme, Shown,
        Suggestion, lit, offer, shown,
    };

    impl DisplayManager {
        /// Ask a yes-or-no question; Enter alone answers `default`.
        ///
        /// # Errors
        ///
        /// [`PromptFailure`] when the prompt got no answer.
        pub(crate) fn confirm(
            &self,
            text: impl Into<Shown>,
            default: bool,
        ) -> Result<bool, PromptFailure> {
            let theme = self.theme();
            self.prompt(
                dialoguer::Confirm::with_theme(theme.as_ref())
                    .with_prompt(self.prompt_text(text))
                    .default(default),
            )
        }

        /// Ask for one of `choices`, the first highlighted, and return its
        /// value.
        ///
        /// # Errors
        ///
        /// [`PromptFailure`] when the prompt got no answer.
        pub(crate) fn select<T: Clone>(
            &self,
            text: impl Into<Shown>,
            choices: &[Choice<T>],
        ) -> Result<T, PromptFailure> {
            let theme = self.theme();
            let picked = self.prompt(
                dialoguer::Select::with_theme(theme.as_ref())
                    .with_prompt(self.prompt_text(text))
                    .items(self.labels(choices))
                    .default(0),
            )?;
            Ok(choices[picked].value.clone())
        }

        /// Ask for one of `choices`, filtered as the user types, and return
        /// its value. `None` when the user pressed Esc.
        ///
        /// # Errors
        ///
        /// [`PromptFailure`] when the prompt got no answer.
        pub(crate) fn fuzzy_select<T: Clone>(
            &self,
            text: impl Into<Shown>,
            choices: &[Choice<T>],
        ) -> Result<Option<T>, PromptFailure> {
            let theme = self.theme();
            let picked = self.prompt(
                dialoguer::FuzzySelect::with_theme(theme.as_ref())
                    .with_prompt(self.prompt_text(text))
                    .items(self.labels(choices))
                    .highlight_matches(self.fuzzy_highlights())
                    .default(0),
            )?;
            Ok(picked.map(|picked| choices[picked].value.clone()))
        }

        /// Ask for any number of `choices`, none checked at first, and return
        /// the values of those picked, in the order of `choices`.
        ///
        /// # Errors
        ///
        /// [`PromptFailure`] when the prompt got no answer.
        pub(crate) fn multi_select<T: Clone>(
            &self,
            text: impl Into<Shown>,
            choices: &[Choice<T>],
        ) -> Result<Vec<T>, PromptFailure> {
            let theme = self.theme();
            let picked = self.prompt(
                dialoguer::MultiSelect::with_theme(theme.as_ref())
                    .with_prompt(self.prompt_text(text))
                    .items(self.labels(choices)),
            )?;
            Ok(picked
                .into_iter()
                .map(|picked| choices[picked].value.clone())
                .collect())
        }

        /// Ask for a line of text, offering `suggested` as its rules say, and
        /// return what the user typed. An empty answer returns a suggestion
        /// that could only be shown escaped, when it is keepable. Otherwise a
        /// blank answer is returned or refused as `blank` says.
        ///
        /// # Errors
        ///
        /// [`PromptFailure`] when the prompt got no answer.
        pub(crate) fn input(
            &self,
            text: impl Into<Shown>,
            suggested: Suggestion,
            blank: Blank,
        ) -> Result<String, PromptFailure> {
            let text = self.prompt_text(text);
            let offered = offer(&suggested);
            loop {
                let theme = self.theme();
                // dialoguer's own empty check would re-ask silently, and never
                // under a test's script, so a blank is judged below.
                let mut widget = dialoguer::Input::<String>::with_theme(theme.as_ref())
                    .with_prompt(text.clone())
                    .allow_empty(true);
                let (note, kept) = match &offered {
                    Offer::Bare => (None, None),
                    Offer::Prefill(text) => {
                        widget = widget.with_initial_text(*text);
                        (None, None)
                    }
                    Offer::Note { shown, keep } => {
                        let hint = match keep {
                            Some(_) => lit!("Enter keeps it"),
                            None => lit!("shown escaped; type the answer"),
                        };
                        (Some(shown!("  Suggested: {} ({})", shown, hint)), *keep)
                    }
                };
                let typed = self.prompt_under(note, widget)?;
                let answer = match kept {
                    Some(kept) if typed.is_empty() => kept.to_string(),
                    _ => typed,
                };
                if let Blank::Refused(warning) = blank
                    && Name::is_blank(&answer)
                {
                    self.print_warning(warning);
                    continue;
                }
                return Ok(answer);
            }
        }

        // Prompts draw on stderr, so they follow its color decision.
        pub(super) fn prompt_theme(&self) -> PromptTheme {
            if self.colors.stderr {
                PromptTheme::Colorful
            } else {
                PromptTheme::Simple
            }
        }

        fn theme(&self) -> Box<dyn Theme> {
            match self.prompt_theme() {
                PromptTheme::Colorful => Box::new(colorful_theme()),
                PromptTheme::Simple => Box::new(SimpleTheme),
            }
        }

        // Whether a fuzzy select bolds the characters the filter matched.
        // SimpleTheme bolds them through console's own answer for stderr, so
        // they are left plain where this display gives stderr no colors.
        pub(super) fn fuzzy_highlights(&self) -> bool {
            self.colors.stderr
        }
    }

    // ColorfulTheme, its styles written whatever console decides for stderr.
    // Its defaults defer to that decision, and the theme is chosen only when
    // the display gives stderr colors.
    pub(super) fn colorful_theme() -> ColorfulTheme {
        let mut theme = ColorfulTheme::default();
        theme.defaults_style = theme.defaults_style.force_styling(true);
        theme.prompt_style = theme.prompt_style.force_styling(true);
        theme.prompt_prefix = theme.prompt_prefix.force_styling(true);
        theme.prompt_suffix = theme.prompt_suffix.force_styling(true);
        theme.success_prefix = theme.success_prefix.force_styling(true);
        theme.success_suffix = theme.success_suffix.force_styling(true);
        theme.error_prefix = theme.error_prefix.force_styling(true);
        theme.error_style = theme.error_style.force_styling(true);
        theme.hint_style = theme.hint_style.force_styling(true);
        theme.values_style = theme.values_style.force_styling(true);
        theme.active_item_style = theme.active_item_style.force_styling(true);
        theme.inactive_item_style = theme.inactive_item_style.force_styling(true);
        theme.active_item_prefix = theme.active_item_prefix.force_styling(true);
        theme.inactive_item_prefix = theme.inactive_item_prefix.force_styling(true);
        theme.checked_item_prefix = theme.checked_item_prefix.force_styling(true);
        theme.unchecked_item_prefix = theme.unchecked_item_prefix.force_styling(true);
        theme.picked_item_prefix = theme.picked_item_prefix.force_styling(true);
        theme.unpicked_item_prefix = theme.unpicked_item_prefix.force_styling(true);
        theme.fuzzy_cursor_style = theme.fuzzy_cursor_style.force_styling(true);
        theme.fuzzy_match_highlight_style = theme.fuzzy_match_highlight_style.force_styling(true);
        theme
    }

    impl DisplayManager {
        fn prompt_text(&self, text: impl Into<Shown>) -> String {
            text.into().guarded(self.colors.stderr).into_string()
        }

        fn labels<T>(&self, choices: &[Choice<T>]) -> Vec<String> {
            choices
                .iter()
                .map(|choice| self.prompt_text(choice.label.clone()))
                .collect()
        }
    }

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

    // `interact_text`: line-editing keys, and no ASCII control typed into
    // the answer.
    impl Prompt for dialoguer::Input<'_, String> {
        type Answer = String;
        fn ask(self) -> dialoguer::Result<String> {
            self.interact_text()
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
fn unreadable_answer(error: &std::io::Error) -> Shown {
    shown!(
        "Could not read the answer: {}",
        crate::received::sentence(error)
    )
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
    pub package_name: Name,
    pub operation: Prose,
    pub command: Option<Command>,
    pub exit_code: Option<i32>,
    /// Bounded by the library: the only producer copies it out of
    /// `CommandFailure::ExecutionFailed`, whose field is a `BoundedText` and so
    /// cannot hold unbounded text. There is deliberately no `stdout`
    /// counterpart: see `CommandFailure::ExecutionFailed`.
    pub stderr: Option<Vec<Output>>,
    pub message: Shown,
}

impl ErrorDetail {
    /// The detail of `operation`'s failure, worded by `message`, with no
    /// command.
    pub(crate) fn of(
        operation: &selfie::package::event::OperationInfo,
        message: impl Into<Shown>,
    ) -> Self {
        let operation = crate::received::Operation::from(operation);
        Self {
            package_name: operation.package,
            operation: operation.kind,
            command: None,
            exit_code: None,
            stderr: None,
            message: message.into(),
        }
    }
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

    /// Format and return the error summary.
    ///
    /// Returns `None` if fewer than 2 errors were collected, since single
    /// errors are already shown inline by the event processor.
    pub(crate) fn format_summary(&self) -> Option<Shown> {
        if self.errors.len() <= 1 {
            return None;
        }

        let mut lines: Vec<Shown> = vec![
            lit!("").into(),
            lit!("── Errors ─────────────────────────────────────").into(),
        ];

        for error in &self.errors {
            lines.push(shown!("✗ {} ({})", error.package_name, error.operation));
            if !error.message.is_empty() {
                lines.push(led(&Shown::from(lit!("  ")), error.message.clone()));
            }
            // Each starts its continuation lines under its text, so a
            // multi-line command keeps its shape and its note matches it.
            if let Some(command) = &error.command {
                const COMMAND: Literal = lit!("Command");
                let [command_pad, escaped_pad] = aligned_pads(COMMAND, ESCAPED);
                lines.push(led(
                    &shown!("  {}: {}", COMMAND, spaces(command_pad)),
                    command.clone(),
                ));
                if let Some(note) = command.note() {
                    lines.push(led(&shown!("  {}: {}", ESCAPED, spaces(escaped_pad)), note));
                }
            }
            if let Some(code) = error.exit_code {
                lines.push(shown!("  Exit code: {}", code));
            }
            // A tool's own stderr, cleaned as command output is: its colors
            // stripped, not shown as escapes.
            let blank = |line: &&Output| line.as_str().trim().is_empty();
            let stderr: Vec<&Output> = error.stderr.iter().flatten().skip_while(blank).collect();
            let end = stderr
                .iter()
                .rposition(|line| !blank(line))
                .map_or(0, |at| at + 1);
            if end > 0 {
                lines.push(lit!("  stderr:").into());
                for line in &stderr[..end] {
                    lines.push(shown!("    {}", *line));
                }
            }
            lines.push(lit!("").into());
        }

        lines.push(lit!("───────────────────────────────────────────────").into());
        Some(Shown::join(lines, lit!("\n")))
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
    subject: Shown,
    started: Instant,
    tail: VecDeque<Output>,
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
    colors: StreamColors,
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
    failed_tail: Arc<Mutex<Vec<Output>>>,
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
    // Each line a lead and its text were written as, styling removed, so a test
    // can compare the columns of two lines as the terminal shows them.
    #[cfg(test)]
    written: Arc<Mutex<Vec<String>>>,
    // How many times `draw` wrote, so a test can see a block written as one.
    #[cfg(test)]
    draws: Arc<std::sync::atomic::AtomicUsize>,
    // Each line exactly as it reached its stream, styling included, so a test
    // can see which stream's colors a printer followed.
    #[cfg(test)]
    emitted: Arc<Mutex<Vec<(Channel, String)>>>,
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

/// Whether selfie's own styling reaches each stream.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct StreamColors {
    stdout: bool,
    stderr: bool,
}

impl StreamColors {
    fn of(self, channel: Channel) -> bool {
        match channel {
            Channel::Stdout => self.stdout,
            Channel::Stderr => self.stderr,
        }
    }

    // Give `channel` colors (test-only).
    #[cfg(test)]
    fn give(&mut self, channel: Channel) {
        match channel {
            Channel::Stdout => self.stdout = true,
            Channel::Stderr => self.stderr = true,
        }
    }
}

// A stream gets colors when the run has them on and console would give that
// stream colors. console gives them to a stream when `CLICOLOR_FORCE` is set to
// anything but `0`, `NO_COLOR` included; otherwise to a terminal, unless
// `NO_COLOR` is set to any value, `CLICOLOR` is `0` or `TERM` is `dumb` or unset.
fn decided(use_colors: bool, console: StreamColors) -> StreamColors {
    StreamColors {
        stdout: use_colors && console.stdout,
        stderr: use_colors && console.stderr,
    }
}

// What console decides for each stream. Read, never set: console's answer is
// process-wide, and each display decides for itself. A test never probes, so
// a suite run from a terminal reads the same colors as one run from CI.
fn console_colors() -> StreamColors {
    if cfg!(test) {
        StreamColors::default()
    } else {
        StreamColors {
            stdout: console::colors_enabled(),
            stderr: console::colors_enabled_stderr(),
        }
    }
}

/// Whether stderr gets selfie's styling in a run whose colors are
/// `use_colors`, as a display built for that run decides, for a writer that is
/// not a display, such as the verbose log.
pub(crate) fn stderr_colors(use_colors: bool) -> bool {
    decided(use_colors, console_colors()).stderr
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
        let mp = if draws_spinner {
            MultiProgress::new()
        } else {
            let mp = MultiProgress::new();
            mp.set_draw_target(ProgressDrawTarget::hidden());
            mp
        };

        Self {
            mp,
            colors: decided(use_colors, console_colors()),
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
            written: Arc::new(Mutex::new(Vec::new())),
            #[cfg(test)]
            draws: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            #[cfg(test)]
            emitted: Arc::new(Mutex::new(Vec::new())),
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
            self.print_warning(shown!(
                "A signal cannot end a prompt in this run, so it takes a second one to stop \
                 selfie while it asks: {}",
                crate::received::sentence(error)
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

    // Whether selfie's own styling is written to `channel`.
    fn colors(&self, channel: Channel) -> bool {
        self.colors.of(channel)
    }

    // Give `channel` colors, as though console had said that stream gets
    // them, so a test can see the styling a printer writes there.
    #[cfg(test)]
    pub(crate) fn colored(mut self, channel: Channel) -> Self {
        self.colors.give(channel);
        self
    }

    // Every line as it reached its stream, styling included, in order.
    #[cfg(test)]
    pub(crate) fn emitted(&self) -> Vec<(Channel, String)> {
        self.emitted
            .lock()
            .map(|emitted| emitted.clone())
            .unwrap_or_default()
    }

    #[cfg(test)]
    fn record(&self, stream: Channel, line: &str) {
        if let Ok(mut printed) = self.printed.lock() {
            printed.push((stream, line.to_string()));
        }
    }

    // Record `text` as `channel` gets it with its styling removed. Outside a
    // test this does nothing, so it costs nothing.
    #[cfg(test)]
    fn record_plain(&self, channel: Channel, text: &Shown) {
        self.record(channel, &guard(text.as_str(), false));
    }

    #[cfg(not(test))]
    fn record_plain(&self, _channel: Channel, _text: &Shown) {}

    // Every line a lead and its text were written as, styling removed, in order.
    #[cfg(test)]
    pub(crate) fn written(&self) -> Vec<String> {
        self.written
            .lock()
            .map(|written| written.clone())
            .unwrap_or_default()
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
    pub(crate) fn start_waiting(&self, step: StepId, subject: Name) {
        let subject = Shown::from(subject);
        let bar = if self.draws_spinner {
            Some(self.spinner(&subject))
        } else {
            self.print_progress(shown!("{}...", subject));
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
            let mark: Option<(Literal, Paint)> = match ending {
                StepEnding::Succeeded => Some((lit!("✓"), |s| s.green())),
                StepEnding::Failed => Some((lit!("✗"), |s| s.red())),
                // The refusal or cancellation that follows says why; a mark
                // would claim an outcome for commands that did not finish or start.
                StepEnding::Canceled | StepEnding::NotRun => None,
            };
            if let Some((mark, paint)) = mark {
                let text = shown!(
                    "{} ({})",
                    line.subject,
                    Name::new(&elapsed(line.started.elapsed()))
                );
                let rendered = shown!(
                    "{} {}",
                    Shown::from(mark).painted(paint),
                    text.painted(|s| s.dim())
                );
                self.put_above_spinners(&rendered);
            }
        }
        if ending == StepEnding::Failed && !line.tail.is_empty() {
            // Under `--verbose` the lines were printed as they came.
            if !self.is_verbose() {
                self.print_error_context(Shown::join(line.tail.iter().cloned(), lit!("\n")));
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
        let shown: Vec<(Shown, DisplayPath)> = entries
            .iter()
            .map(|entry| {
                (
                    relative_text(&entry.source).text(),
                    shorten_path(&entry.target),
                )
            })
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
        self.println(entry_line(row, columns));
    }

    /// Hold every clone's printing of source lines until the guard drops, so
    /// a heading and the lines under it print together. Not reentrant: a
    /// holder must not ask again.
    pub(crate) fn hold_block(&self) -> std::sync::MutexGuard<'_, ()> {
        self.block
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Print a failed command's stderr, one [`Output`] per line, as context for
    /// its error (stderr), leaving out the lines its step already showed when
    /// it ended.
    pub(crate) fn print_command_stderr(&self, stderr: &[Output]) {
        let shown = self
            .failed_tail
            .lock()
            .map(|mut failed| std::mem::take(&mut *failed))
            .unwrap_or_default();
        let rest: Vec<&Output> = stderr.iter().filter(|line| !shown.contains(line)).collect();
        if rest.iter().any(|line| !line.as_str().trim().is_empty()) {
            self.print_error_context(Shown::join(rest.into_iter().cloned(), lit!("\n")));
        }
    }

    /// Render one line of the output of the command step `step` runs (stderr).
    ///
    /// At normal verbosity it becomes that step's spinner message on a
    /// terminal, and is not shown otherwise. Under `--verbose` every line is
    /// printed: indented and dimmed on a terminal, and prefixed with what the
    /// step waits on without one.
    pub(crate) fn command_output(&self, step: StepId, output: Output) {
        let line = output.as_str();
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
        // Only a `--verbose` line names its step.
        let subject = if self.is_verbose() {
            current
                .as_ref()
                .map(|w| w.subject.clone())
                .unwrap_or_default()
        } else {
            Shown::default()
        };
        if let Some(current) = current {
            if current.tail.len() == TAIL_LINES {
                current.tail.pop_front();
            }
            current.tail.push_back(output.clone());
            if !self.is_verbose()
                && let Some(bar) = &current.bar
            {
                bar.set_message(fit_to_terminal(line));
            }
        }
        drop(waiting);
        if !self.is_verbose() {
            return;
        }
        let rendered = if self.draws_spinner {
            let text = if concurrent {
                shown!("{} │ {}", subject, output)
            } else {
                Shown::from(output)
            };
            shown!("    {}", text.painted(|s| s.dim()))
        } else {
            shown!("  {} │ {}", subject, output)
        };
        self.draw(|| self.put(Channel::Stderr, &rendered));
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
        let bar = self.mp.add(ProgressBar::new_spinner());
        bar.set_style(
            ProgressStyle::with_template(self.spinner_template())
                .unwrap()
                .tick_strings(&["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"]),
        );
        bar.set_message(message.to_string());
        bar.enable_steady_tick(Duration::from_millis(80));
        bar
    }

    // The spinner draws on stderr, so its template follows stderr's colors.
    // indicatif applies a template's style through console's own answer for
    // stderr, and the plain template gives it no style to apply.
    fn spinner_template(&self) -> &'static str {
        if self.colors.stderr {
            "{spinner:.cyan} {msg}"
        } else {
            "{spinner} {msg}"
        }
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
    pub(crate) fn print_info(&self, message: impl Into<Shown>) {
        self.print_marked(
            Channel::Stdout,
            lit!("ℹ"),
            |s| s.blue(),
            |s| s.blue(),
            message,
        );
    }

    /// Print an error message (stderr)
    pub(crate) fn print_error(&self, message: impl Into<Shown>) {
        self.print_marked(
            Channel::Stderr,
            lit!("✗"),
            |s| s.red().bold(),
            |s| s.red().bold(),
            message,
        );
    }

    /// Print supporting lines under an error (stderr), one per line.
    ///
    /// For a block a reader looks at rather than reads — a source window under a
    /// parse failure. Each line is indented and dimmed and carries no `✗`; the
    /// marker belongs to the sentence above, not to every line of its evidence.
    pub(crate) fn print_error_context(&self, block: impl Into<Shown>) {
        // Its own styling is written below, so the block's is removed.
        let lines: Vec<Shown> = block.into().guarded(false).split_lines().collect();
        self.draw(|| {
            for line in &lines {
                // A test reads the line back without the indent.
                self.record_plain(Channel::Stderr, line);
                self.write(
                    Channel::Stderr,
                    &shown!("  {}", line.clone().painted(|s| s.dim())),
                );
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
        message: impl Into<Shown>,
    ) {
        use selfie::package::event::Outcome;

        let (mark, mark_paint, paint): (Literal, Paint, Paint) = match outcome {
            Outcome::Clean => (lit!("✓"), |s| s.green().bold(), |s| s.green()),
            Outcome::Found => (lit!("⚠"), |s| s.yellow().bold(), |s| s.yellow()),
            Outcome::Failed => (lit!("✗"), |s| s.red().bold(), |s| s.red()),
        };
        self.print_marked(Channel::Stdout, mark, mark_paint, paint, message);
    }

    /// Print a success message (stdout)
    pub(crate) fn print_success(&self, message: impl Into<Shown>) {
        self.print_marked(
            Channel::Stdout,
            lit!("✓"),
            |s| s.green().bold(),
            |s| s.green(),
            message,
        );
    }

    /// Print a warning message (stderr)
    pub(crate) fn print_warning(&self, message: impl Into<Shown>) {
        self.print_marked(
            Channel::Stderr,
            lit!("⚠"),
            |s| s.yellow().bold(),
            |s| s.yellow().bold(),
            message,
        );
    }

    /// Print a line about the run's progress (stderr).
    pub(crate) fn print_progress(&self, message: impl Into<Shown>) {
        let text = message.into().painted(|s| s.dim());
        self.print_led(
            Channel::Stderr,
            Shown::from(lit!("  ")),
            Shown::default(),
            text,
        );
    }

    /// Print a line saying what the command is doing (stderr), only under
    /// [`Verbosity::Verbose`].
    ///
    /// Use it for work selfie does itself. A step that waits on something
    /// outside selfie uses [`start_waiting`](Self::start_waiting), which shows
    /// at every verbosity.
    pub(crate) fn print_status(&self, message: impl Into<Shown>) {
        if self.is_verbose() {
            self.print_progress(message);
        }
    }

    /// Print a note about the run (stderr), such as the operation header.
    pub(crate) fn print_run_note(&self, message: impl Into<Shown>) {
        self.print_marked(
            Channel::Stderr,
            lit!("ℹ"),
            |s| s.blue(),
            |s| s.blue(),
            message,
        );
    }

    // `message`, escaped and painted with `paint`, after `mark` painted with
    // `mark_paint` and a space.
    fn print_marked(
        &self,
        channel: Channel,
        mark: Literal,
        mark_paint: Paint,
        paint: Paint,
        message: impl Into<Shown>,
    ) {
        // The space stays outside the mark's styling.
        let mark = shown!("{} ", Shown::from(mark).painted(mark_paint));
        let text = message.into().painted(paint);
        self.print_led(channel, mark, Shown::default(), text);
    }

    /// Print `lead` and then `text` (stdout), the lines after the first of
    /// `text` starting under its first character.
    pub(crate) fn println_after(&self, lead: Shown, text: impl Into<Shown>) {
        self.print_led(Channel::Stdout, Shown::default(), lead, text);
    }

    // Print `mark`, `lead` and then `text` on `channel`, the text's
    // continuation lines under its first character. What a test reads back is
    // the line without `mark`, as the stream gets it with its styling removed.
    fn print_led(&self, channel: Channel, mark: Shown, lead: Shown, text: impl Into<Shown>) {
        let line = led(&shown!("{}{}", mark, lead), text);
        #[cfg(test)]
        self.record_led(channel, &mark, &line);
        self.draw(|| self.write(channel, &line));
    }

    // Record `line` as `print_led` wrote it, styling removed: whole in
    // `written`, and without `mark` in `printed`.
    #[cfg(test)]
    fn record_led(&self, channel: Channel, mark: &Shown, line: &Shown) {
        let plain = guard(line.as_str(), false);
        let plain_mark = guard(mark.as_str(), false);
        let recorded = plain.strip_prefix(plain_mark.as_ref()).unwrap_or(&plain);
        self.record(channel, recorded);
        if let Ok(mut written) = self.written.lock() {
            written.push(plain.to_string());
        }
    }

    /// Print a suggestion (stderr): advice about the run, never the answer.
    pub(crate) fn print_suggestion(&self, message: impl Into<Shown>) {
        let mark = shown!(
            "{} {}: ",
            Shown::from(lit!("✨")).painted(|s| s.bold()),
            Shown::from(lit!("Suggestion")).painted(|s| s.yellow().bold())
        );
        self.print_led(Channel::Stderr, mark, Shown::default(), message);
    }

    /// Print a section header (stdout)
    pub(crate) fn print_section_header(&self, title: impl Into<Shown>) {
        self.print_section_header_to(Channel::Stdout, title);
    }

    /// Print a section header on `channel`.
    pub(crate) fn print_section_header_to(&self, channel: Channel, title: impl Into<Shown>) {
        // Its own styling is written below, so the title's is removed.
        let title = title.into().guarded(false);
        // A test reads the title back without its rules.
        self.record_plain(channel, &title);
        let rendered = shown!("── {} ──", title.painted(|s| s.bold()));
        self.draw(|| self.write(channel, &rendered));
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
    pub(crate) fn print_diff(&self, diff: Diff) {
        // Written in one suspend, so no other line lands between two rows of
        // the diff and a spinner does not redraw between them.
        let rendered: Vec<Shown> = diff
            .0
            .into_iter()
            .map(|(text, paint)| shown!("  {}", text.painted(paint)))
            .collect();
        self.draw(|| {
            for row in &rendered {
                self.put(Channel::Stdout, row);
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
    fn prompt<P: Prompt>(&self, prompt: P) -> Result<P::Answer, PromptFailure> {
        self.prompt_under(None, prompt)
    }

    // As `prompt`, with `note` printed on stderr just above the question, in
    // the same suspension, so no spinner draws over it. The note is written
    // only once the prompt is ready, so none is left above a question never
    // asked.
    fn prompt_under<P: Prompt>(
        &self,
        note: Option<Shown>,
        prompt: P,
    ) -> Result<P::Answer, PromptFailure> {
        self.ready()?;

        // Suspended, not cleared: a spinner would redraw over the question while
        // the user types, but a step still running, or whose end is already on
        // its way, keeps its line and gets its ✓ or ✗ once the prompt returns.
        let answer = self
            .draw(|| {
                if let Some(note) = &note {
                    self.put(Channel::Stderr, note);
                }
                self.ask(prompt)
            })
            .map_err(PromptFailure::from);
        if let (Err(PromptFailure::Interrupted), Some(token)) = (&answer, &self.cancellation) {
            token.cancel();
        }
        answer
    }

    // Whether a prompt may be asked now: there is a terminal, and the run is
    // not canceled.
    fn ready(&self) -> Result<(), PromptFailure> {
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
        Ok(())
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
        needs: Literal,
        without_terminal: impl Into<Shown>,
    ) -> Exit {
        match failure {
            PromptFailure::NoTerminal => {
                self.print_error(shown!("{} needs a terminal to ask on.", needs));
                self.print_suggestion(without_terminal);
            }
            PromptFailure::Interrupted => {}
            PromptFailure::Unreadable(error) => {
                self.print_error(unreadable_answer(error));
            }
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
        let message = message.into();
        self.draw(|| self.put(Channel::Stdout, &message));
    }

    /// Print a block of plain lines to stderr, as [`println`](Self::println)
    /// prints them: its stderr twin, for the lines a diagnostic lists.
    pub(crate) fn print_note(&self, message: impl Into<Shown>) {
        let message = message.into();
        self.draw(|| self.put(Channel::Stderr, &message));
    }

    /// Print a plain line on `channel`.
    pub(crate) fn line_to(&self, channel: Channel, message: impl Into<Shown>) {
        match channel {
            Channel::Stdout => self.println(message),
            Channel::Stderr => self.print_note(message),
        }
    }

    // Write `text` on `channel`, and record it for a test as the stream gets
    // it. A caller that must not be drawn over writes inside `draw`.
    fn put(&self, channel: Channel, text: &Shown) {
        self.record_plain(channel, text);
        self.write(channel, text);
    }

    // The one writer of a line, which records nothing for `printed`: a printer
    // whose test reads back part of the line records that part itself.
    fn write(&self, channel: Channel, text: &Shown) {
        let line = self.guarded(channel, text);
        match channel {
            Channel::Stdout => println!("{line}"),
            Channel::Stderr => eprintln!("{line}"),
        }
    }

    // As `put` on stderr, above the waiting spinners, which keep drawing.
    fn put_above_spinners(&self, text: &Shown) {
        self.record_plain(Channel::Stderr, text);
        let _ = self.mp.println(self.guarded(Channel::Stderr, text));
    }

    // `text` as `channel` gets it: selfie's own styling kept when the stream
    // has colors and removed otherwise, and anything else that could act on
    // the terminal escaped.
    fn guarded(&self, channel: Channel, text: &Shown) -> String {
        let line = guard(text.as_str(), self.colors(channel)).into_owned();
        #[cfg(test)]
        self.record_emitted(channel, &line);
        line
    }

    #[cfg(test)]
    fn record_emitted(&self, channel: Channel, line: &str) {
        if let Ok(mut emitted) = self.emitted.lock() {
            emitted.push((channel, line.to_string()));
        }
    }

    /// Print a styled key-value pair (stdout, for config display, etc.), the
    /// value's lines after the first starting under its first character.
    pub(crate) fn print_field(&self, key: Literal, value: impl Into<Shown>) {
        // The printer styles the value itself, so any styling in it is removed.
        let value = value.into().guarded(false);
        self.println_after(
            shown!("  {} ", Shown::from(key).painted(|s| s.italic().dim())),
            value.painted(|s| s.bold()),
        );
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
    /// display.result_card(lit!("Check Results"))
    ///     .field(lit!("Package"), check.package)
    ///     .field(lit!("Environment"), check.environment)
    ///     .command_if(lit!("Command"), check.command)
    ///     .print();
    /// ```
    pub(crate) fn result_card(&self, title: Literal) -> ResultCard<'_> {
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
            self.print_note(summary);
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
/// Created by [`DisplayManager::result_card()`]. Call `.field()` / `.command_if()`
/// to add rows, then `.print()` to render.
pub(crate) struct ResultCard<'a> {
    display: &'a DisplayManager,
    title: Literal,
    // Each field's key, its value, and the spaces after the key's colon that
    // start the value at a column shared with another field.
    fields: Vec<(Literal, Shown, usize)>,
}

impl<'a> ResultCard<'a> {
    fn new(display: &'a DisplayManager, title: Literal) -> Self {
        Self {
            display,
            title,
            fields: Vec::new(),
        }
    }

    /// Add a key-value field to the card.
    pub(crate) fn field(mut self, key: Literal, value: impl Into<Shown>) -> Self {
        self.fields.push((key, value.into(), 0));
        self
    }

    /// Add a field holding a command, only if there is one, and a field
    /// holding its note under it when it has one. The two values start at one
    /// column, whatever `key` is.
    pub(crate) fn command_if(mut self, key: Literal, command: Option<Command>) -> Self {
        if let Some(command) = command {
            match command.note() {
                Some(note) => {
                    let [key_pad, escaped_pad] = aligned_pads(key, ESCAPED);
                    self.fields.push((key, command.into(), key_pad));
                    self.fields.push((ESCAPED, note, escaped_pad));
                }
                None => self.fields.push((key, command.into(), 0)),
            }
        }
        self
    }

    /// Add fields whose values selfie styled itself, in order.
    pub(crate) fn styled_fields(
        mut self,
        fields: impl IntoIterator<Item = (Literal, Shown)>,
    ) -> Self {
        self.fields
            .extend(fields.into_iter().map(|(key, value)| (key, value, 0)));
        self
    }

    /// Render the card to the display
    pub(crate) fn print(self) {
        use crate::formatters::format_key;

        self.display.println(lit!(""));
        self.display.print_section_header(self.title);

        for (key, value, pad) in self.fields {
            // The guard removes the key's styling where stdout has none.
            self.display.println_after(
                shown!("{}{}: {}", INDENT, format_key(key, true), spaces(pad)),
                value,
            );
        }
    }
}

// The spaces that pad each of two labels to the wider one's width, so values
// written after them start at one column.
fn aligned_pads(first: Literal, second: Literal) -> [usize; 2] {
    let width = |label: Literal| console::measure_text_width(label.as_str());
    let widest = width(first).max(width(second));
    [widest - width(first), widest - width(second)]
}

// `count` spaces, as text a lead can hold.
fn spaces(count: usize) -> Shown {
    Shown::from(Name::new(&" ".repeat(count)))
}

// The terminal column at which `needle` starts in `text`, counted from the
// start of the line it is on (test-only).
#[cfg(test)]
pub(crate) fn column(text: &str, needle: &str) -> usize {
    let at = text
        .find(needle)
        .unwrap_or_else(|| panic!("{needle:?} not in {text:?}"));
    let start = text[..at].rfind('\n').map_or(0, |n| n + 1);
    console::measure_text_width(&text[start..at])
}

/// A unified diff, each line escaped as file content and the file names as
/// paths, ready for [`DisplayManager::print_diff`].
pub(crate) struct Diff(Vec<(Shown, Paint)>);

impl Diff {
    /// Escapes each line of `diff`, a unified diff.
    pub(crate) fn new(diff: &str) -> Self {
        const SEPARATOR: Literal =
            lit!("──────────────────────────────────────────────────────────────────────");

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
                DiffLine::Hunk(line) => rows.push((Shown::from(Content::new(line)), |s| s.cyan())),
                DiffLine::Removed(line) => {
                    rows.push((Shown::from(Content::new(line)), |s| s.red()))
                }
                DiffLine::Added(line) => {
                    rows.push((Shown::from(Content::new(line)), |s| s.green()))
                }
                DiffLine::Context(line) => {
                    rows.push((Shown::from(Content::new(line)), |s| s.dim()))
                }
            }
        }
        if printed_separator {
            rows.push((Shown::from(SEPARATOR), |s| s.dim()));
        }

        Self(rows)
    }
}

/// `block`'s lines, each escaped as a line of file content, so an invisible
/// character shows as its escape and a tab stays a tab. A trailing line break,
/// LF or CRLF, costs nothing.
pub(crate) fn content_lines(block: &str) -> Shown {
    Shown::join(block.lines().map(Content::new), lit!("\n"))
}

// Returns `lead`, then `text` with its continuation lines starting under its
// first character: at the width of the lead's last line, as the terminal shows
// it. A tab in `text` is expanded from the text's own start, so a line hung
// under the lead keeps the shape it has at column 0.
fn led(lead: &Shown, text: impl Into<Shown>) -> Shown {
    let last = lead.rsplit('\n').next().unwrap_or_default();
    shown!(
        "{}{}",
        lead,
        text.into()
            .tabs_expanded()
            .hang(console::measure_text_width(last))
    )
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
            .field("colors", &self.colors)
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

    // Each message printer shows a library sentence as prose: a joiner renders,
    // and a line break is kept with the line after it indented under the
    // message, so no line of it starts at the left margin.
    #[test]
    fn every_message_printer_escapes_what_it_prints() {
        use selfie::package::event::Outcome;

        let text = lit!("a\u{1b}[2J\u{202E}b\u{200D}\n✓ c");
        let shown = "a\\u{1b}[2J\\u{202e}b\u{200D}\n  ✓ c";
        let display = DisplayManager::new(false);
        display.print_info(text);
        display.print_error(text);
        display.print_result(Outcome::Clean, text);
        display.print_success(text);
        display.print_warning(text);
        display.print_progress(text);
        display.print_run_note(text);

        let printed = display.printed();
        assert_eq!(printed.len(), 7, "{printed:?}");
        assert!(printed.iter().all(|(_, line)| line == shown), "{printed:?}");
    }

    // A suggestion's text continues under its first character, past the mark
    // and the word that introduce it, whether selfie styled it or not.
    #[test]
    fn a_suggestion_continues_under_its_text() {
        let display = DisplayManager::new(false);
        // Words no mark could hold, so each is found in the text.
        let (first, second) = ("¶one", "¶two");
        display.print_suggestion(Prose::new(&format!("{first}\u{1b}[2J\n{second}")));
        display.print_suggestion(shown!(
            "{}\n{}",
            Shown::from(lit!("¶one")).paint(true, |s| s.bold()),
            lit!("¶two")
        ));

        let written = display.written();
        assert_eq!(written.len(), 2, "{written:?}");
        for line in &written {
            assert!(
                column(line, first) > 2,
                "control: the text is past the mark"
            );
            assert_eq!(column(line, second), column(line, first), "{line:?}");
        }
        assert!(written[0].contains("¶one\\u{1b}[2J"), "{written:?}");
        let printed = display.printed();
        let text_start = written[1].find(first).expect("the text was written");
        assert_eq!(printed[1].1, written[1][text_start..], "{printed:?}");
    }

    // A field's value continues under its first line.
    #[test]
    fn a_field_value_continues_under_its_first_line() {
        let display = DisplayManager::new(false);

        display.print_field(lit!("key:"), Prose::new("a\nb"));

        let printed = display.printed();
        assert_eq!(
            printed,
            [(Channel::Stdout, "  key: a\n       b".to_string())]
        );
        assert_eq!(column(&printed[0].1, "b"), column(&printed[0].1, "a"));
    }

    // A card's value continues under its first line, past its key.
    #[test]
    fn a_card_value_continues_under_its_first_line() {
        let display = DisplayManager::new(false);

        display
            .result_card(lit!("Results"))
            .field(lit!("Output"), Prose::new("a\nb"))
            .print();

        let line = display
            .written()
            .into_iter()
            .find(|line| line.contains("Output"))
            .expect("the field was written");
        assert!(line.starts_with("   Output: a"), "{line:?}");
        assert_eq!(column(&line, "b"), column(&line, "a"), "{line:?}");
    }

    // A text after a lead continues under its first character, the whole
    // line recorded. This reads the record, which has its styling removed; that
    // a styled lead's escapes take no room is `led`'s, tested above.
    #[test]
    fn println_after_hangs_under_the_text() {
        let display = DisplayManager::new(false);

        display.println_after(shown!("  {} ", lit!("⚠")), Prose::new("a\nb"));

        let printed = display.printed();
        assert_eq!(printed, [(Channel::Stdout, "  ⚠ a\n    b".to_string())]);
    }

    // A message selfie styled itself keeps its styling: the printer writes its
    // sequences as selfie's own, never as escapes.
    #[test]
    fn a_painted_message_shows_no_escapes() {
        let display = DisplayManager::new(true);
        display.print_info(Shown::from(lit!("blue")).paint(true, |s| s.blue()));

        assert_eq!(display.printed(), [(Channel::Stdout, "blue".to_string())]);
    }

    // A library sentence written with Windows line endings ends at its text,
    // with no carriage return shown after it.
    #[test]
    fn a_message_ending_in_a_carriage_return_shows_none() {
        let display = DisplayManager::new(false);
        display.print_info(crate::received::message("one\r\n"));

        assert_eq!(display.printed(), [(Channel::Stdout, "one".to_string())]);
    }

    // The two printers that write their text themselves still guard it: a
    // control in selfie's own text shows as an escape.
    #[test]
    fn error_context_and_section_headers_guard_what_they_print() {
        let display = DisplayManager::new(false);
        display.print_error_context(lit!("a\u{1b}[2Jb"));
        display.print_section_header(lit!("t\u{1b}]0;x\u{7}"));

        assert_eq!(
            display.printed(),
            [
                (Channel::Stderr, r"a\u{1b}[2Jb".to_string()),
                (Channel::Stdout, r"t\u{1b}]0;x\u{7}".to_string()),
            ]
        );
    }

    // A section header is one line: a line break in its title is escaped.
    #[test]
    fn a_section_header_stays_on_one_line() {
        let display = DisplayManager::new(false);
        display.print_section_header(Name::new("a\n✓ b"));

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
        display.println(lit!("one\u{1b}[31m\ntwo\u{202E}\t"));
        display.print_note(lit!("three\u{1b}]0;t\u{7}"));

        assert_eq!(
            display.printed(),
            [
                (Channel::Stdout, "one\ntwo\\u{202e}\t".to_string()),
                (Channel::Stderr, "three\\u{1b}]0;t\\u{7}".to_string()),
            ]
        );
    }

    // Error context is evidence a reader inspects, such as a source window or a
    // failed tool's stderr, so each line is escaped as file content: an
    // invisible character shows, and a tab stays a tab.
    #[test]
    fn error_context_lines_show_invisible_characters_and_keep_tabs() {
        let display = DisplayManager::new(false);
        display.print_error_context(content_lines("a\u{200B}b\tc\nd\u{200D}e\u{1b}[2J"));

        assert_eq!(
            display.printed(),
            [
                (Channel::Stderr, "a\\u{200b}b\tc".to_string()),
                (Channel::Stderr, "d\\u{200d}e\\u{1b}[2J".to_string()),
            ]
        );
    }

    // A carriage return before a line feed ends the line with it, as a file
    // read line by line ends it, so a block from a CRLF file shows no `\r`. An
    // empty block prints no line at all.
    #[test]
    fn error_context_treats_crlf_as_a_line_break_and_prints_nothing_for_nothing() {
        let crlf = DisplayManager::new(false);
        crlf.print_error_context(content_lines("one\r\ntwo\r\n"));
        let empty = DisplayManager::new(false);
        empty.print_error_context(content_lines(""));

        let lines = |display: &DisplayManager| -> Vec<String> {
            display
                .printed()
                .into_iter()
                .map(|(_, line)| line)
                .collect()
        };
        assert_eq!(lines(&crlf), ["one", "two"]);
        assert!(lines(&empty).is_empty(), "{:?}", lines(&empty));
    }

    // A run without colors prints no styling, whatever console decides about
    // the stream: the printers do not read the environment's color settings
    // alone.
    #[test]
    fn a_run_without_colors_prints_no_styling() {
        let display = DisplayManager::new(false);
        display.println(Shown::from(lit!("x")).paint(true, |s| s.red()));

        assert_eq!(display.printed(), [(Channel::Stdout, "x".to_string())]);
    }

    // A waiting step's subject shows as it was handed in while the step runs
    // and on the line it collapses to when it ends.
    #[test]
    fn a_waiting_subject_is_escaped() {
        let shown = "Running `printf \\u{1b}[2J`";
        let plain = DisplayManager::new(false);
        plain.start_waiting(ONE, Name::new("Running `printf \u{1b}[2J`"));
        assert_eq!(plain.printed(), [(Channel::Stderr, format!("{shown}..."))]);

        let drawn = DisplayManager::new(false).drawing_as_a_terminal();
        drawn.start_waiting(ONE, Name::new("Running `printf \u{1b}[2J`"));
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

    // A waiting step's description can name a path holding a line break. It
    // stays on its one line, on the spinner and on the line it collapses to,
    // so it cannot print a line of its own such as a fake `✓`; and a blank in
    // the path shows as an escape, as in the entry row under it.
    #[test]
    fn a_waiting_subject_stays_on_one_line() {
        let subject = "Running the commands that produce ~/.a\n✓ b\u{A0}c";
        let shown = "Running the commands that produce ~/.a\\n✓ b\\u{a0}c";

        let plain = DisplayManager::new(false);
        plain.start_waiting(ONE, crate::received::step(subject));
        assert_eq!(plain.printed(), [(Channel::Stderr, format!("{shown}..."))]);

        let drawn = DisplayManager::new(false).drawing_as_a_terminal();
        drawn.start_waiting(ONE, crate::received::step(subject));
        assert_eq!(drawn.waiting_messages(), [shown]);
        drawn.end_waiting(ONE, StepEnding::Succeeded);
        let ended = drawn.printed();
        assert_eq!(ended.len(), 1, "{ended:?}");
        assert!(ended[0].1.starts_with(&format!("✓ {shown} (")), "{ended:?}");
    }

    // A text after a lead continues under its first character: the lead is
    // measured as the terminal shows it, its last line only, and its styling
    // takes no room.
    #[test]
    fn led_measures_only_the_leads_last_line() {
        let text = || Shown::from(Prose::new("a\nb"));

        assert_eq!(led(&Shown::from(lit!("x\n  ")), text()), "x\n  a\n  b");
        let painted = shown!("{} ", Shown::from(lit!("✓")).paint(true, |s| s.green()));
        assert!(
            painted.len() > 2,
            "control: the lead has more bytes than columns"
        );
        assert_eq!(guard(led(&painted, text()).as_str(), false), "✓ a\n  b");
    }

    // The end-of-run summary continues a multi-line message under its first
    // line.
    #[test]
    fn the_error_summary_continues_a_message_under_it() {
        let mut collector = ErrorCollector::default();
        for (name, message) in [("one", "a\nb"), ("two", "c")] {
            collector.collect(ErrorDetail {
                package_name: Name::new(name),
                operation: Prose::new("install"),
                command: None,
                exit_code: None,
                stderr: None,
                message: Prose::new(message).into(),
            });
        }

        let summary = collector
            .format_summary()
            .expect("two errors make a summary");

        assert!(summary.contains("\n  a\n  b\n"), "{summary}");
    }

    // A diff is written in one draw, so no other line lands between its rows.
    #[test]
    fn a_diff_is_written_in_one_draw() {
        let display = DisplayManager::new(false);

        display.print_diff(Diff::new("--- a\n+++ b\n@@ -1 +1 @@\n-x\n+y\n"));

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
        assert_eq!(Output::line("\u{1b}[31mred\u{1b}[0m").to_string(), "red");
        assert_eq!(
            Output::line("a\u{202E}b\u{FE0F}c").to_string(),
            "a\\u{202e}b\u{FE0F}c"
        );
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
        assert_eq!(dm.colors, StreamColors::default());
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
        dm.print_info(crate::terminal_text::lit!("test info"));
        dm.print_error(crate::terminal_text::lit!("test error"));
        dm.print_success(crate::terminal_text::lit!("test success"));
        dm.print_warning(crate::terminal_text::lit!("test warning"));
        dm.print_progress(crate::terminal_text::lit!("test progress"));
        dm.print_suggestion(crate::terminal_text::lit!("test suggestion"));
        dm.print_section_header(lit!("test section"));
        dm.println(lit!("test println"));
        dm.print_field(lit!("key:"), lit!("value"));
    }

    #[test]
    fn test_static_output_with_colors() {
        let dm = DisplayManager::new(true);
        dm.print_info(crate::terminal_text::lit!("test info"));
        dm.print_error(crate::terminal_text::lit!("test error"));
        dm.print_success(crate::terminal_text::lit!("test success"));
        dm.print_warning(crate::terminal_text::lit!("test warning"));
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
            package_name: Name::new("test-pkg"),
            operation: Prose::new("install"),
            command: Some(Command::written("brew install test-pkg")),
            exit_code: Some(1),
            stderr: Some(vec![Output::line("Error: not found")]),
            message: Prose::new("Installation failed").into(),
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
            package_name: Name::new("test-pkg"),
            operation: Prose::new("install"),
            command: Some(Command::written("brew install test-pkg")),
            exit_code: Some(1),
            stderr: Some(vec![Output::line("Error: not found")]),
            message: Prose::new("Installation failed").into(),
        });
        collector.collect(ErrorDetail {
            package_name: Name::new("other-pkg"),
            operation: Prose::new("install"),
            command: Some(Command::written("brew install other-pkg")),
            exit_code: Some(1),
            stderr: None,
            message: Prose::new("Also failed").into(),
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

    // A failed command keeps its lines in the summary, each continued line
    // under the command's first character, and a command that reads as another
    // gets its escaped note the same way, line for line.
    // A tab in a command is expanded from the command's own start, so a
    // continued line keeps its shape under the value column in the summary and
    // in a card, and no raw tab reaches the terminal to jump to the screen's
    // own tab stop.
    #[test]
    fn a_tab_in_a_summary_or_card_command_is_expanded_under_its_text() {
        let command = || Command::written("make\n\tbuild");
        let mut collector = ErrorCollector::default();
        for name in ["a", "b"] {
            collector.collect(ErrorDetail {
                package_name: Name::new(name),
                operation: Prose::new("install"),
                command: Some(command()),
                exit_code: None,
                stderr: None,
                message: Prose::new("failed").into(),
            });
        }
        let summary = collector.format_summary().unwrap().into_string();
        let display = DisplayManager::new(false);
        display
            .result_card(lit!("Check Results"))
            .command_if(lit!("Command"), Some(command()))
            .print();
        let card = display
            .printed()
            .into_iter()
            .map(|(_, line)| line)
            .collect::<Vec<_>>()
            .join("\n");

        for text in [&summary, &card] {
            let lines: Vec<&str> = text.lines().collect();
            let at = lines
                .iter()
                .position(|line| line.contains("Command: make"))
                .unwrap_or_else(|| panic!("{lines:#?}"));
            let value = column(lines[at], "make");
            assert_eq!(
                lines[at + 1],
                format!("{}{}build", " ".repeat(value), " ".repeat(8)),
                "{lines:#?}"
            );
            assert!(!text.contains('\t'), "{text:?}");
        }
    }

    // A card's note starts at its command's column whatever the command's key
    // is: a key shorter than the note's label is padded, and so is the label
    // when the key is longer.
    #[test]
    fn a_card_note_lines_up_with_its_command_under_any_key() {
        for key in [lit!("Run"), lit!("Install command")] {
            let display = DisplayManager::new(false);
            display
                .result_card(lit!("Check Results"))
                .command_if(key, Some(Command::written("echo a\u{200B}b")))
                .print();
            let text = display
                .printed()
                .into_iter()
                .map(|(_, line)| line)
                .collect::<Vec<_>>()
                .join("\n");
            let lines: Vec<&str> = text.lines().collect();
            let at = lines
                .iter()
                .position(|line| line.contains(key.as_str()))
                .unwrap_or_else(|| panic!("{lines:#?}"));

            assert!(lines[at + 1].contains("Escaped:"), "{lines:#?}");
            assert_eq!(
                column(lines[at + 1], "echo"),
                column(lines[at], "echo"),
                "{lines:#?}"
            );
        }
    }

    #[test]
    fn the_summary_hangs_a_command_and_its_note_under_the_command() {
        let mut collector = ErrorCollector::default();
        for name in ["a", "b"] {
            collector.collect(ErrorDetail {
                package_name: Name::new(name),
                operation: Prose::new("install"),
                command: Some(Command::written("echo a\u{200B}b\nfalse")),
                exit_code: Some(1),
                stderr: None,
                message: Prose::new("failed").into(),
            });
        }

        let summary = collector.format_summary().unwrap().into_string();
        let lines: Vec<&str> = summary.lines().collect();
        let command = lines
            .iter()
            .position(|line| line.starts_with("  Command: "))
            .unwrap_or_else(|| panic!("{lines:#?}"));

        let value = column(lines[command], "echo");
        assert_eq!(
            lines[command + 1],
            format!("{}false", " ".repeat(value)),
            "{lines:#?}"
        );
        assert_eq!(
            lines[command + 2],
            "  Escaped: echo a\\u{200b}b",
            "{lines:#?}"
        );
        assert_eq!(
            lines[command + 3],
            format!("{}false", " ".repeat(value)),
            "{lines:#?}"
        );
    }

    // The end-of-run summary is printed on stderr, through the printer's
    // guard, once two errors are collected.
    #[test]
    fn finishing_prints_the_error_summary_on_stderr() {
        let dm = DisplayManager::new(false);
        for name in ["a\u{200B}b", "c"] {
            dm.collect_error(ErrorDetail {
                package_name: Name::new(name),
                operation: Prose::new("install"),
                command: None,
                exit_code: None,
                stderr: None,
                message: lit!("failed").into(),
            });
        }

        dm.finish();

        let printed = dm.printed();
        assert!(
            printed
                .iter()
                .all(|(channel, _)| *channel == Channel::Stderr),
            "{printed:?}"
        );
        let summary: String = printed.into_iter().map(|(_, text)| text).collect();
        assert!(summary.contains("── Errors"), "{summary}");
        assert!(summary.contains(r"✗ a\u{200b}b (install)"), "{summary}");
    }

    #[test]
    fn test_display_manager_error_collection() {
        let dm = DisplayManager::new(false);
        assert!(!dm.has_errors());

        dm.collect_error(ErrorDetail {
            package_name: Name::new("test"),
            operation: Prose::new("check"),
            command: None,
            exit_code: None,
            stderr: None,
            message: Prose::new("test error").into(),
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
            package_name: Name::new("test"),
            operation: Prose::new("install"),
            command: None,
            exit_code: None,
            stderr: None,
            message: Prose::new("shared error").into(),
        });

        // Clone should see the same errors (Arc<Mutex<>>)
        assert!(dm2.has_errors());
    }

    #[test]
    fn test_result_card_basic() {
        let dm = DisplayManager::new(false);
        // Verify the builder API works without panicking
        dm.result_card(lit!("Test Results"))
            .field(lit!("Package"), lit!("test-pkg"))
            .field(lit!("Environment"), lit!("macos"))
            .print();
    }

    #[test]
    fn test_result_card_with_colors() {
        let dm = DisplayManager::new(true);
        dm.result_card(lit!("Test Results"))
            .field(lit!("Package"), lit!("test-pkg"))
            .field(lit!("Status"), lit!("valid"))
            .print();
    }

    // A card's command shows with its blanks as written, so it pastes as it
    // runs, and a missing one leaves no row. A blank that reads as a space gets
    // an `Escaped` field under it, whose lines start at the command's column
    // and match the command's lines.
    #[test]
    fn a_result_card_shows_a_command_as_written() {
        let dm = DisplayManager::new(false);

        dm.result_card(lit!("Test Results"))
            .field(lit!("Package"), lit!("test-pkg"))
            .command_if(
                lit!("Command"),
                Some(Command::written("echo 'a\u{A0}b'\necho \u{200B}done")),
            )
            .command_if(lit!("Plain"), Some(Command::written("true")))
            .command_if(lit!("Missing"), None)
            .print();

        let printed: Vec<String> = dm.printed().into_iter().map(|(_, line)| line).collect();
        let text = printed.join("\n");
        let lines: Vec<&str> = text.lines().collect();
        let at = |needle: &str| {
            lines
                .iter()
                .position(|line| line.contains(needle))
                .unwrap_or_else(|| panic!("{needle:?} not in {lines:#?}"))
        };
        let command = at("Command: echo 'a\u{A0}b'");
        assert_eq!(
            lines[command + 1].trim_start(),
            "echo \u{200B}done",
            "{lines:#?}"
        );
        assert!(
            lines[command + 2].contains("Escaped: echo 'a\\u{a0}b'"),
            "{lines:#?}"
        );
        assert_eq!(
            lines[command + 3].trim_start(),
            "echo \\u{200b}done",
            "{lines:#?}"
        );
        let value = column(lines[command], "echo");
        for offset in 1..=3 {
            assert_eq!(column(lines[command + offset], "echo"), value, "{lines:#?}");
        }
        assert_eq!(lines[command + 4].trim_start(), "Plain: true", "{lines:#?}");
        assert_eq!(lines.len(), command + 5, "{lines:#?}");
        assert!(!text.contains("Missing"), "{lines:#?}");
    }

    #[test]
    fn test_static_output_during_a_waiting_step() {
        let dm = DisplayManager::new(false).drawing_as_a_terminal();
        dm.start_waiting(ONE, Name::new("Running the install command for bat"));
        // These should not panic even with a spinner running
        dm.print_info(crate::terminal_text::lit!("info during spinner"));
        dm.print_warning(crate::terminal_text::lit!("warning during spinner"));
        dm.print_error(crate::terminal_text::lit!("error during spinner"));
        dm.print_success(crate::terminal_text::lit!("success during spinner"));
        dm.println(lit!("plain during spinner"));
        dm.print_section_header(lit!("header during spinner"));
        dm.print_progress(crate::terminal_text::lit!("progress during spinner"));
        dm.print_suggestion(crate::terminal_text::lit!("suggestion during spinner"));
        dm.print_field(lit!("key:"), lit!("value"));
        // An ordinary line does not end the wait.
        assert!(!dm.waiting_messages().is_empty());
        dm.end_waiting(ONE, StepEnding::Succeeded);
    }

    #[test]
    fn a_command_line_becomes_the_spinner_message_on_a_terminal() {
        let dm = DisplayManager::new(false).drawing_as_a_terminal();
        dm.start_waiting(ONE, Name::new("Running the install command for ripgrep"));

        dm.command_output(ONE, Output::line("\x1b[32mDownloading\x1b[0m ripgrep\r"));
        assert_eq!(dm.waiting_messages(), vec!["Downloading ripgrep"]);

        dm.command_output(ONE, Output::line("Pouring ripgrep"));
        assert_eq!(dm.waiting_messages(), vec!["Pouring ripgrep"]);
        // Nothing from the command is printed as a line of its own.
        assert!(dm.printed().is_empty(), "{:?}", dm.printed());
    }

    // Without a terminal and at default verbosity, a command's output is not
    // shown: the one status line stands for it.
    #[test]
    fn a_command_line_is_hidden_without_a_terminal() {
        let dm = DisplayManager::new(false);
        dm.start_waiting(ONE, Name::new("Running the install command for ripgrep"));
        dm.command_output(ONE, Output::line("Downloading ripgrep"));

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
        dm.start_waiting(ONE, Name::new("Running the install command for ripgrep"));
        dm.command_output(ONE, Output::line("Downloading ripgrep"));

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
        dm.start_waiting(ONE, Name::new("Running the install command for ripgrep"));
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

    // A menu answers with the value of the choice at the index picked, so a
    // name's raw bytes come back even though the menu showed them escaped.
    #[test]
    fn select_returns_the_picked_choices_value() {
        let choices = [
            Choice::named("plain", "plain".to_string()),
            Choice::named("evil\u{1b}[2J", "evil\u{1b}[2J".to_string()),
        ];
        let dm = DisplayManager::new(false).answering(vec![answer(1_usize)]);

        assert_eq!(
            dm.select(lit!("Which?"), &choices).unwrap(),
            "evil\u{1b}[2J"
        );
    }

    #[test]
    fn multi_select_returns_only_the_picked_values() {
        let choices = [
            Choice::labeled(lit!("one"), 1),
            Choice::labeled(lit!("two"), 2),
            Choice::labeled(lit!("three"), 3),
        ];
        let dm = DisplayManager::new(false).answering(vec![answer(vec![0_usize, 2])]);

        assert_eq!(dm.multi_select(lit!("Which?"), &choices).unwrap(), [1, 3]);
    }

    // Esc at a fuzzy select picks nothing.
    #[test]
    fn fuzzy_select_returns_none_for_esc() {
        let choices = [
            Choice::labeled(lit!("one"), 1),
            Choice::labeled(lit!("two"), 2),
        ];
        let dm = DisplayManager::new(false)
            .answering(vec![answer(None::<usize>), answer(Some(1_usize))]);

        assert_eq!(dm.fuzzy_select(lit!("Which?"), &choices).unwrap(), None);
        assert_eq!(dm.fuzzy_select(lit!("Which?"), &choices).unwrap(), Some(2));
    }

    #[test]
    fn confirm_returns_the_answer() {
        let dm = DisplayManager::new(false).answering(vec![answer(true), answer(false)]);

        assert!(dm.confirm(lit!("Sure?"), false).unwrap());
        assert!(!dm.confirm(lit!("Sure?"), true).unwrap());
    }

    // A named choice shows its name escaped; the menu never draws it raw.
    #[test]
    fn a_named_choice_shows_its_name_escaped() {
        let choice = Choice::named("a\u{1b}\u{200b}b", ());

        assert_eq!(choice.label.as_str(), "a\\u{1b}\\u{200b}b");
    }

    // A suggestion is pre-filled only when it shows as written; otherwise it
    // is noted escaped, and kept on Enter only when its bytes could be an
    // answer.
    #[test]
    fn a_suggestion_is_offered_as_it_can_be_shown() {
        let keepable = Suggestion::Keepable("dev\u{1b}".to_string());
        let unkeepable = Suggestion::Unkeepable("dev\u{1b}".to_string());

        assert_eq!(offer(&Suggestion::Nothing), Offer::Bare);
        assert_eq!(
            offer(&Suggestion::Keepable("dev".to_string())),
            Offer::Prefill("dev")
        );
        assert_eq!(
            offer(&Suggestion::Unkeepable("dev".to_string())),
            Offer::Prefill("dev")
        );
        assert_eq!(
            offer(&keepable),
            Offer::Note {
                shown: Name::new("dev\u{1b}"),
                keep: Some("dev\u{1b}")
            }
        );
        assert_eq!(
            offer(&unkeepable),
            Offer::Note {
                shown: Name::new("dev\u{1b}"),
                keep: None
            }
        );
    }

    // Enter under a keepable suggestion's note returns its bytes, and the note
    // shows them escaped, once.
    #[test]
    fn an_empty_answer_keeps_a_keepable_suggestion_shown_escaped() {
        let dm = DisplayManager::new(false).answering(vec![answer(String::new())]);

        let typed = dm
            .input(
                lit!("Environment name"),
                Suggestion::Keepable("dev\u{1b}".to_string()),
                Blank::Allowed,
            )
            .unwrap();

        assert_eq!(typed, "dev\u{1b}");
        assert_eq!(
            dm.printed(),
            vec![(
                Channel::Stderr,
                "  Suggested: dev\\u{1b} (Enter keeps it)".to_string()
            )]
        );
    }

    // An unkeepable suggestion is shown, never returned.
    #[test]
    fn an_empty_answer_never_returns_an_unkeepable_suggestion() {
        let dm = DisplayManager::new(false).answering(vec![answer(String::new())]);

        let typed = dm
            .input(
                lit!("Name"),
                Suggestion::Unkeepable("git\u{200b}".to_string()),
                Blank::Allowed,
            )
            .unwrap();

        assert_eq!(typed, "");
        assert_eq!(
            dm.printed(),
            vec![(
                Channel::Stderr,
                "  Suggested: git\\u{200b} (shown escaped; type the answer)".to_string()
            )]
        );
    }

    // What the user types is the answer, whatever was suggested.
    #[test]
    fn a_typed_answer_is_returned_as_typed() {
        let dm = DisplayManager::new(false).answering(vec![answer("prod".to_string())]);

        let typed = dm
            .input(
                lit!("Environment name"),
                Suggestion::Keepable("dev\u{1b}".to_string()),
                Blank::Allowed,
            )
            .unwrap();

        assert_eq!(typed, "prod");
    }

    // A required prompt asks again on a blank answer, a cleared pre-fill
    // included, and says why.
    #[test]
    fn a_blank_answer_to_a_required_prompt_is_asked_again() {
        let dm = DisplayManager::new(false).answering(vec![
            answer(String::new()),
            answer("  ".to_string()),
            answer("prod".to_string()),
        ]);

        let typed = dm
            .input(
                lit!("Environment name"),
                Suggestion::Keepable("dev".to_string()),
                Blank::Refused(lit!("An answer is required.")),
            )
            .unwrap();

        assert_eq!(typed, "prod");
        let refusals = dm
            .printed()
            .iter()
            .filter(|(_, line)| line.contains("An answer is required."))
            .count();
        assert_eq!(refusals, 2, "{:?}", dm.printed());
    }

    // A kept suggestion that is blank is refused by a required prompt like a
    // typed blank: U+00A0 is shown escaped, kept on Enter, and still blank.
    #[test]
    fn a_blank_kept_suggestion_is_refused_by_a_required_prompt() {
        let dm = DisplayManager::new(false)
            .answering(vec![answer(String::new()), answer("prod".to_string())]);

        let typed = dm
            .input(
                lit!("Environment name"),
                Suggestion::Keepable("\u{a0}".to_string()),
                Blank::Refused(lit!("Environment name cannot be empty.")),
            )
            .unwrap();

        assert_eq!(typed, "prod");
        let refusals = dm
            .printed()
            .iter()
            .filter(|(_, line)| line.contains("Environment name cannot be empty."))
            .count();
        assert_eq!(refusals, 1, "{:?}", dm.printed());
    }

    // The note is written in the prompt's own draw, with every spinner
    // suspended, not in a draw of its own that a spinner could overwrite
    // before the question appears.
    #[test]
    fn a_suggestions_note_is_written_with_the_question() {
        let dm = DisplayManager::new(false).answering(vec![answer(String::new())]);

        dm.input(
            lit!("Environment name"),
            Suggestion::Keepable("dev\u{1b}".to_string()),
            Blank::Allowed,
        )
        .unwrap();

        assert_eq!(dm.draws.load(std::sync::atomic::Ordering::Relaxed), 1);
        assert_eq!(dm.printed().len(), 1, "{:?}", dm.printed());
    }

    // An allowed blank is the caller's to judge.
    #[test]
    fn a_blank_answer_to_an_optional_prompt_is_returned() {
        let dm = DisplayManager::new(false).answering(vec![answer(String::new())]);

        let typed = dm
            .input(lit!("Description"), Suggestion::Nothing, Blank::Allowed)
            .unwrap();

        assert_eq!(typed, "");
        assert!(dm.printed().is_empty(), "{:?}", dm.printed());
    }

    // With no terminal nothing is asked, so no note is printed above it.
    #[test]
    fn an_input_without_a_terminal_prints_no_note() {
        let dm = DisplayManager::new(false);

        let failure = dm
            .input(
                lit!("Environment name"),
                Suggestion::Keepable("dev\u{1b}".to_string()),
                Blank::Allowed,
            )
            .unwrap_err();

        assert!(matches!(failure, PromptFailure::NoTerminal), "{failure:?}");
        assert!(dm.printed().is_empty(), "{:?}", dm.printed());
    }

    // Without colors a prompt draws with the plain theme. The colored half is
    // tested on a pseudoterminal in prompt_signal_tests.rs, since a unit test
    // run from a terminal can see one on stderr.
    #[test]
    fn a_display_without_colors_prompts_in_the_plain_theme() {
        assert_eq!(
            DisplayManager::new(false).prompt_theme(),
            PromptTheme::Simple
        );
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

        let exit = dm.refuse_prompt(
            &PromptFailure::NoTerminal,
            lit!("Removing a spec"),
            lit!("Pass --yes."),
        );
        let canceled = dm.refuse_prompt(
            &PromptFailure::Interrupted,
            lit!("Removing a spec"),
            lit!("Pass --yes."),
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
        dm.start_waiting(ONE, Name::new("Running the commands that produce ~/.creds"));

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
        assert_eq!(Output::line("10%\r50%\r100%\r").to_string(), "100%");
        assert_eq!(Output::line("Name\tVersion").to_string(), "Name Version");
    }

    // A stream that ends without a result leaves no spinner behind.
    #[test]
    fn finishing_clears_a_waiting_spinner() {
        let dm = DisplayManager::new(false).drawing_as_a_terminal();
        dm.start_waiting(ONE, Name::new("Running the install command for bat"));

        dm.finish();

        assert!(dm.waiting_messages().is_empty());
    }

    const TWO: StepId = StepId::from_raw(2);

    // Concurrent steps each keep their own spinner: ending one leaves the
    // other running, and its line names only itself.
    #[test]
    fn overlapping_steps_end_one_at_a_time() {
        let dm = DisplayManager::new(false).drawing_as_a_terminal();
        dm.start_waiting(ONE, Name::new("Running the audit command for bat"));
        dm.start_waiting(TWO, Name::new("Running the audit command for fd"));

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
        dm.start_waiting(ONE, Name::new("Running the install command for a"));
        dm.start_waiting(TWO, Name::new("Running the install command for b"));

        dm.command_output(ONE, Output::line("from a"));

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
        dm.start_waiting(ONE, Name::new("Running the install command for a"));
        dm.command_output(ONE, Output::line("alone"));
        dm.start_waiting(TWO, Name::new("Running the install command for b"));
        dm.command_output(ONE, Output::line("beside b"));

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
        dm.start_waiting(ONE, Name::new("Running the commands that produce ~/.creds"));
        dm.command_output(ONE, Output::line("no session"));

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
        dm.start_waiting(ONE, Name::new("Running the install command for foo"));
        dm.command_output(ONE, Output::line("Error: no bottle"));
        dm.command_output(ONE, Output::line("==> Caveats"));
        dm.end_waiting(ONE, StepEnding::Failed);

        dm.print_command_stderr(&[
            Output::line("Error: checksum mismatch"),
            Output::line("Error: no bottle"),
        ]);

        let lines: Vec<String> = dm.printed().into_iter().map(|(_, l)| l).collect();
        let count = |needle: &str| lines.iter().filter(|l| l.contains(needle)).count();
        assert_eq!(count("checksum mismatch"), 1, "{lines:?}");
        assert_eq!(count("no bottle"), 1, "{lines:?}");
    }

    // A failed step's last lines and a failed command's stderr are what the
    // tool printed: a joiner or a zero-width space in them renders, as it does
    // under `--verbose`.
    #[test]
    fn a_failed_steps_stderr_shows_as_the_tool_printed_it() {
        let dm = DisplayManager::new(false);
        dm.start_waiting(ONE, Name::new("Running the install command for foo"));
        dm.command_output(ONE, Output::line("tail x\u{200B}y"));
        dm.end_waiting(ONE, StepEnding::Failed);
        dm.print_command_stderr(&[Output::line("stderr a\u{200D}b")]);

        let lines: Vec<String> = dm.printed().into_iter().map(|(_, l)| l).collect();
        assert!(lines.contains(&"tail x\u{200B}y".to_string()), "{lines:?}");
        assert!(
            lines.contains(&"stderr a\u{200D}b".to_string()),
            "{lines:?}"
        );
    }

    // The tool's lines are still cleaned as its output: colors removed, a
    // line redrawn by carriage returns shown in its final state, and no
    // control character left.
    #[test]
    fn a_failed_steps_stderr_is_still_cleaned() {
        let dm = DisplayManager::new(false);
        dm.start_waiting(ONE, Name::new("Running the install command for foo"));
        dm.command_output(ONE, Output::line("\u{1b}[31mred\u{1b}[0m 10%\r100%"));
        dm.end_waiting(ONE, StepEnding::Failed);
        dm.print_command_stderr(&[Output::line("\u{1b}[2Jcleared\u{7}")]);

        let lines: Vec<String> = dm.printed().into_iter().map(|(_, l)| l).collect();
        assert!(lines.contains(&"100%".to_string()), "{lines:?}");
        assert!(lines.contains(&"cleared".to_string()), "{lines:?}");
        assert!(lines.iter().all(|l| !l.contains('\u{1b}')), "{lines:?}");
    }

    // Under `--verbose` the lines were shown as they came, so a failure does
    // not repeat them.
    #[test]
    fn a_failed_step_does_not_repeat_lines_shown_verbosely() {
        let dm = DisplayManager::new(false).with_verbosity(Verbosity::Verbose);
        dm.start_waiting(ONE, Name::new("Running the install command for a"));
        dm.command_output(ONE, Output::line("no bottle"));

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
        dm.start_waiting(ONE, Name::new("first"));
        std::thread::sleep(Duration::from_millis(1100));
        dm.start_waiting(TWO, Name::new("second"));

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
        dm.start_waiting(ONE, Name::new("Running the install command for a"));

        dm.end_waiting(ONE, StepEnding::Canceled);

        assert!(dm.printed().is_empty(), "{:?}", dm.printed());
        assert!(dm.waiting_messages().is_empty());
    }

    #[test]
    fn a_status_line_prints_only_when_verbose() {
        let normal = DisplayManager::new(false);
        normal.print_status(crate::terminal_text::lit!("Loading specs..."));
        assert!(normal.printed().is_empty());

        let verbose = DisplayManager::new(false).with_verbosity(Verbosity::Verbose);
        verbose.print_status(crate::terminal_text::lit!("Loading specs..."));
        assert_eq!(
            verbose.printed(),
            vec![(Channel::Stderr, "Loading specs...".to_string())]
        );
    }

    // What one printer does to a display, and the stream it writes to.
    type Printing = (&'static str, Channel, fn(&DisplayManager));

    // Every printer that styles what it writes, each through selfie's own
    // styling, so a line it writes holds an SGR sequence exactly when it kept
    // that styling.
    fn styling_printers() -> Vec<Printing> {
        use selfie::package::event::Outcome;

        fn red() -> Shown {
            Shown::from(lit!("x")).painted(|s| s.red())
        }
        vec![
            ("info", Channel::Stdout, |d| d.print_info(lit!("x"))),
            ("success", Channel::Stdout, |d| d.print_success(lit!("x"))),
            ("result", Channel::Stdout, |d| {
                d.print_result(Outcome::Clean, lit!("x"));
            }),
            ("println", Channel::Stdout, |d| d.println(red())),
            ("println_after", Channel::Stdout, |d| {
                d.println_after(red(), lit!("x"));
            }),
            ("field", Channel::Stdout, |d| {
                d.print_field(lit!("k:"), lit!("v"))
            }),
            ("diff", Channel::Stdout, |d| {
                d.print_diff(Diff::new("--- a\n+++ b\n@@ -1 +1 @@\n-x\n+y\n"));
            }),
            ("entry row", Channel::Stdout, |d| {
                d.print_entry_row(&EntryRow {
                    outcome: crate::entry_rows::EntryOutcome::Deployed,
                    source: crate::source_paths::SourceText::Plain(DisplayPath::new("s").into()),
                    target: DisplayPath::new("t"),
                    detail: None,
                });
            }),
            ("result card", Channel::Stdout, |d| {
                d.result_card(lit!("T")).field(lit!("k"), lit!("v")).print();
            }),
            ("section header", Channel::Stdout, |d| {
                d.print_section_header(lit!("x"));
            }),
            ("error", Channel::Stderr, |d| d.print_error(lit!("x"))),
            ("warning", Channel::Stderr, |d| d.print_warning(lit!("x"))),
            ("run note", Channel::Stderr, |d| d.print_run_note(lit!("x"))),
            ("progress", Channel::Stderr, |d| d.print_progress(lit!("x"))),
            ("suggestion", Channel::Stderr, |d| {
                d.print_suggestion(lit!("x"))
            }),
            ("note", Channel::Stderr, |d| d.print_note(red())),
            ("error context", Channel::Stderr, |d| {
                d.print_error_context(lit!("x"));
            }),
            ("stderr section header", Channel::Stderr, |d| {
                d.print_section_header_to(Channel::Stderr, lit!("x"));
            }),
            ("verbose command output", Channel::Stderr, |d| {
                d.start_waiting(ONE, Name::new("step"));
                d.command_output(ONE, Output::line("out"));
            }),
        ]
    }

    fn styled(line: &str) -> bool {
        line.contains("\u{1b}[")
    }

    fn other(channel: Channel) -> Channel {
        match channel {
            Channel::Stdout => Channel::Stderr,
            Channel::Stderr => Channel::Stdout,
        }
    }

    fn display_for(colored: Channel) -> DisplayManager {
        DisplayManager::new(true)
            .with_verbosity(Verbosity::Verbose)
            .drawing_as_a_terminal()
            .colored(colored)
    }

    // A printer keeps selfie's styling exactly when the stream it writes to has
    // colors: colored on its own stream it writes SGR there, and colored only
    // on the other stream it writes none, on either stream.
    #[test]
    fn each_printer_styles_only_when_its_own_stream_has_colors() {
        for (name, channel, print) in styling_printers() {
            let own = display_for(channel);
            print(&own);
            let emitted = own.emitted();
            assert!(!emitted.is_empty(), "{name} wrote nothing");
            assert!(
                emitted.iter().all(|(stream, _)| *stream == channel),
                "{name} wrote on another stream: {emitted:?}"
            );
            assert!(
                emitted.iter().any(|(_, line)| styled(line)),
                "{name} wrote no styling with its stream colored: {emitted:?}"
            );

            let elsewhere = display_for(other(channel));
            print(&elsewhere);
            let emitted = elsewhere.emitted();
            assert!(
                emitted.iter().all(|(_, line)| !styled(line)),
                "{name} wrote styling its stream has no colors for: {emitted:?}"
            );
        }
    }

    // A waiting step's mark is written above the spinners on stderr, so it
    // follows stderr's colors.
    #[test]
    fn a_waiting_steps_mark_follows_stderr() {
        for (colored, expect_styled) in [(Channel::Stderr, true), (Channel::Stdout, false)] {
            let dm = DisplayManager::new(true)
                .drawing_as_a_terminal()
                .colored(colored);
            dm.start_waiting(ONE, Name::new("step"));
            dm.end_waiting(ONE, StepEnding::Succeeded);

            let emitted = dm.emitted();
            assert_eq!(emitted.len(), 1, "{emitted:?}");
            assert_eq!(emitted[0].0, Channel::Stderr);
            assert!(emitted[0].1.contains('✓'), "{emitted:?}");
            assert_eq!(styled(&emitted[0].1), expect_styled, "{emitted:?}");
        }
    }

    // A suggestion's note above the question is written by the guarded writer,
    // on stderr, with the question's own draw.
    #[test]
    fn a_suggestions_note_is_written_on_stderr() {
        let dm = DisplayManager::new(true)
            .colored(Channel::Stdout)
            .answering(vec![answer(String::new())]);

        dm.input(
            lit!("Environment name"),
            Suggestion::Keepable("dev\u{1b}".to_string()),
            Blank::Allowed,
        )
        .unwrap();

        assert_eq!(
            dm.emitted(),
            [(
                Channel::Stderr,
                "  Suggested: dev\\u{1b} (Enter keeps it)".to_string()
            )]
        );
    }

    // A stream gets colors only when the run has them on and console would
    // give that stream colors.
    #[test]
    fn colors_are_on_only_where_the_config_and_the_stream_both_say_so() {
        for use_colors in [false, true] {
            for stdout in [false, true] {
                for stderr in [false, true] {
                    assert_eq!(
                        decided(use_colors, StreamColors { stdout, stderr }),
                        StreamColors {
                            stdout: use_colors && stdout,
                            stderr: use_colors && stderr,
                        },
                        "use_colors {use_colors}, console {stdout}/{stderr}"
                    );
                }
            }
        }
    }

    // The spinner draws on stderr, so its template follows stderr's colors.
    #[test]
    fn the_spinner_template_follows_stderr() {
        let stderr = DisplayManager::new(true).colored(Channel::Stderr);
        let stdout = DisplayManager::new(true).colored(Channel::Stdout);

        assert!(stderr.spinner_template().contains(":.cyan"));
        assert!(!stdout.spinner_template().contains(":.cyan"));
    }

    // console's color answer for stderr is process-wide, and two tests set
    // it; each holds this while it does.
    static COLOR_GLOBAL: Mutex<()> = Mutex::new(());

    // console's stderr answer set to `colors` until the guard drops, which
    // restores it, while the test holds `COLOR_GLOBAL`.
    struct StderrColors {
        before: bool,
        _held: std::sync::MutexGuard<'static, ()>,
    }

    impl StderrColors {
        fn set(colors: bool) -> Self {
            let held = COLOR_GLOBAL
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let before = console::colors_enabled_stderr();
            console::set_colors_enabled_stderr(colors);
            Self {
                before,
                _held: held,
            }
        }
    }

    impl Drop for StderrColors {
        fn drop(&mut self) {
            console::set_colors_enabled_stderr(self.before);
        }
    }

    // Everything `theme` draws, through every method dialoguer's prompts call,
    // with each choice a method takes.
    fn everything_drawn(theme: &dyn dialoguer::theme::Theme) -> Vec<String> {
        let matcher = fuzzy_matcher::skim::SkimMatcherV2::default();
        type Draw<'a> = Box<dyn Fn(&mut String) -> std::fmt::Result + 'a>;
        let draws: Vec<Draw<'_>> = vec![
            Box::new(|f| theme.format_prompt(f, "p")),
            Box::new(|f| theme.format_error(f, "e")),
            Box::new(|f| theme.format_confirm_prompt(f, "p", Some(true))),
            Box::new(|f| theme.format_confirm_prompt(f, "p", Some(false))),
            Box::new(|f| theme.format_confirm_prompt(f, "p", None)),
            Box::new(|f| theme.format_confirm_prompt_selection(f, "p", Some(true))),
            Box::new(|f| theme.format_confirm_prompt_selection(f, "p", Some(false))),
            Box::new(|f| theme.format_confirm_prompt_selection(f, "p", None)),
            Box::new(|f| theme.format_input_prompt(f, "p", Some("d"))),
            Box::new(|f| theme.format_input_prompt(f, "", Some("d"))),
            Box::new(|f| theme.format_input_prompt(f, "p", None)),
            Box::new(|f| theme.format_input_prompt_selection(f, "p", "s")),
            Box::new(|f| theme.format_password_prompt(f, "p")),
            Box::new(|f| theme.format_password_prompt_selection(f, "p")),
            Box::new(|f| theme.format_select_prompt(f, "p")),
            Box::new(|f| theme.format_select_prompt_selection(f, "p", "s")),
            Box::new(|f| theme.format_multi_select_prompt(f, "p")),
            Box::new(|f| theme.format_multi_select_prompt_selection(f, "p", &["a", "b"])),
            Box::new(|f| theme.format_sort_prompt(f, "p")),
            Box::new(|f| theme.format_sort_prompt_selection(f, "p", &["a", "b"])),
            Box::new(|f| theme.format_fuzzy_select_prompt(f, "p", "ab", 1)),
        ];
        let mut drawn: Vec<String> = draws
            .iter()
            .map(|draw| {
                let mut text = String::new();
                draw(&mut text).unwrap();
                text
            })
            .collect();
        for one in [true, false] {
            let mut text = String::new();
            theme
                .format_select_prompt_item(&mut text, "t", one)
                .unwrap();
            drawn.push(text);
            for other in [true, false] {
                let mut text = String::new();
                theme
                    .format_multi_select_prompt_item(&mut text, "t", one, other)
                    .unwrap();
                theme
                    .format_sort_prompt_item(&mut text, "t", one, other)
                    .unwrap();
                theme
                    .format_fuzzy_select_prompt_item(&mut text, "abc", one, other, &matcher, "b")
                    .unwrap();
                drawn.push(text);
            }
        }
        drawn
    }

    // The colorful theme draws the same, styled, whatever console's own answer
    // for stderr is, since the display chose it for a stream it colors. A
    // style left to console's answer draws plain once that answer is no.
    #[test]
    fn the_colorful_theme_styles_whatever_consoles_state_says() {
        let theme = prompts::colorful_theme();
        let with = {
            let _colors = StderrColors::set(true);
            everything_drawn(&theme)
        };
        let without = {
            let _colors = StderrColors::set(false);
            everything_drawn(&theme)
        };

        assert!(with.iter().any(|text| styled(text)), "{with:?}");
        assert_eq!(with, without);
    }

    // A fuzzy select bolds its matches only where the display gives stderr
    // colors: the plain theme would bold them through console's own answer.
    #[test]
    fn the_fuzzy_highlight_follows_stderr() {
        use dialoguer::theme::{SimpleTheme, Theme};

        let plain = DisplayManager::new(true).colored(Channel::Stdout);
        let mut item = String::new();
        {
            let _colors = StderrColors::set(true);
            SimpleTheme
                .format_fuzzy_select_prompt_item(
                    &mut item,
                    "abc",
                    true,
                    plain.fuzzy_highlights(),
                    &fuzzy_matcher::skim::SkimMatcherV2::default(),
                    "b",
                )
                .unwrap();
        }

        assert!(!styled(&item), "{item:?}");
        assert!(
            DisplayManager::new(true)
                .colored(Channel::Stderr)
                .fuzzy_highlights()
        );
    }
}
