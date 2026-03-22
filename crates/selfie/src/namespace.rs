//! Shared namespace validation for packages and standalone dotfiles
//!
//! Packages in `packages/` and standalone dotfiles in `dotfiles/` share a
//! single namespace — there must not be a `dotfiles/foo.yaml` AND a
//! `packages/foo.yaml`. This module provides validation to enforce that
//! constraint.

use std::fmt;

use crate::package::port::PackageRepository;

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

/// Validate that a name is unique across both the package and dotfiles repositories.
///
/// Returns `Ok(())` if the name is not found in either repository, or
/// `Err(NamespaceConflict)` if it exists in one of them.
///
/// If either repository lookup fails (e.g., directory doesn't exist), the
/// failure is treated as "name not found" — this is intentional since the
/// `dotfiles/` directory is optional.
pub fn validate_unique_name(
    name: &str,
    package_repo: &impl PackageRepository,
    dotfiles_repo: Option<&impl PackageRepository>,
) -> Result<(), NamespaceConflict> {
    // Check packages directory
    if let Ok(files) = package_repo.find_package_files(name)
        && !files.is_empty()
    {
        return Err(NamespaceConflict {
            name: name.to_string(),
            found_in: NameLocation::Packages,
        });
    }

    // Check dotfiles directory (if present)
    if let Some(dotfiles) = dotfiles_repo
        && let Ok(files) = dotfiles.find_package_files(name)
        && !files.is_empty()
    {
        return Err(NamespaceConflict {
            name: name.to_string(),
            found_in: NameLocation::Dotfiles,
        });
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::package::port::MockPackageRepository;
    use std::path::PathBuf;

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

    #[test]
    fn test_conflict_in_packages() {
        let mut package_repo = MockPackageRepository::new();
        package_repo
            .expect_find_package_files()
            .returning(|_| Ok(vec![PathBuf::from("/packages/existing.yaml")]));

        let result =
            validate_unique_name("existing", &package_repo, None::<&MockPackageRepository>);
        assert_eq!(
            result,
            Err(NamespaceConflict {
                name: "existing".to_string(),
                found_in: NameLocation::Packages,
            })
        );
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
        assert_eq!(
            result,
            Err(NamespaceConflict {
                name: "existing".to_string(),
                found_in: NameLocation::Dotfiles,
            })
        );
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
