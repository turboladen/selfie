//! A sentence the library words, as the CLI shows it: [`Prose`], from one of
//! the library's own values or from a string the library hands over as a
//! sentence, or a [`Shown`] built from a library value's parts.

use std::fmt::Display;

use selfie::commands::runner::TimedOut;
use selfie::config::ConfigLoadError;
use selfie::namespace::{NamespaceConflict, NamespaceValidationError};
use selfie::package::event::{
    CheckFailure, OperationFailure, OperationType, SkipReason, SpecStatus, UncheckedSource,
    UnknownStatus,
};
use selfie::package::port::{PackageError, PackageListError, PackageParseKind, PackageRepoError};
use selfie::parts::{Composed, Part, Parts, Subcommand};
use selfie::sync_service::port::SyncError;
use selfie::yaml::ParseFailure;

use crate::terminal_text::{Command, DisplayPath, Literal, Name, Prose, Shown, lit};

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
    NamespaceValidationError,
    OperationFailure,
    OperationType,
    PackageError,
    PackageListError,
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
/// leaves them out; a name or a path shows its own line breaks escaped.
pub(crate) fn rendered(parts: &Parts) -> Shown {
    let mut parts = parts.into_iter().peekable();
    let mut shown = Vec::new();
    while let Some(part) = parts.next() {
        let last = parts.peek().is_none();
        // No catch-all, so a part the library adds must choose here how it
        // is shown.
        shown.push(match part {
            // Trimmed before it is escaped, so a CR before the LF goes too.
            Part::Text(clause) if last => Shown::from(message(clause)),
            Part::Text(clause) => Prose::new(clause).into(),
            Part::Name(name) => Name::new(name).into(),
            Part::Path(path) => DisplayPath::of(path).into(),
            // The library quoted the operand when it built the command, so
            // the word is shown as it is and never quoted again.
            Part::Command(command) => Command::with_quoted(
                &[lit!("selfie"), subcommand_word(command.subcommand())],
                command.word(),
            )
            .into(),
        });
    }
    Shown::join(shown, lit!(""))
}

// The word that names `subcommand` on a command line.
fn subcommand_word(subcommand: Subcommand) -> Literal {
    // No catch-all, so a subcommand the library adds fails to compile here
    // until the CLI says how to show it.
    match subcommand {
        Subcommand::Apply => lit!("apply"),
    }
}
