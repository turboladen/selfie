//! What collecting packages found worth saying, and what it refused.
//!
//! These are raised before any event stream exists, so they travel as values and
//! leave later, each as the event its kind calls for.

use std::path::PathBuf;

use crate::package::event::OperationFailure;

/// Something collecting packages found, in the order it found it.
// One list for both kinds, so the events leave in the order the findings were
// made: a skipped spec's warning still precedes the failure it explains.
#[derive(Clone)]
pub(crate) enum Finding {
    /// Worth saying, and refusing nothing.
    Warning(ApplyWarning),
    /// One refusal for an operation over every package.
    Refusal(CollectionRefusal),
}

impl Finding {
    /// The refusal this finding is, if it is one.
    pub(crate) fn refusal(&self) -> Option<&CollectionRefusal> {
        match self {
            Self::Refusal(refusal) => Some(refusal),
            Self::Warning(_) => None,
        }
    }

    /// Whether this finding is about a package name other than `name`, which a
    /// run asked for `name` alone has no reason to report.
    pub(crate) fn is_about_another_name(&self, name: &str) -> bool {
        let about = match self {
            Self::Warning(ApplyWarning::SkippedSpec(error))
            | Self::Refusal(CollectionRefusal::UnloadableSpec { error, .. }) => {
                crate::package::spec_name_of(error.package_path())
            }
            Self::Warning(ApplyWarning::Named { name, .. })
            | Self::Refusal(CollectionRefusal::AmbiguousName { name, .. }) => Some(name.clone()),
            Self::Warning(ApplyWarning::AbsentDotfilesDirectory { .. })
            | Self::Refusal(CollectionRefusal::UnreadableDotfilesDirectory(_)) => None,
        };
        about.is_some_and(|about| about != name.to_lowercase())
    }

    /// Whether a run for the one package `name` reports this finding: not one
    /// about a spec of another name, and no ambiguity.
    pub(crate) fn reported_for(&self, name: &str) -> bool {
        // A run that finds its package has no use for either kind it leaves out,
        // and one that does not find it fails with a message naming the files.
        !self.is_about_another_name(name)
            && !matches!(self, Self::Refusal(CollectionRefusal::AmbiguousName { .. }))
    }

    /// Whether this finding is a warning, and not about a spec of another name.
    pub(crate) fn is_warning_for(&self, name: &str) -> bool {
        matches!(self, Self::Warning(_)) && !self.is_about_another_name(name)
    }

    /// Emit this finding on the event stream, as the event its kind calls for.
    pub(crate) async fn send(&self, sender: &crate::package::event::EventSender) {
        match self {
            Self::Warning(warning) => warning.send(sender).await,
            Self::Refusal(refusal) => refusal.send(sender).await,
        }
    }
}

/// A non-fatal warning raised while collecting packages, before any event stream
/// exists to send it on.
#[derive(Clone)]
pub(crate) enum ApplyWarning {
    /// A dotfiles-directory spec that could not be parsed, under a name the
    /// package directory claims, so nothing would have used it.
    SkippedSpec(crate::package::port::PackageParseError),
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

impl ApplyWarning {
    async fn send(&self, sender: &crate::package::event::EventSender) {
        // A skipped spec travels typed so each adapter renders it; everything else
        // is already a sentence.
        match self {
            Self::SkippedSpec(error) => sender.send_spec_skipped(error.clone()).await,
            Self::AbsentDotfilesDirectory { path, reason } => {
                sender
                    .send_dotfiles_directory_warning(
                        super::directory::DotfilesDirectoryWarning::Absent {
                            path: path.clone(),
                            reason: reason.clone(),
                        },
                    )
                    .await;
            }
            Self::Named { message, .. } => sender.send_warning(message.clone()).await,
        }
    }
}

/// A file or directory an operation over every package could not use, found
/// while collecting the packages, before any of them is looked at.
///
/// Each variant names what was refused and carries what a renderer needs to
/// say why; [`sentence`](Self::sentence) says it as prose.
#[derive(Debug, Clone)]
pub enum CollectionRefusal {
    /// A dotfiles directory selfie could not read through, so it may hold
    /// standalone dotfiles.
    UnreadableDotfilesDirectory(super::directory::UnreadableDotfilesDirectory),
    /// A name several spec files in one directory claim, so none of them is used.
    /// Carries the files, sorted.
    AmbiguousName {
        /// The name the files claim, lowercased, as names are compared.
        name: String,
        /// Every file claiming it.
        paths: Vec<PathBuf>,
        /// The directory the files are in.
        base: crate::package::event::BaseKind,
    },
    /// A spec file that could not be loaded, where it could have been used, so
    /// nothing it declares is looked at.
    UnloadableSpec {
        /// Why it could not be loaded.
        error: crate::package::port::PackageParseError,
        /// The directory it is in.
        base: crate::package::event::BaseKind,
    },
}

impl CollectionRefusal {
    /// What a run could not compare its targets with because of this refusal:
    /// the file, every file of an ambiguous name, or the dotfiles directory.
    #[must_use]
    pub fn unchecked_sources(&self) -> Vec<crate::package::event::UncheckedSource> {
        use crate::package::event::UncheckedSource;
        match self {
            Self::UnloadableSpec { error, base } => {
                vec![UncheckedSource::spec_file(error.package_path(), *base)]
            }
            Self::AmbiguousName { paths, base, .. } => paths
                .iter()
                .map(|path| UncheckedSource::spec_file(path, *base))
                .collect(),
            Self::UnreadableDotfilesDirectory(_) => {
                vec![UncheckedSource::DotfilesDirectory]
            }
        }
    }

