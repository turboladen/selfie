//! Shared namespace validation for packages and standalone dotfiles
//!
//! Packages in `packages/` and standalone dotfiles in `dotfiles/` share a
//! single namespace — there must not be a `dotfiles/foo.yaml` AND a
//! `packages/foo.yaml`. This module provides validation to enforce that
//! constraint.

use std::fmt;

use crate::package::port::{PackageListError, PackageRepository};
use crate::parts::{Composed, Parts};

/// Location where a name was found during namespace validation
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NameLocation {
    /// Found in the packages directory
    Packages,
    /// Found in the standalone dotfiles directory
    Dotfiles,
}

/// What a caller can do about a [`NamespaceConflict`], besides choosing another
/// name, which it can always do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Remedy {
    /// The caller can only choose another name, because several spec files
    /// claim this one.
    ChooseAnotherName,
    /// Act on the one package spec that holds the name.
    UseThePackage,
    /// Remove the one standalone dotfile spec that holds the name.
    RemoveTheDotfileSpec,
}

/// Which spec files claim a name, by where they are and how many there are.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConflictKind {
    /// One package spec claims the name.
    Package,
    /// One standalone dotfile spec claims the name.
    DotfileSpec,
    /// Several package specs claim the name.
    SeveralPackages,
    /// Several standalone dotfile specs claim the name.
    SeveralDotfileSpecs,
}

/// Error returned when a name already exists in the shared namespace
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NamespaceConflict {
    /// The conflicting name
    pub name: String,
    /// Where the name was found
    pub found_in: NameLocation,
    // Never empty: `new` is the only way to build one.
    paths: Vec<std::path::PathBuf>,
}

impl NamespaceConflict {
    /// The conflict for `name`, claimed in `found_in` by the spec files at
    /// `paths`, or `None` when `paths` is empty and nothing claims it.
    #[must_use]
    pub fn new(
        name: impl Into<String>,
        found_in: NameLocation,
        paths: Vec<std::path::PathBuf>,
    ) -> Option<Self> {
        (!paths.is_empty()).then(|| Self {
            name: name.into(),
            found_in,
            paths,
        })
    }

    /// Every spec file that claims the name, in the order the lookup found
    /// them. Never empty.
    #[must_use]
    pub fn paths(&self) -> &[std::path::PathBuf] {
        &self.paths
    }

    /// Which spec files claim the name.
    fn kind(&self) -> ConflictKind {
        match (&self.found_in, self.paths.len() > 1) {
            (NameLocation::Packages, false) => ConflictKind::Package,
            (NameLocation::Dotfiles, false) => ConflictKind::DotfileSpec,
            (NameLocation::Packages, true) => ConflictKind::SeveralPackages,
            (NameLocation::Dotfiles, true) => ConflictKind::SeveralDotfileSpecs,
        }
    }

    /// What the caller can do about the conflict, for it to word in its own
    /// commands.
    #[must_use]
    pub fn remedy(&self) -> Remedy {
        // With several claimants the sentence already says the extra files must
        // go, and every command refuses the name until they do. Removing them
        // still leaves it taken, so another name is the only way forward.
        match self.kind() {
            ConflictKind::Package => Remedy::UseThePackage,
            ConflictKind::DotfileSpec => Remedy::RemoveTheDotfileSpec,
            ConflictKind::SeveralPackages | ConflictKind::SeveralDotfileSpecs => {
                Remedy::ChooseAnotherName
            }
        }
    }

    /// The files that claim the name, for a sentence: the path itself for one
    /// file, and each path quoted for several, so a comma or a space in a path
    /// cannot be mistaken for the separator.
    #[must_use]
    pub fn claimants(&self) -> String {
        match self.paths.as_slice() {
            [only] => only.display().to_string(),
            [rest @ .., last] => {
                let quoted =
                    |path: &std::path::PathBuf| format!("{:?}", path.display().to_string());
                let rest = rest.iter().map(quoted).collect::<Vec<_>>().join(", ");
                format!("{rest} and {}", quoted(last))
            }
            // `new` refuses an empty list, so no conflict reaches this arm.
            [] => String::new(),
        }
    }
}

