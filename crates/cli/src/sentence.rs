//! A sentence the library words, as the CLI shows it: [`Prose`], from one of
//! the library's own values or from a string the library hands over as a
//! sentence, or a [`Shown`] built from a library value's parts.

use std::fmt::Display;

use selfie::commands::runner::TimedOut;
use selfie::config::ConfigLoadError;
use selfie::namespace::NamespaceConflict;
use selfie::package::event::{
    CheckFailure, OperationType, SkipReason, SpecStatus, UncheckedSource, UnknownStatus,
};
use selfie::parts::{Composed, Part, Parts, Program, Subcommand, SuggestedCommand};
use selfie::sync_service::port::SyncError;
use selfie::yaml::ParseFailure;

use crate::terminal_text::{
    Command, DisplayPath, ESCAPED, Literal, Name, Prose, Shown, lit, shown,
};

/// A library value whose `Display` is a sentence the CLI shows as [`Prose`].
///
/// Implemented for the library's errors and outcomes, never for a string: a
/// string the library hands over as a sentence goes to [`message`], and a value
/// whose sentence is built from parts goes to [`composed`].
#[diagnostic::on_unimplemented(
    message = "`{Self}` is not one of the library's values, so sentence() cannot take it",
    label = "a string the library hands over as a sentence goes to message(), and a value built from parts to composed()"
)]
pub(crate) trait Sentence: Display + sealed::Seal {}

mod sealed {
    pub trait Seal {}
}

macro_rules! sentences {
    ($($library:ty),+ $(,)?) => {
        $(
            impl sealed::Seal for $library {}
            impl Sentence for $library {}
        )+
    };
}

sentences!(
    CheckFailure,
    ConfigLoadError,
    NamespaceConflict,
    OperationType,
    ParseFailure,
    SkipReason,
    SpecStatus,
    SyncError,
    TimedOut,
    UncheckedSource,
    UnknownStatus,
    selfie::yaml::SerializeFailure,
    std::io::Error,
);

impl<T: Sentence + ?Sized> sealed::Seal for &T {}
impl<T: Sentence + ?Sized> Sentence for &T {}

/// `value`'s sentence, as the library words it, without a trailing line
/// break.
pub(crate) fn sentence(value: impl Sentence) -> Prose {
    message(value.to_string())
}

/// `text`, a sentence the library hands over as a string, such as a warning's
/// message or a refusal's suggestion, without a trailing line break.
pub(crate) fn message(text: impl AsRef<str>) -> Prose {
    Prose::new(text.as_ref().trim_end_matches(['\r', '\n']))
}

/// `value`'s sentence, as the library words it: its parts [`rendered`].
pub(crate) fn composed(value: &impl Composed) -> Shown {
    rendered(&value.parts())
}

/// `parts` as the CLI shows them: a name as a [`Name`], a path as a
/// [`DisplayPath`], and the words around them as [`Prose`]. Line breaks at
/// the end of a sentence that ends in words are left out, as [`message`]
/// leaves them out; a name or a path shows its own line breaks escaped. A
/// command that shows any character escaped is followed by a line saying so,
/// as [`noted_suggestion`] notes it, under the line that holds the command.
/// Show a value whose parts can hold a command at the end of its line, or the
/// last note lands under text that is not its own.
pub(crate) fn rendered(parts: &Parts) -> Shown {
    // The line that holds a command ends at the first line break in a clause
    // after it, or at the end of the sentence, and its notes go there.
    let mut parts = parts.into_iter().peekable();
    let mut line: Vec<Shown> = Vec::new();
    let mut commands: Vec<(&SuggestedCommand, Command)> = Vec::new();
    while let Some(part) = parts.next() {
        // No catch-all, so a part the library adds must choose here how it
        // is shown.
        match part {
            Part::Text(clause) => {
                // Trimmed before it is escaped, so a CR before the LF goes too.
                let clause: &str = if parts.peek().is_none() {
                    clause.trim_end_matches(['\r', '\n'])
                } else {
                    clause
                };
                match clause.split_once('\n') {
                    Some((head, rest)) if !commands.is_empty() => {
                        line.push(Prose::new(head).into());
                        let held = noted_line(std::mem::take(&mut line), &mut commands);
                        line.push(shown!("{}\n", held));
                        line.push(Prose::new(rest).into());
                    }
                    _ => line.push(Prose::new(clause).into()),
                }
            }
            Part::Name(name) => line.push(Name::new(name).into()),
            Part::Path(path) => line.push(DisplayPath::of(path).into()),
            Part::Command(command) => {
                let shown = suggested(command);
                line.push(shown.clone().into());
                commands.push((command, shown));
            }
        }
    }
    noted_line(line, &mut commands)
}

// `line` joined, followed by the note of each of `commands`, which it holds,
// in order. Empties `commands`.
fn noted_line(line: Vec<Shown>, commands: &mut Vec<(&SuggestedCommand, Command)>) -> Shown {
    commands
        .drain(..)
        .fold(Shown::join(line, lit!("")), |text, (command, shown)| {
            noted_suggestion(text, command, &shown)
        })
}

/// `text`, a line that shows `shown`, which is `command` as [`suggested`]
/// shows it, followed by a line spelling the command out when what the line
/// shows is not what the library wrote. That is the case for a character that
/// reads as another, such as U+200B, which the note shows escaped, and for a
/// control character such as a line feed or a tab, which the command already
/// shows escaped, so the command as shown does not run as written.
pub(crate) fn noted_suggestion(text: Shown, command: &SuggestedCommand, shown: &Command) -> Shown {
    // A command whose operand holds a control character shows it escaped,
    // which leaves nothing for the strict escape of the note to change, so the
    // note is the command as shown.
    let note = shown
        .note()
        .or_else(|| (shown.to_string() != command.to_string()).then(|| shown.clone().into()));
    match note {
        Some(note) => shown!("{}\n{}: {}", text, ESCAPED, note),
        None => text,
    }
}

/// `command`, a command the library suggests, as the CLI shows it: its
/// program's words, then the words of its operand or operands.
pub(crate) fn suggested(command: &SuggestedCommand) -> Command {
    // The library quoted the operand or operands when it built the command, so
    // the words are shown as they are and never quoted again.
    // No catch-all, so a program the library adds fails to compile here until
    // the CLI says how to show it.
    match command.program() {
        Program::Selfie(subcommand) => {
            Command::with_quoted(selfie_words(subcommand), command.word())
        }
        Program::MakeDirectory => {
            Command::with_quoted(&[lit!("mkdir"), lit!("-p"), lit!("--")], command.word())
        }
    }
}

// The words that run `subcommand` on a command line, `selfie` first.
fn selfie_words(subcommand: Subcommand) -> &'static [Literal] {
    // `lit!` takes a literal token, so these words cannot be read from the
    // library's spelling; the parity test that every suggested command reads as
    // its Display is what keeps the two in step.
    const APPLY: &[Literal] = &[lit!("selfie"), lit!("apply")];
    const TRACK_DOTFILE: &[Literal] = &[lit!("selfie"), lit!("package"), lit!("track-dotfile")];
    // No catch-all, so a subcommand the library adds fails to compile here
    // until the CLI says how to show it.
    match subcommand {
        Subcommand::Apply => APPLY,
        Subcommand::PackageTrackDotfile => TRACK_DOTFILE,
    }
}
