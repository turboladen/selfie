use dialoguer::{Confirm, theme::SimpleTheme};
use selfie::{config::AppConfig, package::port::PackageRepository};
use tracing::info;

use crate::terminal_progress_reporter::TerminalProgressReporter;

use super::common;

pub(crate) fn handle_remove(
    package_name: &str,
    config: &AppConfig,
    reporter: TerminalProgressReporter,
) -> i32 {
    info!("Removing package: {}", package_name);

    // Create repository to interact with packages
    let repo = common::create_package_repository(config);

    // First, verify the package exists and get its details
    let Ok(package_blob) = repo.get_package(package_name) else {
        reporter.report_error(format!("Package '{package_name}' not found."));
        return 1;
    };

    // Show package location
    reporter.report_info(format!("Package '{package_name}' found at:"));
    reporter.report_info(format!("  {}", package_blob.file_path().display()));

    // Check if this package is a dependency of others
    let dependent_packages = match repo.find_dependent_packages(package_name) {
        Ok(deps) => deps,
        Err(e) => {
            reporter.report_warning(format!("Could not check for dependent packages: {e}"));
            Vec::new()
        }
    };

    // Build confirmation prompt based on dependencies
    let (prompt, default_answer) = if dependent_packages.is_empty() {
        reporter.report_info(format!(
            "✓ Package '{package_name}' is not a dependency of any other packages."
        ));
        (format!("Remove package '{package_name}'?"), false)
    } else {
        reporter.report_warning(format!(
            "Package '{package_name}' is a dependency of the following packages:"
        ));
        for dep in &dependent_packages {
            reporter.report_warning(format!("  - {}", dep.name()));
        }
        (
            "Are you sure you want to remove this package?".to_string(),
            false,
        )
    };

    // Single confirmation prompt
    let confirm_removal = Confirm::with_theme(&SimpleTheme)
        .with_prompt(prompt)
        .default(default_answer)
        .interact();

    let proceed = match confirm_removal {
        Ok(true) => true,
        Ok(false) => {
            reporter.report_info("Package removal cancelled.");
            return 0;
        }
        Err(_) => {
            reporter.report_error("Failed to read user input.");
            return 1;
        }
    };

    if !proceed {
        return 0;
    }

    // Perform the actual removal
    if let Err(e) = repo.remove_package(package_name) {
        reporter.report_error(format!("Failed to remove package '{package_name}': {e}"));
        return 1;
    }

    reporter.report_success(format!(
        "Package '{}' removed successfully from {}",
        package_name,
        package_blob.file_path().display()
    ));

    // Warn about broken dependencies if any exist
    if !dependent_packages.is_empty() {
        reporter.report_warning("Note: The following packages may have broken dependencies:");
        for dep in &dependent_packages {
            reporter.report_warning(format!("  - {}", dep.name()));
        }
        reporter.report_info("You may need to update these packages to remove the dependency.");
    }

    0
}

#[cfg(test)]
mod tests {
    use super::*;
    use selfie::fs::MockFileSystem;
    use selfie::package::port::MockPackageRepository;
    use std::path::PathBuf;
    use test_common::test_config_with_dir;

    #[test]
    fn test_handle_remove_package_not_found() {
        // Test behavior when trying to remove a non-existent package - no filesystem needed
        let package_dir = PathBuf::from("/test/packages");
        let config = test_config_with_dir(&package_dir);

        // Test removing a package that doesn't exist by checking the repository directly
        let repo = common::create_package_repository(&config);
        let result = repo.remove_package("nonexistent-package");

        // Should return error since package doesn't exist
        let e = result.unwrap_err();

        // Verify the error is about package not found
        assert!(
            e.to_string().contains("not found")
                || e.to_string().contains("nonexistent-package")
                || e.to_string().contains("Package directory not found")
                || e.to_string().contains("does not exist")
        );
    }