/// The fact that the name is already taken, and by which files, as one
/// sentence. It carries no remedy: what to do about it depends on what the
/// caller was trying to do.
impl fmt::Display for NamespaceConflict {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (name, files, count) = (&self.name, self.claimants(), self.paths.len());
        match self.kind() {
            ConflictKind::DotfileSpec => {
                write!(f, "A dotfile spec named '{name}' already exists ({files}).")
            }
            ConflictKind::Package => write!(f, "'{name}' is already a package ({files})."),
            // Several files claiming one name is itself a problem: every command
            // that looks the name up refuses it until all but one are gone.
            ConflictKind::SeveralDotfileSpecs => write!(
                f,
                "{count} dotfile specs claim the name '{name}', {files}; all but one must be \
                 removed."
            ),
            ConflictKind::SeveralPackages => write!(
                f,
                "{count} package specs claim the name '{name}', {files}; all but one must be \
                 removed."
            ),
        }
    }
}

impl std::error::Error for NamespaceConflict {}

/// Errors that can occur during namespace validation
#[derive(Debug)]
pub enum NamespaceValidationError {
    /// The name conflicts with an existing entry
    Conflict(NamespaceConflict),
    /// The package directory could not be read, or something other than a
    /// directory is at its path. The listing error names the directory and says
    /// what is at it.
    PackageDirectoryUnreadable(PackageListError),
    /// The dotfiles directory could not be read, so whether the name is already
    /// taken is unknown. The listing error names the directory and says what is at
    /// it.
    DotfilesDirectoryUnreadable(PackageListError),
}

crate::parts::display_from_parts!(NamespaceValidationError);

