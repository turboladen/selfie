use dialoguer::{Confirm, theme::SimpleTheme};
use selfie::package::{
    event::{OperationResult, PackageEvent},
    port::PackageRepository,
    service::SpecService,
};

use crate::config::CliConfig;
use crate::display_manager::DisplayManager;
use crate::event_processor::EventProcessor;
use tracing::info;

use crate::commands::common;

pub(crate) async fn handle_remove(
    service: &impl SpecService,
    package_name: &str,
    config: &CliConfig,
    display: &DisplayManager,
) -> i32 {
    info!("Removing package: {}", package_name);

    // Pre-flight: verify the package exists and check dependencies for the confirmation prompt.
    // These read-only repo calls gather info for UX before we commit to removal via the service.
    // Note: service.remove() repeats the get_package + find_dependent_packages internally.
    // This is an intentional tradeoff — the service needs to be self-contained for non-CLI
    // consumers (like the MCP server) that skip confirmation, while the CLI needs dependency
    // info before prompting the user. A dry-run API could eliminate this but adds complexity.
    let repo = common::create_package_repository(config);

    let Ok(package_blob) = repo.get_package(package_name) else {
        display.print_error(format!("Package '{package_name}' not found."));
        return 1;
    };

    display.print_info(format!("Package '{package_name}' found at:"));
    display.print_info(format!("  {}", package_blob.file_path().display()));

    let dependent_packages = match repo.find_dependent_packages(package_name) {
        Ok(deps) => deps,
        Err(e) => {
            display.print_warning(format!("Could not check for dependent packages: {e}"));
            Vec::new()
        }
    };

    let (prompt, default_answer) = if dependent_packages.is_empty() {
        display.print_success(format!(
            "Package '{package_name}' is not a dependency of any other packages."
        ));
        (format!("Remove package '{package_name}'?"), false)
    } else {
        display.print_warning(format!(
            "Package '{package_name}' is a dependency of the following packages:"
        ));
        for dep in &dependent_packages {
            display.print_warning(format!("  - {}", dep.name()));
        }
        (
            "Are you sure you want to remove this package?".to_string(),
            false,
        )
    };

    let confirm_removal = Confirm::with_theme(&SimpleTheme)
        .with_prompt(prompt)
        .default(default_answer)
        .interact();

    match confirm_removal {
        Ok(true) => {}
        Ok(false) => {
            display.print_info("Package removal cancelled.");
            return 0;
        }
        Err(_) => {
            display.print_error("Failed to read user input.");
            return 1;
        }
    }

    // Perform the actual removal through the service layer
    let event_stream = service.remove(package_name).await;

    let processor = EventProcessor::new(display.clone());
    let result = processor
        .process_events(event_stream, |event| match event {
            PackageEvent::RemovalDependencyInfo { .. } => {
                // Already displayed dependency info above before confirmation
                true
            }
            PackageEvent::Progress { .. } => true,
            PackageEvent::Completed { result, .. } => {
                if let OperationResult::Success(_) = result {
                    display.print_success(format!(
                        "Package '{}' removed successfully from {}",
                        package_name,
                        package_blob.file_path().display()
                    ));

                    if !dependent_packages.is_empty() {
                        display.print_warning(
                            "Note: The following packages may have broken dependencies:",
                        );
                        for dep in &dependent_packages {
                            display.print_warning(format!("  - {}", dep.name()));
                        }
                        display.print_info(
                            "You may need to update these packages to remove the dependency.",
                        );
                    }
                    return true;
                }
                false // Let default handling show the error
            }
            _ => false,
        })
        .await;

    result.exit_code
}

#[cfg(test)]
mod tests {
    use selfie::fs::MockFileSystem;
    use selfie::package::port::MockPackageRepository;
    use std::path::PathBuf;
    use test_common::test_config_with_dir;

    use crate::config::CliConfig;

    use crate::commands::common;

