//! What selfie says about the standalone dotfiles directory.

use std::path::{Path, PathBuf};

use crate::package::port::PackageListError;

/// What a failed listing of the standalone dotfiles directory means.
#[derive(Debug)]
pub(crate) enum UnlistedDotfilesDirectory {
    /// Nothing is at the default path, and `dotfiles_directory` is not set.
    UnsetAndMissing,
    /// Nothing is at the path the user set.
    ConfiguredAndMissing(PathBuf),
    /// Something is at the path and could not be listed, so it may hold
    /// standalone dotfiles.
    Unlistable(PackageListError),
}

impl UnlistedDotfilesDirectory {
    /// Classify `error` from listing the dotfiles directory, where `configured`
    /// says whether the user set `dotfiles_directory`.
    pub(crate) fn from_list_error(error: PackageListError, configured: bool) -> Self {
        // No memory of earlier listings is needed for the default. It is the
        // sibling of the package directory, which is listed first, so its parent
        // is known to be traversable and "not found" is a definite answer.
        match error {
            PackageListError::PackageDirectoryNotFound(path) if configured => {
                Self::ConfiguredAndMissing(path)
            }
            PackageListError::PackageDirectoryNotFound(_) => Self::UnsetAndMissing,
            error @ PackageListError::IoError(_) => Self::Unlistable(error),
        }
    }
}

/// The warning for a command that carries on without the standalone dotfiles
/// in the configured directory at `path`, which does not exist.
pub(crate) fn missing_warning(path: &Path) -> String {
    format!(
        "Dotfiles directory does not exist: {} — standalone dotfiles will not be read. {}",
        path.display(),
        create_suggestion(path)
    )
}

/// The refusal for tracking a standalone dotfile into the dotfiles directory at
/// `path`, which does not exist.
pub(crate) fn track_refusal(path: &Path) -> String {
    format!(
        "Cannot track a standalone dotfile: the dotfiles directory does not exist: {} {}",
        path.display(),
        create_suggestion(path)
    )
}

/// The refusal for `track_standalone`, given `error` from listing the
/// dotfiles directory at `path` for the name about to be tracked.
///
/// A directory that is not there gets [`track_refusal`]'s sentence and its
/// `mkdir -p` hint. Any other listing error is a directory that exists and
/// could not be read, which offers no hint, because creating a directory
/// that is already there would not fix anything.
pub(crate) fn track_listing_refusal(
    path: &Path,
    error: crate::package::port::PackageListError,
) -> String {
    match error {
        crate::package::port::PackageListError::PackageDirectoryNotFound(_) => track_refusal(path),
        other => format!(
            "Cannot track a standalone dotfile: the dotfiles directory could not be listed: {} — {other}",
            path.display()
        ),
    }
}

/// How to create the dotfiles directory at `path`.
fn create_suggestion(path: &Path) -> String {
    format!("Create it with: mkdir -p {}", path.display())
}

#[cfg(test)]
mod tests {
    use std::{path::PathBuf, sync::Arc};

    use super::*;

    fn not_found() -> PackageListError {
        PackageListError::PackageDirectoryNotFound(PathBuf::from("/home/me/dotfiles"))
    }

    fn unlistable() -> PackageListError {
        PackageListError::IoError(Arc::new(std::io::Error::from(
            std::io::ErrorKind::PermissionDenied,
        )))
    }

    #[test]
    fn a_configured_directory_that_is_not_there_is_missing_at_its_path() {
        match UnlistedDotfilesDirectory::from_list_error(not_found(), true) {
            UnlistedDotfilesDirectory::ConfiguredAndMissing(path) => {
                assert_eq!(path, PathBuf::from("/home/me/dotfiles"));
            }
            other => panic!("expected ConfiguredAndMissing, got {other:?}"),
        }
    }

    // The ordinary state of anyone who keeps no standalone dotfiles.
    #[test]
    fn an_unset_default_that_is_not_there_is_unset_and_missing() {
        assert!(matches!(
            UnlistedDotfilesDirectory::from_list_error(not_found(), false),
            UnlistedDotfilesDirectory::UnsetAndMissing
        ));
    }

    // Something is at the path whether or not the user named it, and it may hold
    // standalone dotfiles.
    #[test]
    fn a_directory_that_will_not_list_is_unlistable_whether_or_not_configured() {
        for configured in [true, false] {
            assert!(
                matches!(
                    UnlistedDotfilesDirectory::from_list_error(unlistable(), configured),
                    UnlistedDotfilesDirectory::Unlistable(PackageListError::IoError(_))
                ),
                "configured: {configured}"
            );
        }
    }

    // A missing directory is the one case `track_listing_refusal` hands off to
    // `track_refusal` outright, so the two must produce identical sentences,
    // `mkdir -p` remedy included.
    #[test]
    fn a_missing_directory_produces_the_same_refusal_as_track_refusal() {
        let dir = Path::new("/home/me/dotfiles");

        assert_eq!(track_listing_refusal(dir, not_found()), track_refusal(dir));
    }

    // Any other listing error names the directory and says it could not be
    // listed, but offers no `mkdir -p` hint: the directory already exists, so
    // creating it again would not fix anything.
    #[test]
    fn any_other_listing_error_names_the_directory_without_a_mkdir_hint() {
        let dir = Path::new("/home/me/dotfiles");

        let refusal = track_listing_refusal(dir, unlistable());

        assert!(refusal.contains("/home/me/dotfiles"), "{refusal}");
        assert!(refusal.contains("could not be listed"), "{refusal}");
        assert!(!refusal.contains("mkdir"), "{refusal}");
    }

    // Apply warns and track refuses about the same directory in one session, so
    // the two must say different things while naming the same fix.
    #[test]
    fn the_warning_and_the_refusal_do_not_say_the_same_thing() {
        let dir = Path::new("/nonexistent/dotfiles");

        let warning = missing_warning(dir);
        let refusal = track_refusal(dir);

        assert_ne!(warning, refusal);
        assert!(warning.contains("will not be read"), "{warning}");
        assert!(refusal.contains("Cannot track"), "{refusal}");
        assert!(warning.contains("/nonexistent/dotfiles"), "{warning}");
        assert!(refusal.contains("/nonexistent/dotfiles"), "{refusal}");
        assert_eq!(
            create_suggestion(dir),
            "Create it with: mkdir -p /nonexistent/dotfiles"
        );
    }
}
