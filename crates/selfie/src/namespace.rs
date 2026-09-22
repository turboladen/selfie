//! Shared namespace validation for packages and standalone dotfiles
//!
//! Packages in `packages/` and standalone dotfiles in `dotfiles/` share a
//! single namespace — there must not be a `dotfiles/foo.yaml` AND a
//! `packages/foo.yaml`. This module provides validation to enforce that
//! constraint.

use std::fmt;

use std::path::Path;

use crate::{
    fs::{DirectoryState, FileSystem},
    package::port::PackageRepository,
};

/// Location where a name was found during namespace validation
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NameLocation {
    /// Found in the packages directory
    Packages,
    /// Found in the standalone dotfiles directory
    Dotfiles,
}

impl fmt::Display for NameLocation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Packages => write!(f, "packages"),
            Self::Dotfiles => write!(f, "dotfiles"),
        }
    }
}

/// Error returned when a name already exists in the shared namespace
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NamespaceConflict {
    /// The conflicting name
    pub name: String,
    /// Where the name was found
    pub found_in: NameLocation,
}

impl fmt::Display for NamespaceConflict {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "name '{}' already exists in {}",
            self.name, self.found_in
        )
    }
}

impl std::error::Error for NamespaceConflict {}

/// Errors that can occur during namespace validation
#[derive(Debug)]
pub enum NamespaceValidationError {
    /// The name conflicts with an existing entry
    Conflict(NamespaceConflict),
    /// Failed to look up names in the required package repository
    LookupFailed(String),
    /// The dotfiles directory could not be read, so whether the name is already
    /// taken is unknown.
    DotfilesDirectoryUnreadable(String),
}

impl fmt::Display for NamespaceValidationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Conflict(c) => write!(f, "{c}"),
            Self::LookupFailed(msg) => write!(f, "namespace lookup failed: {msg}"),
            Self::DotfilesDirectoryUnreadable(msg) => write!(
                f,
                "cannot tell whether the name is already taken: the dotfiles directory {msg}"
            ),
        }
    }
}

impl std::error::Error for NamespaceValidationError {}

impl From<NamespaceConflict> for NamespaceValidationError {
    fn from(conflict: NamespaceConflict) -> Self {
        Self::Conflict(conflict)
    }
}