impl Composed for NamespaceValidationError {
    fn parts(&self) -> Parts<'_> {
        match self {
            // The conflict's sentence is not built from parts, so it enters as
            // one text.
            Self::Conflict(conflict) => Parts::new().text(conflict.to_string()),
            // `error` leads with the directory's own path, so no noun goes in front
            // of it: "the dotfiles directory /home/me/dots could not be listed" reads
            // as two subjects.
            // Something that is not a directory holds no names, so the listing's own
            // sentence is the whole answer. Only a path selfie could not look into
            // leaves the name's status unknown.
            Self::PackageDirectoryUnreadable(error) if error.is_absent() => error.parts(),
            Self::PackageDirectoryUnreadable(error) | Self::DotfilesDirectoryUnreadable(error) => {
                Parts::new()
                    .text("cannot tell whether the name is already taken: ")
                    .append(error.parts())
            }
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
/// A failed dotfiles listing is judged by the state its error carries.
///
/// # Errors
///
/// [`NamespaceValidationError::Conflict`] if the name is taken in either
/// directory. [`NamespaceValidationError::PackageDirectoryUnreadable`] if the
/// package directory could not be read, or something other than a directory is at
/// its path. [`NamespaceValidationError::DotfilesDirectoryUnreadable`] if the dotfiles
/// directory's listing failed and its error does not report the directory absent,
/// because a name in a directory selfie cannot read is a name it cannot report as
/// free.
///
/// A package directory with nothing at its path, and a dotfiles directory that is
/// genuinely not there, hold no names, so neither is an error: `Ok(())` says the
/// name is free, and the caller may create it.
pub fn validate_unique_name(
    name: &str,
    package_repo: &impl PackageRepository,
    dotfiles_repo: Option<&impl PackageRepository>,
) -> Result<(), NamespaceValidationError> {
    // The package directory. Nothing at its path holds no names, and the first save
    // creates it, so the dotfiles directory is still asked. A file or a dangling link
    // there holds no names either, but the save's `create_dir_all` fails against it,
    // so it is refused here as `PackageRepoError::means_no_such_package` refuses it.
    let files = match package_repo.find_package_files(name) {
        Ok(files) => files,
        Err(error) if error.may_be_created() => Vec::new(),
        Err(error) => return Err(NamespaceValidationError::PackageDirectoryUnreadable(error)),
    };
    if let Some(conflict) = NamespaceConflict::new(name, NameLocation::Packages, files) {
        return Err(conflict.into());
    }

    // The dotfiles directory. A failed listing here must not be swallowed: answering
    // "the name is free" for a directory selfie could not read lets the caller create
    // a spec beside one that may already carry the name.
    if let Some(dotfiles) = dotfiles_repo {
        match dotfiles.find_package_files(name) {
            Ok(files) => {
                if let Some(conflict) = NamespaceConflict::new(name, NameLocation::Dotfiles, files)
                {
                    return Err(conflict.into());
                }
            }
            Err(error) => {
                // Nothing that can hold a spec is at the path, so it holds no names
                // and this one is free. Every other state may be hiding a spec that
                // already carries it, and the error words which and names the
                // directory, so nothing is re-derived here.
                if error.is_absent() {
                    return Ok(());
                }
                return Err(NamespaceValidationError::DotfilesDirectoryUnreadable(error));
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{path::PathBuf, sync::Arc};

    use tempfile::tempdir;

    use super::*;
    use crate::{
        fs::{AbsentReason, DirectoryState, RealFileSystem},
        package::port::{MockPackageRepository, PackageListError},
    };

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

        let result = validate_unique_name("new-pkg", &package_repo, Some(&dotfiles_repo));
        assert!(result.is_ok());
    }

    // The kind is where the claimants are and whether there are several. One
    // claimant's remedy follows where it is; several claimants leave only
    // another name, wherever they are.
    #[test]
    fn the_kind_and_remedy_follow_the_claimants() {
        let conflict = |found_in: NameLocation, paths: &[&str]| {
            NamespaceConflict::new("x", found_in, paths.iter().map(PathBuf::from).collect())
                .expect("a claimant")
        };
        let remedy = |found_in: NameLocation, paths: &[&str]| conflict(found_in, paths).remedy();
        let several = ["/s/x.yaml", "/s/x.yml"];

        assert_eq!(
            conflict(NameLocation::Packages, &["/p/x.yml"]).kind(),
            ConflictKind::Package
        );
        assert_eq!(
            conflict(NameLocation::Dotfiles, &["/d/x.yml"]).kind(),
            ConflictKind::DotfileSpec
        );
        assert_eq!(
            conflict(NameLocation::Packages, &several).kind(),
            ConflictKind::SeveralPackages
        );
        assert_eq!(
            conflict(NameLocation::Dotfiles, &several).kind(),
            ConflictKind::SeveralDotfileSpecs
        );

        assert_eq!(
            remedy(NameLocation::Packages, &["/p/x.yml"]),
            Remedy::UseThePackage
        );
        assert_eq!(
            remedy(NameLocation::Dotfiles, &["/d/x.yml"]),
            Remedy::RemoveTheDotfileSpec
        );
        for found_in in [NameLocation::Packages, NameLocation::Dotfiles] {
            assert_eq!(remedy(found_in, &several), Remedy::ChooseAnotherName);
        }
    }

    #[test]
    fn test_conflict_in_packages() {
        let mut package_repo = MockPackageRepository::new();
        package_repo.expect_find_package_files().returning(|_| {
            Ok(vec![
                PathBuf::from("/packages/Existing.yaml"),
                PathBuf::from("/packages/existing.yml"),
            ])
        });

        let result =
            validate_unique_name("existing", &package_repo, None::<&MockPackageRepository>);
        // Every file claiming the name is named, so removing one does not lead
        // to a second refusal for a file never mentioned.
        assert!(matches!(
            result,
            Err(NamespaceValidationError::Conflict(NamespaceConflict {
                found_in: NameLocation::Packages,
                ref paths,
                ..
            })) if *paths == [
                PathBuf::from("/packages/Existing.yaml"),
                PathBuf::from("/packages/existing.yml"),
            ]
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

        let result = validate_unique_name("existing", &package_repo, Some(&dotfiles_repo));
        assert!(matches!(
            result,
            Err(NamespaceValidationError::Conflict(NamespaceConflict {
                found_in: NameLocation::Dotfiles,
                ref paths,
                ..
            })) if *paths == [PathBuf::from("/dotfiles/existing.yaml")]
        ));
    }

    #[test]
    fn test_no_dotfiles_repo_is_ok() {
        let mut package_repo = MockPackageRepository::new();
        package_repo
            .expect_find_package_files()
            .returning(|_| Ok(vec![]));

        let no_dotfiles: Option<&MockPackageRepository> = None;
        let result = validate_unique_name("new-pkg", &package_repo, no_dotfiles);
        assert!(result.is_ok());
    }

    // The first package anyone creates has no package directory to list yet, and the
    // save creates it, so a directory that is not there leaves every name free.
    #[test]
    fn a_package_directory_that_is_not_there_leaves_the_name_free() {
        let mut package_repo = MockPackageRepository::new();
        package_repo.expect_find_package_files().returning(|_| {
            Err(PackageListError::new(
                "/packages".into(),
                DirectoryState::Absent(AbsentReason::Empty),
            ))
        });
        let mut dotfiles_repo = MockPackageRepository::new();
        dotfiles_repo
            .expect_find_package_files()
            .returning(|_| Ok(vec![]));

        let result = validate_unique_name("foo", &package_repo, Some(&dotfiles_repo));
        assert!(result.is_ok(), "got {result:?}");
    }

    // A missing package directory answers only for itself: the name may still be
    // taken by a standalone dotfile spec.
    #[test]
    fn a_package_directory_that_is_not_there_still_checks_the_dotfiles_directory() {
        let mut package_repo = MockPackageRepository::new();
        package_repo.expect_find_package_files().returning(|_| {
            Err(PackageListError::new(
                "/packages".into(),
                DirectoryState::Absent(AbsentReason::Empty),
            ))
        });
        let mut dotfiles_repo = MockPackageRepository::new();
        dotfiles_repo
            .expect_find_package_files()
            .returning(|_| Ok(vec![PathBuf::from("/dotfiles/foo.yaml")]));

        let result = validate_unique_name("foo", &package_repo, Some(&dotfiles_repo));
        assert!(
            matches!(
                result,
                Err(NamespaceValidationError::Conflict(NamespaceConflict {
                    found_in: NameLocation::Dotfiles,
                    ..
                }))
            ),
            "got {result:?}"
        );
    }

    // A file at the package directory's path holds no names, but the save cannot
    // create the directory through it, so the lookup still fails.
    #[test]
    fn a_file_at_the_package_directory_path_still_fails_the_lookup() {
        let mut package_repo = MockPackageRepository::new();
        package_repo.expect_find_package_files().returning(|_| {
            Err(PackageListError::new(
                "/packages".into(),
                DirectoryState::Absent(AbsentReason::Occupied {
                    kind: "regular file",
                }),
            ))
        });
        let mut dotfiles_repo = MockPackageRepository::new();
        dotfiles_repo
            .expect_find_package_files()
            .returning(|_| Ok(vec![]));

        let result = validate_unique_name("foo", &package_repo, Some(&dotfiles_repo));
        let Err(error @ NamespaceValidationError::PackageDirectoryUnreadable(_)) = result else {
            panic!("expected the lookup to fail, got {result:?}");
        };
        // The directory is what failed, so the sentence names it and says what is
        // there. A file holds no names, so the sentence does not claim the answer is
        // unknown either.
        let message = error.to_string();
        assert_eq!(
            message,
            "/packages is not a directory, it is a regular file"
        );
    }

    // A package directory selfie could not list may hold the name, so the refusal
    // says the answer is unknown and names the directory.
    #[test]
    fn a_package_directory_that_cannot_be_listed_leaves_the_name_unknown() {
        let mut package_repo = MockPackageRepository::new();
        package_repo.expect_find_package_files().returning(|_| {
            Err(PackageListError::new(
                "/packages".into(),
                DirectoryState::Unlistable(Arc::new(std::io::Error::from(
                    std::io::ErrorKind::PermissionDenied,
                ))),
            ))
        });
        let mut dotfiles_repo = MockPackageRepository::new();
        dotfiles_repo
            .expect_find_package_files()
            .returning(|_| Ok(vec![]));

        let result = validate_unique_name("foo", &package_repo, Some(&dotfiles_repo));
        let Err(error @ NamespaceValidationError::PackageDirectoryUnreadable(_)) = result else {
            panic!("expected the lookup to fail, got {result:?}");
        };
        let message = error.to_string();
        assert!(
            message.starts_with("cannot tell whether the name is already taken: /packages"),
            "{message}"
        );
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
            Err(PackageListError::new(
                "/dotfiles".into(),
                DirectoryState::Absent(AbsentReason::Empty),
            ))
        });

        let result = validate_unique_name("foo", &package_repo, Some(&dotfiles_repo));
        assert!(result.is_ok());
    }

    // The defect. A dotfiles directory that cannot be listed may already hold the
    // name, and reporting the name free let the caller create a second spec for it.
    // The refusal names the directory as the problem, so the user does not retype
    // the name expecting a different answer.
    #[test]
    fn a_dotfiles_directory_that_cannot_be_listed_refuses_rather_than_reporting_the_name_free() {
        let mut package_repo = MockPackageRepository::new();
        package_repo
            .expect_find_package_files()
            .returning(|_| Ok(vec![]));
        let mut dotfiles_repo = MockPackageRepository::new();
        dotfiles_repo.expect_find_package_files().returning(|_| {
            Err(PackageListError::new(
                "/dotfiles".into(),
                DirectoryState::Unlistable(Arc::new(std::io::Error::from(
                    std::io::ErrorKind::PermissionDenied,
                ))),
            ))
        });

        let result = validate_unique_name("foo", &package_repo, Some(&dotfiles_repo));

        let Err(NamespaceValidationError::DotfilesDirectoryUnreadable(error)) = result else {
            panic!("expected a refusal, got {result:?}");
        };
        let message = error.to_string();
        assert!(message.contains("could not be listed"), "{message}");
    }

    // A real directory whose listing fails with anything but a permission error
    // arrives as `DirectoryState::Directory`, because the path classifies perfectly
    // well. It could not be *listed*, and saying it could not be *checked* claims
    // less than selfie knows.
    #[test]
    fn a_directory_whose_listing_failed_says_it_could_not_be_listed() {
        let dir = tempdir().unwrap();

        let mut package_repo = MockPackageRepository::new();
        package_repo
            .expect_find_package_files()
            .returning(|_| Ok(vec![]));
        let mut dotfiles_repo = MockPackageRepository::new();
        // `Directory` is what the repository records for a path that classified cleanly
        // and whose listing still failed, and it is the arm this test is about.
        let listed = dir.path().to_path_buf();
        dotfiles_repo
            .expect_find_package_files()
            .returning(move |_| {
                Err(PackageListError::new(
                    listed.clone(),
                    DirectoryState::Directory,
                ))
            });

        let result = validate_unique_name("foo", &package_repo, Some(&dotfiles_repo));

        let Err(NamespaceValidationError::DotfilesDirectoryUnreadable(error)) = result else {
            panic!("expected a refusal, got {result:?}");
        };
        let message = error.to_string();
        assert!(message.contains("could not be listed"), "{message}");
        assert!(!message.contains("could not be checked"), "{message}");
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
        let looped = link.clone();
        dotfiles_repo
            .expect_find_package_files()
            .returning(move |_| {
                Err(PackageListError::from_listing(
                    &RealFileSystem,
                    looped.clone(),
                    &std::fs::read_dir(&looped).unwrap_err(),
                ))
            });

        let result = validate_unique_name("foo", &package_repo, Some(&dotfiles_repo));

        let Err(NamespaceValidationError::DotfilesDirectoryUnreadable(error)) = result else {
            panic!("expected a refusal, got {result:?}");
        };
        let message = error.to_string();
        assert!(message.contains("could not be checked"), "{message}");
    }

    // The refusal a user reads has to say what to fix. "Cannot tell whether the
    // name is already taken" is the whole point: the name may be fine.
    #[test]
    fn the_unreadable_refusal_says_the_answer_is_unknown_not_that_the_name_is_taken() {
        let rendered =
            NamespaceValidationError::DotfilesDirectoryUnreadable(PackageListError::new(
                "/dotfiles".into(),
                DirectoryState::Unlistable(Arc::new(std::io::Error::from(
                    std::io::ErrorKind::PermissionDenied,
                ))),
            ))
            .to_string();

        assert!(
            rendered.contains("cannot tell whether the name is already taken"),
            "{rendered}"
        );
        assert!(!rendered.contains("already exists"), "{rendered}");
    }

    // A path with a comma in it stays one path when several files are listed.
    #[test]
    fn several_claimants_are_each_quoted() {
        let conflict = NamespaceConflict::new(
            "rc",
            NameLocation::Dotfiles,
            vec![
                "/d/a, b/rc.yml".into(),
                "/d/Rc.yaml".into(),
                "/d/rc.yaml".into(),
            ],
        )
        .expect("three claimants");

        assert_eq!(
            conflict.to_string(),
            "3 dotfile specs claim the name 'rc', \"/d/a, b/rc.yml\", \"/d/Rc.yaml\" and \
             \"/d/rc.yaml\"; all but one must be removed."
        );
    }

    // A name no file claims is no conflict, so no sentence can list nothing.
    #[test]
    fn a_conflict_needs_a_claimant() {
        assert_eq!(
            NamespaceConflict::new("starship", NameLocation::Packages, Vec::new()),
            None
        );
    }

    // A conflict reads as the fact for its location, with no remedy: that is
    // the caller's to add.
    #[test]
    fn a_conflict_reads_as_the_taken_name_sentence() {
        let package = NamespaceConflict::new(
            "starship",
            NameLocation::Packages,
            vec!["/p/Starship.yaml".into(), "/p/starship.yml".into()],
        )
        .expect("two claimants");
        assert_eq!(
            package.to_string(),
            "2 package specs claim the name 'starship', \"/p/Starship.yaml\" and \
             \"/p/starship.yml\"; all but one must be removed."
        );
        let dotfile = NamespaceConflict::new(
            "starship",
            NameLocation::Dotfiles,
            vec!["/d/starship.yaml".into()],
        )
        .expect("one claimant");
        assert_eq!(
            dotfile.to_string(),
            "A dotfile spec named 'starship' already exists (/d/starship.yaml)."
        );
    }
}
