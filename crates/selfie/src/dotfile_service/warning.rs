//! What collecting packages found worth saying, and what a name in both
//! directories means.
//!
//! These are raised before any event stream exists, so they travel as values and
//! leave later, each as the event its kind calls for.

use std::path::PathBuf;

use crate::package::event::OperationFailure;

/// A non-fatal warning raised while collecting packages, before any event stream
/// exists to send it on.
///
/// Two kinds travel together because they are produced in one pass, and they are
/// kept apart because they leave as different events: a skipped spec is reported
/// whole so each adapter can render it, and everything else is already prose.
pub(super) enum ApplyWarning {
    /// A package file that could not be parsed.
    SkippedSpec(crate::package::port::PackageParseError),
    /// A repository that exists and could not be listed, so the collection is
    /// missing whatever it holds.
    UnreadableRepository(crate::package::port::PackageListError),
    /// A repository whose path could not be classified, so nothing is known about
    /// what is at it. Kept apart from
    /// [`UnreadableRepository`](Self::UnreadableRepository) because that one says a
    /// directory is there: this one cannot say even that.
    UncheckableRepository(crate::package::port::PackageListError),
    /// A dotfiles directory the user configured that is not a directory, and why
    /// not. The reason is carried because it decides both the sentence and
    /// whether any remedy is offered.
    AbsentDotfilesDirectory {
        /// The configured path.
        path: PathBuf,
        /// What is there instead of a directory.
        reason: crate::fs::AbsentReason,
    },
    /// Something worth saying about one package name, already worded.
    Named { name: String, message: String },
}

/// A refusal counted before any package is looked at, because collecting the
/// packages found it.
#[derive(Clone)]
pub(super) enum CollectionRefusal {
    /// A dotfiles directory that exists and could not be listed, or whose path
    /// could not be classified. Its warning travels as an [`ApplyWarning`].
    UnreadableDotfilesDirectory,
    /// A name several spec files in one directory claim, so none of them is used.
    /// Carries the files, sorted.
    AmbiguousName { name: String, paths: Vec<PathBuf> },
}

impl CollectionRefusal {
    /// Send the warning naming this refusal, when no [`ApplyWarning`] already
    /// does.
    pub(super) async fn send(&self, sender: &crate::package::event::EventSender) {
        match self {
            Self::UnreadableDotfilesDirectory => {}
            Self::AmbiguousName { name, paths } => {
                sender
                    .send_warning(format!(
                        "Skipping package '{name}': {}",
                        crate::package::port::ambiguous_files_sentence(name, paths)
                    ))
                    .await;
            }
        }
    }
}

/// What a package name appearing in both directories means to the caller.
// A parameter rather than two collectors: the reading of both repositories, the
// unparsable-spec reporting and the unlistable-directory handling are identical,
// and only this one question differs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum NameCollision {
    /// The `packages/` copy wins and the other is dropped with a warning.
    PackagesWin,
    /// Both are kept, because both files are there.
    KeepBoth,
}

impl ApplyWarning {
    /// Whether any of `warnings` is a repository selfie could not read through.
    ///
    /// Both kinds count. A directory that would not list and a path that would not
    /// classify are alike in the one way that matters to a caller deciding whether
    /// its collection is complete: it is not.
    pub(super) fn any_unreadable_repository(warnings: &[Self]) -> bool {
        warnings.iter().any(|warning| {
            matches!(
                warning,
                Self::UnreadableRepository(_) | Self::UncheckableRepository(_)
            )
        })
    }

    /// Whether this warning is about a package name other than `name`, which a
    /// run asked for `name` alone has no reason to report.
    pub(super) fn is_about_another_name(&self, name: &str) -> bool {
        let about = match self {
            Self::SkippedSpec(error) => crate::package::spec_name_of(error.package_path()),
            Self::Named { name, .. } => Some(name.clone()),
            Self::UnreadableRepository(_)
            | Self::UncheckableRepository(_)
            | Self::AbsentDotfilesDirectory { .. } => None,
        };
        about.is_some_and(|about| about != name.to_lowercase())
    }

    /// Emit this warning on the event stream it belongs to.
    ///
    /// The two kinds leave differently on purpose: a skipped spec travels typed so
    /// each adapter renders it, and everything else is already a sentence. Written
    /// once here rather than at each drain, so three call sites cannot disagree.
    pub(super) async fn send(self, sender: &crate::package::event::EventSender) {
        match self {
            Self::SkippedSpec(error) => sender.send_spec_skipped(error).await,
            Self::UnreadableRepository(e) => {
                sender
                    .send_warning(format!("Failed to load standalone dotfiles: {e}"))
                    .await;
            }
            Self::UncheckableRepository(e) => {
                sender
                    .send_warning(format!(
                        "Could not check the standalone dotfiles directory: {e}"
                    ))
                    .await;
            }
            Self::AbsentDotfilesDirectory { path, reason } => {
                sender
                    .send_warning(super::directory::absent_warning(&path, &reason))
                    .await;
            }
            Self::Named { message, .. } => sender.send_warning(message).await,
        }
    }
}

/// The failure for a named apply that no collected package answers.
pub(super) fn no_such_package(
    name: &str,
    warnings: &[ApplyWarning],
    refusals: &[CollectionRefusal],
    unrefused_ambiguities: &[(String, Vec<PathBuf>)],
) -> OperationFailure {
    use crate::package::event::NoSuchPackageReason;

    // The unloadable check runs before either not-found answer. A spec that
    // failed to parse is not among the collected packages, and "no package
    // named" would send the user looking for a file they may be looking at.
    let requested = name.to_lowercase();
    let unloadable = warnings.iter().any(|warning| {
        matches!(warning, ApplyWarning::SkippedSpec(error)
            if crate::package::spec_name_of(error.package_path())
                .is_some_and(|spec_name| spec_name == requested))
    });

    // An ambiguity is asked first: it is the first thing to fix, since removing a
    // file may remove the one that failed to parse.
    // Whether or not the ambiguity mattered to an apply of everything: every file
    // claiming the name was left out, so the name finds nothing.
    let ambiguous = refusals
        .iter()
        .find_map(|refusal| match refusal {
            CollectionRefusal::AmbiguousName { name, paths } if *name == requested => {
                Some(paths.clone())
            }
            _ => None,
        })
        .or_else(|| {
            unrefused_ambiguities
                .iter()
                .find(|(name, _)| *name == requested)
                .map(|(_, paths)| paths.clone())
        });
    let reason = if let Some(conflicting_paths) = ambiguous {
        NoSuchPackageReason::Ambiguous { conflicting_paths }
    } else if unloadable {
        NoSuchPackageReason::NotLoaded
    } else if warnings
        .iter()
        .any(|w| matches!(w, ApplyWarning::UnreadableRepository(_)))
    {
        // Derived from what the directory turned out to be, not from the fact that
        // some repository failed: the two unreadable states take different
        // sentences, and saying "could not be listed" about a symlink loop sends the
        // user to look inside a directory that may not exist.
        NoSuchPackageReason::MaybeInUnlistableDirectory
    } else if warnings
        .iter()
        .any(|w| matches!(w, ApplyWarning::UncheckableRepository(_)))
    {
        NoSuchPackageReason::MaybeInUncheckableDirectory
    } else {
        NoSuchPackageReason::NotFound
    };
    OperationFailure::NoSuchPackage {
        name: name.to_string(),
        reason,
    }
}