/// Validate that a name is unique across both the package and dotfiles repositories.
///
/// `dotfiles_directory` is where `dotfiles_repo` reads from, classified through
/// `filesystem` when a listing fails.
///
/// # Errors
///
/// [`NamespaceValidationError::Conflict`] if the name is taken in either
/// directory. [`NamespaceValidationError::LookupFailed`] if the package
/// directory could not be read. [`NamespaceValidationError::DotfilesDirectoryUnreadable`]
/// if the dotfiles directory could not be read or classified, because a name in a
/// directory selfie cannot read is a name it cannot report as free.
///
/// A dotfiles directory that is genuinely not there holds no names, so it is not
/// an error: `Ok(())` says the name is free, and the caller may create it.
pub fn validate_unique_name(
    name: &str,
    package_repo: &impl PackageRepository,
    dotfiles_repo: Option<&impl PackageRepository>,
    filesystem: &impl FileSystem,
    dotfiles_directory: &Path,
) -> Result<(), NamespaceValidationError> {
    // Check packages directory — errors propagate (required repo)
    let files = package_repo
        .find_package_files(name)
        .map_err(|e| NamespaceValidationError::LookupFailed(e.to_string()))?;
    if !files.is_empty() {
        return Err(NamespaceConflict {
            name: name.to_string(),
            found_in: NameLocation::Packages,
        }
        .into());
    }

    // The dotfiles directory. A failed listing here must not be swallowed: answering
    // "the name is free" for a directory selfie could not read lets the caller create
    // a spec beside one that may already carry the name.
    if let Some(dotfiles) = dotfiles_repo {
        match dotfiles.find_package_files(name) {
            Ok(files) if !files.is_empty() => {
                return Err(NamespaceConflict {
                    name: name.to_string(),
                    found_in: NameLocation::Dotfiles,
                }
                .into());
            }
            Ok(_) => {}
            Err(error) => {
                let state = error.directory_state(filesystem, dotfiles_directory);
                return match state {
                    // Nothing is at the path, so it holds no names and the name is
                    // free. The one state that answers the question.
                    DirectoryState::Absent(_) => Ok(()),
                    DirectoryState::Unlistable(_) => {
                        Err(NamespaceValidationError::DotfilesDirectoryUnreadable(
                            format!("could not be listed: {error}"),
                        ))
                    }
                    DirectoryState::Unknown(_) | DirectoryState::Directory => {
                        Err(NamespaceValidationError::DotfilesDirectoryUnreadable(
                            format!("could not be checked: {error}"),
                        ))
                    }
                };
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{path::PathBuf, sync::Arc};

    use tempfile::{TempDir, tempdir};

    use super::*;
    use crate::{
        fs::RealFileSystem,
        package::port::{MockPackageRepository, PackageListError},
    };

    // A path with nothing at it. Every test whose subject is the name rather than
    // the directory uses this, so the directory classifies as absent and the
    // dotfiles half answers from the repository alone.
    fn absent_dotfiles() -> (TempDir, PathBuf) {
        let dir = tempdir().unwrap();
        let path = dir.path().join("dotfiles");
        (dir, path)
    }

    #[test]
    fn test_unique_name_passes_when_not_found() {
        let mut package_repo = MockPackageRepository::new();
        package_repo
            .expect_find_package_files()
            .returning(|_| Ok(vec![]));

        let mut dotfiles_repo = MockPackageRepository::new();
        dotfiles_repo
            .expect_find_package_files()
            .returning(|_| Ok(vec![]));

        let (_guard, path) = absent_dotfiles();
        let result = validate_unique_name(
            "new-pkg",
            &package_repo,
            Some(&dotfiles_repo),
            &RealFileSystem,
            &path,
        );
        assert!(result.is_ok());
    }

    #[test]
    fn test_conflict_in_packages() {
        let mut package_repo = MockPackageRepository::new();
        package_repo
            .expect_find_package_files()
            .returning(|_| Ok(vec![PathBuf::from("/packages/existing.yaml")]));

        let (_guard, path) = absent_dotfiles();
        let result = validate_unique_name(
            "existing",
            &package_repo,
            None::<&MockPackageRepository>,
            &RealFileSystem,
            &path,
        );
        assert!(matches!(
            result,
            Err(NamespaceValidationError::Conflict(NamespaceConflict {
                found_in: NameLocation::Packages,
                ..
            }))
        ));
    }

    #[test]
    fn test_conflict_in_dotfiles() {
        let mut package_repo = MockPackageRepository::new();
        package_repo
            .expect_find_package_files()
            .returning(|_| Ok(vec![]));

        let mut dotfiles_repo = MockPackageRepository::new();
        dotfiles_repo
            .expect_find_package_files()
            .returning(|_| Ok(vec![PathBuf::from("/dotfiles/existing.yaml")]));

        let (_guard, path) = absent_dotfiles();
        let result = validate_unique_name(
            "existing",
            &package_repo,
            Some(&dotfiles_repo),
            &RealFileSystem,
            &path,
        );
        assert!(matches!(
            result,
            Err(NamespaceValidationError::Conflict(NamespaceConflict {
                found_in: NameLocation::Dotfiles,
                ..
            }))
        ));
    }

    #[test]
    fn test_no_dotfiles_repo_is_ok() {
        let mut package_repo = MockPackageRepository::new();
        package_repo
            .expect_find_package_files()
            .returning(|_| Ok(vec![]));

        let no_dotfiles: Option<&MockPackageRepository> = None;
        let (_guard, path) = absent_dotfiles();
        let result = validate_unique_name(
            "new-pkg",
            &package_repo,
            no_dotfiles,
            &RealFileSystem,
            &path,
        );
        assert!(result.is_ok());
    }

    #[test]
    fn test_package_repo_error_propagates() {
        let mut package_repo = MockPackageRepository::new();
        package_repo.expect_find_package_files().returning(|_| {
            Err(PackageListError::PackageDirectoryNotFound(
                "/packages".into(),
            ))
        });

        let (_guard, path) = absent_dotfiles();
        let result = validate_unique_name(
            "foo",
            &package_repo,
            None::<&MockPackageRepository>,
            &RealFileSystem,
            &path,
        );
        assert!(matches!(
            result,
            Err(NamespaceValidationError::LookupFailed(_))
        ));
    }

    // A dotfiles directory that is genuinely not there holds no names, so the name
    // is free and the caller may create it. This is the only listing failure that
    // answers the question.
    #[test]
    fn a_dotfiles_directory_that_is_not_there_leaves_the_name_free() {
        let mut package_repo = MockPackageRepository::new();
        package_repo
            .expect_find_package_files()
            .returning(|_| Ok(vec![]));

        let mut dotfiles_repo = MockPackageRepository::new();
        dotfiles_repo.expect_find_package_files().returning(|_| {
            Err(PackageListError::PackageDirectoryNotFound(
                "/dotfiles".into(),
            ))
        });

        let (_guard, path) = absent_dotfiles();
        let result = validate_unique_name(
            "foo",
            &package_repo,
            Some(&dotfiles_repo),
            &RealFileSystem,
            &path,
        );
        assert!(result.is_ok());
    }

    // The defect. A dotfiles directory that cannot be listed may already hold the
    // name, and reporting the name free let the caller create a second spec for it.
    // The refusal names the directory as the problem, so the user does not retype
    // the name expecting a different answer.
    #[test]
    fn a_dotfiles_directory_that_cannot_be_listed_refuses_rather_than_reporting_the_name_free() {
        let dir = tempdir().unwrap();
        let unreadable = dir.path().join("dotfiles");
        std::fs::create_dir(&unreadable).unwrap();
        std::fs::set_permissions(
            &unreadable,
            std::os::unix::fs::PermissionsExt::from_mode(0o000),
        )
        .unwrap();
        // Root reads a 0o000 directory, so the fixture cannot be built for a
        // privileged user and the case is skipped rather than passing vacuously.
        if std::fs::read_dir(&unreadable).is_ok() {
            eprintln!("SKIP: this user can list a 0o000 directory");
            return;
        }

        let mut package_repo = MockPackageRepository::new();
        package_repo
            .expect_find_package_files()
            .returning(|_| Ok(vec![]));
        let mut dotfiles_repo = MockPackageRepository::new();
        dotfiles_repo.expect_find_package_files().returning(|_| {
            Err(PackageListError::IoError(Arc::new(std::io::Error::from(
                std::io::ErrorKind::PermissionDenied,
            ))))
        });

        let result = validate_unique_name(
            "foo",
            &package_repo,
            Some(&dotfiles_repo),
            &RealFileSystem,
            &unreadable,
        );

        let Err(NamespaceValidationError::DotfilesDirectoryUnreadable(message)) = result else {
            panic!("expected a refusal, got {result:?}");
        };
        assert!(message.contains("could not be listed"), "{message}");
    }

    // A symlink loop is no better known than an unreadable directory, so it refuses
    // too. The wording differs because nothing established a directory is there.
    #[test]
    fn a_dotfiles_directory_that_cannot_be_checked_refuses() {
        let dir = tempdir().unwrap();
        let link = dir.path().join("dotfiles");
        std::os::unix::fs::symlink(&link, &link).unwrap();

        let mut package_repo = MockPackageRepository::new();
        package_repo
            .expect_find_package_files()
            .returning(|_| Ok(vec![]));
        let mut dotfiles_repo = MockPackageRepository::new();
        // The error a real listing gives this shape, rather than a chosen kind: a
        // hand-picked `PermissionDenied` classifies as unlistable and would test
        // the arm above instead of this one.
        let listing_error = Arc::new(std::fs::read_dir(&link).unwrap_err());
        dotfiles_repo
            .expect_find_package_files()
            .returning(move |_| Err(PackageListError::IoError(listing_error.clone())));

        let result = validate_unique_name(
            "foo",
            &package_repo,
            Some(&dotfiles_repo),
            &RealFileSystem,
            &link,
        );

        let Err(NamespaceValidationError::DotfilesDirectoryUnreadable(message)) = result else {
            panic!("expected a refusal, got {result:?}");
        };
        assert!(message.contains("could not be checked"), "{message}");
    }

    // The refusal a user reads has to say what to fix. "Cannot tell whether the
    // name is already taken" is the whole point: the name may be fine.
    #[test]
    fn the_unreadable_refusal_says_the_answer_is_unknown_not_that_the_name_is_taken() {
        let rendered = NamespaceValidationError::DotfilesDirectoryUnreadable(
            "could not be listed: denied".to_string(),
        )
        .to_string();

        assert!(
            rendered.contains("cannot tell whether the name is already taken"),
            "{rendered}"
        );
        assert!(!rendered.contains("already exists"), "{rendered}");
    }

    #[test]
    fn test_display_formatting() {
        let conflict = NamespaceConflict {
            name: "starship".to_string(),
            found_in: NameLocation::Packages,
        };
        assert_eq!(
            conflict.to_string(),
            "name 'starship' already exists in packages"
        );
    }
}