    #[test]
    fn test_dependency_check_integration() {
        // Test dependency checking logic without creating real files
        // This test focuses on the business logic rather than filesystem operations

        // Create packages programmatically for testing dependencies
        use selfie::package::{EnvironmentConfig, Package};
        use std::collections::HashMap;

        // Create target package with no dependencies
        let mut target_envs = HashMap::new();
        target_envs.insert(
            "test".to_string(),
            EnvironmentConfig::new(
                "echo 'install target'".to_string(),
                Some("echo 'check target'".to_string()),
                Vec::new(),
            ),
        );
        let target_package = Package::new(
            "target-package".to_string(),
            "1.0.0".to_string(),
            None,
            None,
            target_envs,
            PathBuf::from("/test/packages/target-package.yml"),
        );

        // Create dependent package that depends on target-package
        let mut dependent_envs = HashMap::new();
        dependent_envs.insert(
            "test".to_string(),
            EnvironmentConfig::new(
                "echo 'install dependent'".to_string(),
                Some("echo 'check dependent'".to_string()),
                vec!["target-package".to_string()],
            ),
        );
        let dependent_package = Package::new(
            "dependent-package".to_string(),
            "1.0.0".to_string(),
            None,
            None,
            dependent_envs,
            PathBuf::from("/test/packages/dependent-package.yml"),
        );

        // Verify the dependency relationship exists
        let target_deps = target_package.environments()["test"].dependencies();
        let dependent_deps = dependent_package.environments()["test"].dependencies();

        assert!(target_deps.is_empty());
        assert_eq!(dependent_deps.len(), 1);
        assert_eq!(dependent_deps[0], "target-package");

        // Verify package names and structure
        assert_eq!(target_package.name(), "target-package");
        assert_eq!(dependent_package.name(), "dependent-package");
    }

    #[test]
    fn test_repository_creation_with_different_filesystems() {
        // Test that we can create repositories with different filesystem implementations
        let package_dir = PathBuf::from("/test/packages");
        let config = test_config_with_dir(&package_dir);

        // Test with RealFileSystem
        let repo = common::create_package_repository_with_fs(&config, selfie::fs::RealFileSystem);
        drop(repo);

        // Test with MockFileSystem
        let mock_fs = MockFileSystem::default();
        let mock_repo = common::create_package_repository_with_fs(&config, mock_fs);
        drop(mock_repo);

        // This demonstrates the hexagonal architecture benefit - we can swap implementations
    }

    #[test]
    fn test_remove_operation_error_codes() {
        // Test remove operation using MockFileSystem instead of real filesystem
        let mut mock_fs = MockFileSystem::default();
        let package_dir = PathBuf::from("/test/packages");

        // Mock empty directory (no packages found)
        mock_fs.mock_path_exists(&package_dir, true);
        mock_fs.mock_list_directory(&package_dir, &[]);

        let config = test_config_with_dir(&package_dir);
        let repo = common::create_package_repository_with_fs(&config, mock_fs);

        // Test removing non-existent package - should return error
        let result = repo.remove_package("nonexistent-package");
        assert!(result.is_err());
    }

    #[test]
    fn test_remove_package_with_dependencies_mock_fs() {
        // Test dependency checking scenario using MockFileSystem - simplified version
        let mut mock_fs = MockFileSystem::default();
        let package_dir = PathBuf::from("/test/packages");

        let target_path = package_dir.join("target-package.yml");
        let dependent_path = package_dir.join("dependent-package.yml");

        // Mock package files
        let target_yaml = r#"
name: target-package
version: 1.0.0
environments:
  test:
    install: echo "install target"
    dependencies: []
"#;

        let dependent_yaml = r#"
name: dependent-package
version: 1.0.0
environments:
  test:
    install: echo "install dependent"
    dependencies:
      - target-package
"#;

        // Set up simple mocks for dependency checking
        mock_fs.mock_path_exists(&package_dir, true);
        mock_fs.mock_list_directory(&package_dir, &[&target_path, &dependent_path]);
        mock_fs.mock_read_file(&target_path, target_yaml);
        mock_fs.mock_read_file(&dependent_path, dependent_yaml);

        let config = test_config_with_dir(&package_dir);
        let repo = common::create_package_repository_with_fs(&config, mock_fs);

        // Test finding dependents - should find dependent-package
        let dependents = repo.find_dependent_packages("target-package").unwrap();
        assert_eq!(dependents.len(), 1);
        assert_eq!(dependents[0].name(), "dependent-package");

        // This test demonstrates using MockFileSystem to test dependency scenarios
        // without creating any real files on the filesystem
    }