    /// The status a validation of every spec gives what was refused.
    #[must_use]
    pub fn status(&self) -> crate::package::event::SpecStatus {
        use crate::package::event::SpecStatus;
        match self {
            Self::UnloadableSpec { .. } => SpecStatus::Unparsable,
            Self::AmbiguousName { .. } => SpecStatus::Ambiguous,
            Self::UnreadableDotfilesDirectory(_) => SpecStatus::Unlistable,
        }
    }

    /// Whether this refusal is a dotfiles directory selfie could not read
    /// through, so a collection holding it is missing whatever the directory has.
    pub(crate) fn is_unreadable_directory(&self) -> bool {
        matches!(self, Self::UnreadableDotfilesDirectory(_))
    }

    /// This refusal as a sentence for a person. A spec that could not be loaded
    /// is its parse error's own [`Display`](std::fmt::Display), which leaves out
    /// the file's path.
    #[must_use]
    pub fn sentence(&self) -> String {
        match self {
            Self::UnreadableDotfilesDirectory(unreadable) => unreadable.to_string(),
            Self::AmbiguousName { name, paths, .. } => {
                crate::package::port::ambiguous_files_sentence(name, paths)
            }
            Self::UnloadableSpec { error, .. } => error.to_string(),
        }
    }

    // Each refusal leaves as the event its kind calls for: a directory as its
    // warning, a name as a refused package carrying `sentence`, an unloadable
    // spec typed, so each adapter renders it.
    async fn send(&self, sender: &crate::package::event::EventSender) {
        match self {
            Self::UnreadableDotfilesDirectory(unreadable) => {
                sender
                    .send_dotfiles_directory_warning(
                        super::directory::DotfilesDirectoryWarning::Unreadable(unreadable.clone()),
                    )
                    .await;
            }
            Self::AmbiguousName { name, paths, .. } => {
                sender
                    .send_packages_refused([crate::package::event::PackageRefusal {
                        kind: crate::package::event::RefusalKind::AmbiguousName,
                        reason: self.sentence(),
                        package: crate::package::event::RefusedPackage {
                            name: name.clone(),
                            paths: paths.clone(),
                        },
                    }])
                    .await;
            }
            Self::UnloadableSpec { error, .. } => sender.send_spec_skipped(error.clone()).await,
        }
    }
}

/// The failure for a named apply that no collected package answers.
pub(crate) fn no_such_package(
    name: &str,
    packages: &[crate::package::Package],
    findings: &[Finding],
    unrefused_ambiguities: &[(String, Vec<PathBuf>, crate::package::event::BaseKind)],
) -> OperationFailure {
    use super::directory::UnreadableDotfilesDirectory as Unreadable;
    use crate::package::event::NoSuchPackageReason;

    // The unloadable check runs before either not-found answer. A spec that
    // failed to parse is not among the collected packages, and "no package
    // named" would send the user looking for a file they may be looking at.
    let requested = name.to_lowercase();
    let unloadable = findings.iter().any(|finding| match finding {
        Finding::Warning(ApplyWarning::SkippedSpec(error))
        | Finding::Refusal(CollectionRefusal::UnloadableSpec { error, .. }) => {
            crate::package::spec_name_of(error.package_path())
                .is_some_and(|spec_name| spec_name == requested)
        }
        _ => false,
    });

    // An ambiguity is asked first: it is the first thing to fix, since removing a
    // file may remove the one that failed to parse.
    // Whether or not the ambiguity mattered to an apply of everything: every file
    // claiming the name was left out, so the name finds nothing.
    let ambiguous = findings
        .iter()
        .find_map(|finding| match finding {
            Finding::Refusal(CollectionRefusal::AmbiguousName { name, paths, .. })
                if *name == requested =>
            {
                Some(paths.clone())
            }
            _ => None,
        })
        .or_else(|| {
            unrefused_ambiguities
                .iter()
                .find(|(name, _, _)| *name == requested)
                .map(|(_, paths, _)| paths.clone())
        });
    // Derived from what the directory turned out to be, not from the fact that some
    // repository failed: the two unreadable states take different sentences, and
    // saying "could not be listed" about a symlink loop sends the user to look
    // inside a directory that may not exist.
    let refused = |wanted: fn(&CollectionRefusal) -> bool| {
        findings
            .iter()
            .any(|finding| matches!(finding, Finding::Refusal(refusal) if wanted(refusal)))
    };
    let reason = if let Some(conflicting_paths) = ambiguous {
        NoSuchPackageReason::Ambiguous { conflicting_paths }
    } else if unloadable {
        NoSuchPackageReason::NotLoaded
    } else if refused(|r| {
        matches!(
            r,
            CollectionRefusal::UnreadableDotfilesDirectory(Unreadable::Unlistable(_))
        )
    }) {
        NoSuchPackageReason::MaybeInUnlistableDirectory
    } else if refused(|r| {
        matches!(
            r,
            CollectionRefusal::UnreadableDotfilesDirectory(Unreadable::Uncheckable(_))
        )
    }) {
        NoSuchPackageReason::MaybeInUncheckableDirectory
    } else {
        NoSuchPackageReason::NotFound
    };
    // Only where no file answers to the name: an ambiguous or unloadable one
    // has its own remedy, and a hint beside it would point elsewhere.
    let declared_by = match reason {
        NoSuchPackageReason::NotFound
        | NoSuchPackageReason::MaybeInUnlistableDirectory
        | NoSuchPackageReason::MaybeInUncheckableDirectory => {
            crate::package::port::declaring_name(name, packages)
        }
        NoSuchPackageReason::NotLoaded | NoSuchPackageReason::Ambiguous { .. } => Vec::new(),
    };
    OperationFailure::NoSuchPackage {
        name: name.to_string(),
        reason,
        declared_by,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::package::event::BaseKind;

    fn ambiguity(name: &str) -> Finding {
        Finding::Refusal(CollectionRefusal::AmbiguousName {
            name: name.to_string(),
            paths: vec![PathBuf::from("/p/gem.yml"), PathBuf::from("/p/gem.yaml")],
            base: BaseKind::PackageDirectory,
        })
    }

    // A run for one name reports an ambiguity of no name, whichever name it is
    // about: its own failure names the files, and another name's is not its concern.
    #[test]
    fn a_run_for_one_name_reports_no_ambiguity() {
        assert!(!ambiguity("gem").reported_for("gem"));
        assert!(!ambiguity("gem").reported_for("alpha"));
    }

    #[test]
    fn a_run_for_one_name_reports_a_finding_about_no_name() {
        let finding = Finding::Warning(ApplyWarning::AbsentDotfilesDirectory {
            path: PathBuf::from("/d"),
            reason: crate::fs::AbsentReason::Empty,
        });

        assert!(finding.reported_for("alpha"));
    }

    #[test]
    fn only_a_warning_not_about_another_name_is_a_warning_for_a_name() {
        let named = |name: &str| {
            Finding::Warning(ApplyWarning::Named {
                name: name.to_string(),
                message: String::new(),
            })
        };

        assert!(named("alpha").is_warning_for("alpha"));
        assert!(!named("beta").is_warning_for("alpha"));
        assert!(!ambiguity("alpha").is_warning_for("alpha"));
    }
}
