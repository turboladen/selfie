//! A sentence the library words, as the CLI shows it: [`Prose`], from one of
//! the library's own values or from a string the library hands over as a
//! sentence, or a [`Shown`] built from a library value's parts.

use std::fmt::Display;

use selfie::commands::runner::TimedOut;
use selfie::config::ConfigLoadError;
use selfie::namespace::NamespaceConflict;
use selfie::package::event::{
    CheckFailure, OperationFailure, OperationType, SkipReason, SpecStatus, UncheckedSource,
    UnknownStatus,
};
use selfie::package::port::{PackageError, PackageParseKind, PackageRepoError};
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
    OperationFailure,
    OperationType,
    PackageError,
    PackageParseKind,
    PackageRepoError,
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
/// as [`noted_suggestion`] notes it.
pub(crate) fn rendered(parts: &Parts) -> Shown {
    // The note goes on the line after the sentence, so a value whose parts can
    // hold a command must end the line it is shown on, and no line break may
    // follow a command inside the sentence: the note would then land under a
    // line that does not hold the command.
    let mut parts = parts.into_iter().peekable();
    let mut shown = Vec::new();
    let mut commands = Vec::new();
    while let Some(part) = parts.next() {
        let last = parts.peek().is_none();
        if let Part::Text(clause) = part {
            debug_assert!(
                commands.is_empty() || !clause.contains('\n'),
                "a line break follows a command in {clause:?}"
            );
        }
        // No catch-all, so a part the library adds must choose here how it
        // is shown.
        shown.push(match part {
            // Trimmed before it is escaped, so a CR before the LF goes too.
            Part::Text(clause) if last => Shown::from(message(clause)),
            Part::Text(clause) => Prose::new(clause).into(),
            Part::Name(name) => Name::new(name).into(),
            Part::Path(path) => DisplayPath::of(path).into(),
            Part::Command(command) => {
                commands.push(command);
                suggested(command).into()
            }
        });
    }
    commands
        .into_iter()
        .fold(Shown::join(shown, lit!("")), noted_suggestion)
}

/// `text`, a line that shows `command`, followed by a line spelling the
/// command out when what the line shows is not what the library wrote. That
/// is the case for a character that reads as another, such as U+200B, which
/// the note shows escaped, and for a control character such as a line feed or
/// a tab, which the command already shows escaped, so the command as shown
/// does not run as written.
pub(crate) fn noted_suggestion(text: Shown, command: &SuggestedCommand) -> Shown {
    let shown = suggested(command);
    // A command whose operand holds a control character shows it escaped,
    // which leaves nothing for the strict escape of the note to change, so the
    // note is the command as shown.
    let note = shown
        .note()
        .or_else(|| (shown.to_string() != command.to_string()).then(|| Shown::from(shown.clone())));
    match note {
        Some(note) => shown!("{}\n{}: {}", text, ESCAPED, note),
        None => text,
    }
}

/// `command`, a command the library suggests, as the CLI shows it: its
/// program's words, then its operand's.
pub(crate) fn suggested(command: &SuggestedCommand) -> Command {
    // The library quoted the operand when it built the command, so the word is
    // shown as it is and never quoted again.
    // No catch-all, so a program the library adds fails to compile here until
    // the CLI says how to show it.
    match command.program() {
        Program::Selfie(subcommand) => Command::with_quoted(
            &[lit!("selfie"), subcommand_word(subcommand)],
            command.word(),
        ),
        Program::MakeDirectory => {
            Command::with_quoted(&[lit!("mkdir"), lit!("-p"), lit!("--")], command.word())
        }
    }
}

// The word that names `subcommand` on a command line.
fn subcommand_word(subcommand: Subcommand) -> Literal {
    // No catch-all, so a subcommand the library adds fails to compile here
    // until the CLI says how to show it.
    match subcommand {
        Subcommand::Apply => lit!("apply"),
    }
}