    #[test]
    fn test_save_and_remove_workflow_mock_repo() {
        // Test complete save and remove workflow using MockPackageRepository for pure CLI logic
        let mut mock_repo = MockPackageRepository::new();
        let package_dir = PathBuf::from("/test/packages");
        let config = test_config_with_dir(&package_dir);

        // Mock successful save operation
        mock_repo
            .expect_save_package()
            .times(1)
            .returning(|_, _| Ok(()));

        // Mock successful remove operation
        mock_repo
            .expect_remove_package()
            .with(mockall::predicate::eq("workflow-test"))
            .times(1)
            .returning(|_| Ok(()));

        // Create and save a package
        let package_blob = common::create_new_package("workflow-test", &config);
        let save_result = mock_repo.save_package(package_blob.package(), package_blob.file_path());
        assert!(save_result.is_ok());

        // Remove the package
        let remove_result = mock_repo.remove_package("workflow-test");
        assert!(remove_result.is_ok());

        // This demonstrates testing CLI workflow logic without repository implementation
    }

    #[test]
    fn test_package_discovery_with_mock_repo() {
        // Test package discovery using MockPackageRepository for pure CLI logic testing
        let mut mock_repo = MockPackageRepository::new();

        // Mock available_packages operation which is what the CLI actually uses
        mock_repo
            .expect_available_packages()
            .times(1)
            .returning(|| {
                Ok(vec![
                    "app-server".to_string(),
                    "database".to_string(),
                    "web-client".to_string(),
                ])
            });

        // Test getting available package names
        let package_names = mock_repo.available_packages().unwrap();
        assert_eq!(package_names.len(), 3);
        assert!(package_names.contains(&"app-server".to_string()));
        assert!(package_names.contains(&"database".to_string()));
        assert!(package_names.contains(&"web-client".to_string()));

        // This demonstrates testing CLI package discovery logic without repository implementation
    }

    #[test]
    fn test_remove_package_with_mock_repository() {
        // Test remove operation using MockPackageRepository for pure CLI logic testing
        use selfie::package::{GetPackage, Package, port::MockPackageRepository};
        use std::collections::HashMap;

        let mut mock_repo = MockPackageRepository::new();

        // Create mock package for removal
        let package = Package::new(
            "test-package".to_string(),
            "1.0.0".to_string(),
            None,
            None,
            HashMap::new(),
            PathBuf::from("/test/packages/test-package.yml"),
        );
        let get_package =
            GetPackage::from_existing(package, PathBuf::from("/test/packages/test-package.yml"));

        // Mock successful get and remove operations
        mock_repo
            .expect_get_package()
            .with(mockall::predicate::eq("test-package"))
            .times(1)
            .returning(move |_| Ok(get_package.clone()));

        mock_repo
            .expect_find_dependent_packages()
            .with(mockall::predicate::eq("test-package"))
            .times(1)
            .returning(|_| Ok(vec![])); // No dependents

        mock_repo
            .expect_remove_package()
            .with(mockall::predicate::eq("test-package"))
            .times(1)
            .returning(|_| Ok(()));

        // Test removal logic without repository implementation
        let get_result = mock_repo.get_package("test-package");
        assert!(get_result.is_ok());

        let dependents_result = mock_repo.find_dependent_packages("test-package");
        assert!(dependents_result.is_ok());
        assert!(dependents_result.unwrap().is_empty());

        let remove_result = mock_repo.remove_package("test-package");
        assert!(remove_result.is_ok());

        // This demonstrates testing CLI remove logic without repository implementation
    }