    #[test]
    fn test_dependency_check_integration() {
        use selfie::package::{EnvironmentConfig, Package};
        use std::collections::HashMap;

        let mut target_envs = HashMap::new();
        target_envs.insert(
            "test".to_string(),
            EnvironmentConfig::new(
                "echo 'install target'".to_string(),
                Some("echo 'check target'".to_string()),
                None,
                Vec::new(),
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

        let mut dependent_envs = HashMap::new();
        dependent_envs.insert(
            "test".to_string(),
            EnvironmentConfig::new(
                "echo 'install dependent'".to_string(),
                Some("echo 'check dependent'".to_string()),
                None,
                vec!["target-package".to_string()],
                Vec::new(),
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

        let target_deps = target_package.environments()["test"].dependencies();
        let dependent_deps = dependent_package.environments()["test"].dependencies();

        assert!(target_deps.is_empty());
        assert_eq!(dependent_deps.len(), 1);
        assert_eq!(dependent_deps[0], "target-package");

        assert_eq!(target_package.name(), "target-package");
        assert_eq!(dependent_package.name(), "dependent-package");
    }

    #[test]
    fn test_repository_creation_with_different_filesystems() {
        let package_dir = PathBuf::from("/test/packages");
        let config = CliConfig::wrap_for_test(test_config_with_dir(&package_dir));

        let repo = common::create_package_repository_with_fs(&config, selfie::fs::RealFileSystem);
        drop(repo);

        let mock_fs = MockFileSystem::default();
        let mock_repo = common::create_package_repository_with_fs(&config, mock_fs);
        drop(mock_repo);
    }

    #[test]
    fn test_remove_operation_error_codes() {
        let mut mock_fs = MockFileSystem::default();
        let package_dir = PathBuf::from("/test/packages");

        mock_fs.mock_path_exists(&package_dir, true);
        mock_fs.mock_list_directory(&package_dir, &[]);

        let config = CliConfig::wrap_for_test(test_config_with_dir(&package_dir));
        let repo = common::create_package_repository_with_fs(&config, mock_fs);

        use selfie::package::port::PackageRepository;
        let result = repo.remove_package("nonexistent-package");
        assert!(result.is_err());
    }

    #[test]
    fn test_remove_package_with_dependencies_mock_fs() {
        let mut mock_fs = MockFileSystem::default();
        let package_dir = PathBuf::from("/test/packages");

        let target_path = package_dir.join("target-package.yml");
        let dependent_path = package_dir.join("dependent-package.yml");

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

        mock_fs.mock_path_exists(&package_dir, true);
        mock_fs.mock_list_directory(&package_dir, &[&target_path, &dependent_path]);
        mock_fs.mock_read_file(&target_path, target_yaml);
        mock_fs.mock_read_file(&dependent_path, dependent_yaml);

        let config = CliConfig::wrap_for_test(test_config_with_dir(&package_dir));
        let repo = common::create_package_repository_with_fs(&config, mock_fs);

        use selfie::package::port::PackageRepository;
        let dependents = repo.find_dependent_packages("target-package").unwrap();
        assert_eq!(dependents.len(), 1);
        assert_eq!(dependents[0].name(), "dependent-package");
    }

    #[test]
    fn test_save_and_remove_workflow_mock_repo() {
        use selfie::package::port::PackageRepository;

        let mut mock_repo = MockPackageRepository::new();
        let package_dir = PathBuf::from("/test/packages");
        let config = CliConfig::wrap_for_test(test_config_with_dir(&package_dir));

        mock_repo
            .expect_save_package()
            .times(1)
            .returning(|_, _| Ok(()));

        mock_repo
            .expect_remove_package()
            .with(mockall::predicate::eq("workflow-test"))
            .times(1)
            .returning(|_| Ok(()));

        let package_blob = common::create_new_package("workflow-test", &config);
        let save_result = mock_repo.save_package(package_blob.package(), package_blob.file_path());
        assert!(save_result.is_ok());

        let remove_result = mock_repo.remove_package("workflow-test");
        assert!(remove_result.is_ok());
    }

    #[test]
    fn test_package_discovery_with_mock_repo() {
        use selfie::package::port::PackageRepository;

        let mut mock_repo = MockPackageRepository::new();

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

        let package_names = mock_repo.available_packages().unwrap();
        assert_eq!(package_names.len(), 3);
        assert!(package_names.contains(&"app-server".to_string()));
    }

    #[test]
    fn test_remove_package_not_found_with_mock_repo() {
        use selfie::package::port::{MockPackageRepository, PackageError, PackageRepository};

        let mut mock_repo = MockPackageRepository::new();

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

        let result = mock_repo.get_package("nonexistent");
        assert!(result.is_err());
    }
}
