//! A sentence the library words, as the CLI shows it: [`Prose`], from one of
//! the library's own values or from a string the library hands over as a
//! sentence.

use std::fmt::Display;

use selfie::commands::runner::TimedOut;
use selfie::config::ConfigLoadError;
use selfie::namespace::{NamespaceConflict, NamespaceValidationError};
use selfie::package::event::{
    CheckFailure, OperationFailure, OperationSuccess, OperationType, SkipReason, SpecStatus,
    UncheckedSource, UnknownStatus,
};
use selfie::package::port::{PackageError, PackageListError, PackageParseKind, PackageRepoError};
use selfie::sync_service::port::SyncError;
use selfie::yaml::ParseFailure;

use crate::terminal_text::Prose;

/// A library value whose `Display` is a sentence the CLI shows as [`Prose`].
///
/// Implemented for the library's errors and outcomes, never for a string: a
/// string the library hands over as a sentence goes to [`message`].
#[diagnostic::on_unimplemented(
    message = "`{Self}` is not one of the library's values, so sentence() cannot take it",
    label = "a string the library hands over as a sentence goes to message()"
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
    OperationSuccess,
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