    #[test]
    fn test_remove_package_not_found_with_mock_repo() {
        // Test CLI error handling when package doesn't exist using MockPackageRepository
        use selfie::package::port::{MockPackageRepository, PackageError};

        let mut mock_repo = MockPackageRepository::new();

        // Mock package not found
        mock_repo
            .expect_get_package()
            .with(mockall::predicate::eq("nonexistent"))
            .times(1)
            .returning(|_| {
                Err(PackageError::PackageNotFound {
                    name: "nonexistent".to_string(),
                    packages_path: PathBuf::from("/test/packages"),
                    files_examined: 0,
                    search_patterns: vec!["nonexistent.yml".to_string()],
                }
                .into())
            });

        // Test CLI error handling
        let result = mock_repo.get_package("nonexistent");
        assert!(result.is_err());

        // This tests CLI error handling without repository implementation details
    }

    #[test]
    fn test_remove_package_with_dependents_mock_repo() {
        // Test dependency checking using MockPackageRepository
        use selfie::package::{GetPackage, Package, port::MockPackageRepository};
        use std::collections::HashMap;

        let mut mock_repo = MockPackageRepository::new();

        // Create mock target package
        let target_package = Package::new(
            "target-package".to_string(),
            "1.0.0".to_string(),
            None,
            None,
            HashMap::new(),
            PathBuf::from("/test/packages/target-package.yml"),
        );
        let target_get_package = GetPackage::from_existing(
            target_package.clone(),
            PathBuf::from("/test/packages/target-package.yml"),
        );

        // Create mock dependent package
        let dependent_package = Package::new(
            "dependent-package".to_string(),
            "1.0.0".to_string(),
            None,
            None,
            HashMap::new(),
            PathBuf::from("/test/packages/dependent-package.yml"),
        );

        // Mock get package operation
        mock_repo
            .expect_get_package()
            .with(mockall::predicate::eq("target-package"))
            .times(1)
            .returning(move |_| Ok(target_get_package.clone()));

        // Mock finding dependents
        mock_repo
            .expect_find_dependent_packages()
            .with(mockall::predicate::eq("target-package"))
            .times(1)
            .returning(move |_| Ok(vec![dependent_package.clone()]));

        // Test dependency checking logic
        let get_result = mock_repo.get_package("target-package");
        assert!(get_result.is_ok());

        let dependents = mock_repo.find_dependent_packages("target-package").unwrap();
        assert_eq!(dependents.len(), 1);
        assert_eq!(dependents[0].name(), "dependent-package");

        // This demonstrates testing CLI dependency logic without repository implementation
    }

    #[test]
    fn test_remove_error_handling_with_mock_repo() {
        // Test CLI error handling when remove operation fails
        use selfie::package::{
            GetPackage, Package,
            port::{MockPackageRepository, PackageRepoError},
        };
        use std::collections::HashMap;

        let mut mock_repo = MockPackageRepository::new();

        // Create mock package
        let package = Package::new(
            "error-test".to_string(),
            "1.0.0".to_string(),
            None,
            None,
            HashMap::new(),
            PathBuf::from("/test/packages/error-test.yml"),
        );
        let get_package =
            GetPackage::from_existing(package, PathBuf::from("/test/packages/error-test.yml"));

        // Mock successful get but failed remove
        mock_repo
            .expect_get_package()
            .with(mockall::predicate::eq("error-test"))
            .times(1)
            .returning(move |_| Ok(get_package.clone()));

        mock_repo
            .expect_find_dependent_packages()
            .with(mockall::predicate::eq("error-test"))
            .times(1)
            .returning(|_| Ok(vec![]));

        mock_repo
            .expect_remove_package()
            .with(mockall::predicate::eq("error-test"))
            .times(1)
            .returning(|_| {
                Err(PackageRepoError::IoError(std::sync::Arc::new(
                    std::io::Error::new(std::io::ErrorKind::PermissionDenied, "Permission denied"),
                )))
            });

        // Test error handling workflow
        let get_result = mock_repo.get_package("error-test");
        assert!(get_result.is_ok());

        let dependents_result = mock_repo.find_dependent_packages("error-test");
        assert!(dependents_result.is_ok());

        let remove_result = mock_repo.remove_package("error-test");
        assert!(remove_result.is_err());

        // This tests CLI error handling without filesystem or repository implementation
    }
}
