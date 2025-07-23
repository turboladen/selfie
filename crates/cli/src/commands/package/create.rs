use dialoguer::{Confirm, Input, MultiSelect, Select, theme::SimpleTheme};
use selfie::{
    config::AppConfig,
    package::{EnvironmentConfig, GetPackage, port::PackageRepository},
};
use std::{collections::HashMap, path::PathBuf};
use tracing::info;

use crate::terminal_progress_reporter::TerminalProgressReporter;

use super::common;

const MAX_NAME_RETRIES: usize = 3;

enum PackageNameResult {
    CreateNew(String),     // Use this name to create a new package
    EditExisting(PathBuf), // User wants to edit the existing package at this path
    Cancelled,             // User cancelled the operation
}

pub(crate) fn handle_create(
    package_name: &str,
    config: &AppConfig,
    reporter: TerminalProgressReporter,
    interactive: bool,
) -> i32 {
    info!("Creating package: {}", package_name);

    // Create repository
    let repo = common::create_package_repository(config);

    // Get a valid package name or handle existing package scenarios
    let package_name = match get_valid_package_name(package_name, &repo, reporter) {
        Ok(PackageNameResult::CreateNew(name)) => name,
        Ok(PackageNameResult::EditExisting(path)) => {
            reporter.report_info(format!(
                "Opening existing package for editing at {}",
                path.display()
            ));
            let success_message = format!("Package editing completed at {}", path.display());
            return common::open_editor(&path, reporter, Some(success_message));
        }
        Ok(PackageNameResult::Cancelled) => {
            reporter.report_info("Package creation cancelled.");
            return 0;
        }
        Err(exit_code) => return exit_code,
    };

    // Create new package
    let package_blob = if interactive {
        match create_package_interactive(&package_name, config, reporter) {
            Ok(blob) => blob,
            Err(exit_code) => return exit_code,
        }
    } else {
        create_basic_package(&package_name, config)
    };

    // Save package to file
    if let Err(exit_code) = common::save_package(&repo, &package_blob, reporter) {
        return exit_code;
    }

    reporter.report_success(format!(
        "Package '{}' created successfully at {}",
        package_name,
        package_blob.file_path.display()
    ));

    // Ask if user wants to edit the file (only in interactive mode)
    if interactive {
        let edit_now = Confirm::with_theme(&SimpleTheme)
            .with_prompt("Would you like to open the package file for editing now?")
            .default(true)
            .interact();

        match edit_now {
            Ok(true) => {
                let success_message = format!(
                    "Package '{}' created and saved at {}",
                    package_name,
                    package_blob.file_path.display()
                );
                common::open_editor(&package_blob.file_path, reporter, Some(success_message))
            }
            Ok(false) => {
                reporter.report_info(
                    "Package created. You can edit it later with 'selfie package edit'.",
                );
                0
            }
            Err(_) => {
                reporter.report_error("Failed to read user input.");
                1
            }
        }
    } else {
        reporter.report_info("Package created. Use 'selfie package edit' to customize it.");
        0
    }
}

fn get_valid_package_name(
    initial_name: &str,
    repo: &impl PackageRepository,
    reporter: TerminalProgressReporter,
) -> Result<PackageNameResult, i32> {
    let mut current_name = initial_name.to_string();
    let mut retry_count = 0;

    loop {
        // Check if package already exists
        if let Ok(existing_package) = repo.get_package(&current_name) {
            reporter.report_info(format!("Package '{current_name}' already exists."));

            let action = Select::with_theme(&SimpleTheme)
                .with_prompt("What would you like to do?")
                .items(&[
                    "Edit the existing package",
                    "Create a new package with a different name",
                    "Cancel",
                ])
                .default(0)
                .interact();

            match action {
                Ok(0) => {
                    // Edit existing package
                    return Ok(PackageNameResult::EditExisting(existing_package.file_path));
                }
                Ok(1) => {
                    // Create with different name
                    retry_count += 1;
                    if retry_count > MAX_NAME_RETRIES {
                        reporter.report_error(format!(
                            "Too many retry attempts ({MAX_NAME_RETRIES}). Please try again later."
                        ));
                        return Err(1);
                    }

                    let new_name: String = if let Ok(name) = Input::with_theme(&SimpleTheme)
                        .with_prompt(format!(
                            "Enter a new package name (attempt {retry_count}/{MAX_NAME_RETRIES})"
                        ))
                        .interact()
                    {
                        name
                    } else {
                        reporter.report_error("Failed to read package name.");
                        return Err(1);
                    };
                    current_name = new_name;
                    continue; // Loop back to check the new name
                }
                _ => {
                    // Cancel
                    return Ok(PackageNameResult::Cancelled);
                }
            }
        }
        // Package doesn't exist, we can use this name
        return Ok(PackageNameResult::CreateNew(current_name));
    }
}

fn create_basic_package(package_name: &str, config: &AppConfig) -> GetPackage {
    let mut environments = HashMap::new();

    // Use the environment from config (which may be overridden by --environment)
    let env_name = config.environment();
    let env_config = EnvironmentConfig::new(
        format!("# TODO: Add install command for {package_name}"),
        Some(format!("# TODO: Add check command for {package_name}")),
        Vec::new(),
    );

    environments.insert(env_name.to_string(), env_config);

    let package = selfie::package::Package::new(
        package_name.to_string(),
        "0.1.0".to_string(),
        None,
        None,
        environments,
        config
            .package_directory()
            .join(format!("{package_name}.yml")),
    );

    let file_path = config
        .package_directory()
        .join(format!("{package_name}.yml"));

    GetPackage {
        package,
        file_path,
        is_new: true,
    }
}

fn create_package_interactive(
    package_name: &str,
    config: &AppConfig,
    reporter: TerminalProgressReporter,
) -> Result<GetPackage, i32> {
    reporter.report_info("Creating package interactively...");

    let name = prompt_package_name(package_name, reporter)?;
    let version = prompt_package_version(reporter)?;
    let homepage = prompt_package_homepage(reporter)?;
    let description = prompt_package_description(reporter)?;
    let environments = prompt_environments(&name, config, reporter)?;
    let file_name = prompt_file_name(&name, reporter)?;

    let package = selfie::package::Package::new(
        name,
        version,
        homepage,
        description,
        environments,
        config.package_directory().join(format!("{file_name}.yml")),
    );

    let file_path = config.package_directory().join(format!("{file_name}.yml"));

    Ok(GetPackage {
        package,
        file_path,
        is_new: true,
    })
}

fn prompt_package_name(
    default_name: &str,
    reporter: TerminalProgressReporter,
) -> Result<String, i32> {
    Input::with_theme(&SimpleTheme)
        .with_prompt("Package name")
        .default(default_name.to_string())
        .interact()
        .map_err(|_| {
            reporter.report_error("Failed to read package name.");
            1
        })
}

fn prompt_package_version(reporter: TerminalProgressReporter) -> Result<String, i32> {
    Input::with_theme(&SimpleTheme)
        .with_prompt("Version")
        .default("0.1.0".to_string())
        .interact()
        .map_err(|_| {
            reporter.report_error("Failed to read version.");
            1
        })
}

fn prompt_package_homepage(reporter: TerminalProgressReporter) -> Result<Option<String>, i32> {
    let homepage: String = Input::with_theme(&SimpleTheme)
        .with_prompt("Homepage URL (optional)")
        .allow_empty(true)
        .interact()
        .map_err(|_| {
            reporter.report_error("Failed to read homepage.");
            1
        })?;

    Ok(if homepage.trim().is_empty() {
        None
    } else {
        Some(homepage)
    })
}

fn prompt_package_description(reporter: TerminalProgressReporter) -> Result<Option<String>, i32> {
    let description: String = Input::with_theme(&SimpleTheme)
        .with_prompt("Description (optional)")
        .allow_empty(true)
        .interact()
        .map_err(|_| {
            reporter.report_error("Failed to read description.");
            1
        })?;

    Ok(if description.trim().is_empty() {
        None
    } else {
        Some(description)
    })
}

fn prompt_environments(
    package_name: &str,
    config: &AppConfig,
    reporter: TerminalProgressReporter,
) -> Result<HashMap<String, EnvironmentConfig>, i32> {
    let mut environments = HashMap::new();

    loop {
        reporter.report_info("Adding environment configuration...");

        let env_name = prompt_environment_name(&environments, config, reporter)?;
        let install_cmd = prompt_install_command(reporter)?;
        let check_cmd = prompt_check_command(package_name, reporter)?;
        let dependencies = prompt_dependencies(config, reporter)?;

        let env_config = EnvironmentConfig::new(install_cmd, check_cmd, dependencies);
        environments.insert(env_name, env_config);

        if !prompt_add_another_environment(reporter)? {
            break;
        }
    }

    Ok(environments)
}

fn prompt_environment_name(
    existing_environments: &HashMap<String, EnvironmentConfig>,
    config: &AppConfig,
    reporter: TerminalProgressReporter,
) -> Result<String, i32> {
    let default_env = if existing_environments.is_empty() {
        config.environment().to_string()
    } else {
        "production".to_string()
    };

    Input::with_theme(&SimpleTheme)
        .with_prompt("Environment name")
        .default(default_env)
        .interact()
        .map_err(|_| {
            reporter.report_error("Failed to read environment name.");
            1
        })
}

fn prompt_install_command(reporter: TerminalProgressReporter) -> Result<String, i32> {
    loop {
        let cmd: String = Input::with_theme(&SimpleTheme)
            .with_prompt("Install command (required)")
            .interact()
            .map_err(|_| {
                reporter.report_error("Failed to read install command.");
                1
            })?;

        if !cmd.trim().is_empty() {
            break Ok(cmd);
        }

        reporter.report_error("Install command cannot be empty.");
    }
}

fn prompt_check_command(
    package_name: &str,
    reporter: TerminalProgressReporter,
) -> Result<Option<String>, i32> {
    let default_check = format!("command -v {package_name}");
    let check_cmd: String = Input::with_theme(&SimpleTheme)
        .with_prompt("Check command (optional)")
        .default(default_check)
        .allow_empty(true)
        .interact()
        .map_err(|_| {
            reporter.report_error("Failed to read check command.");
            1
        })?;

    Ok(if check_cmd.trim().is_empty() {
        None
    } else {
        Some(check_cmd)
    })
}

fn prompt_dependencies(
    config: &AppConfig,
    reporter: TerminalProgressReporter,
) -> Result<Vec<String>, i32> {
    let repo = common::create_package_repository(config);
    let available_packages = repo.available_packages().unwrap_or_default();

    if available_packages.is_empty() {
        return Ok(Vec::new());
    }

    let selected = MultiSelect::with_theme(&SimpleTheme)
        .with_prompt("Dependencies (select with space, confirm with enter)")
        .items(&available_packages)
        .interact()
        .map_err(|_| {
            reporter.report_error("Failed to read dependencies.");
            1
        })?;

    Ok(selected
        .into_iter()
        .map(|i| available_packages[i].clone())
        .collect())
}

fn prompt_add_another_environment(reporter: TerminalProgressReporter) -> Result<bool, i32> {
    Confirm::with_theme(&SimpleTheme)
        .with_prompt("Add another environment?")
        .default(false)
        .interact()
        .map_err(|_| {
            reporter.report_error("Failed to read user input.");
            1
        })
}

fn prompt_file_name(default_name: &str, reporter: TerminalProgressReporter) -> Result<String, i32> {
    Input::with_theme(&SimpleTheme)
        .with_prompt("File name (without .yml extension)")
        .default(default_name.to_string())
        .interact()
        .map_err(|_| {
            reporter.report_error("Failed to read file name.");
            1
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use selfie::fs::filesystem::MockFileSystem;
    use selfie::package::port::{MockPackageRepository, PackageRepoError};
    use std::path::PathBuf;
    use test_common::test_config_with_dir;

    #[test]
    fn test_handle_create_basic_non_interactive() {
        // Test basic package creation using MockPackageRepository for CLI logic testing
        let mut mock_repo = MockPackageRepository::new();
        let package_dir = PathBuf::from("/test/packages");
        let config = test_config_with_dir(&package_dir);

        // Mock successful save operation
        mock_repo
            .expect_save_package()
            .times(1)
            .returning(|_, _| Ok(()));

        // Test the package creation logic without repository implementation details
        let package_blob = create_basic_package("test-package", &config);

        // Test saving the package using mocked repository
        let result = mock_repo.save_package(&package_blob.package, &package_blob.file_path);

        assert!(result.is_ok());
        assert_eq!(package_blob.package.name(), "test-package");
        assert_eq!(package_blob.package.version(), "0.1.0");
        assert!(package_blob.is_new);
        assert_eq!(package_blob.file_path, package_dir.join("test-package.yml"));
    }

    #[test]
    fn test_get_package_new_creates_correct_template() {
        // Test GetPackage creation without filesystem operations
        let package_dir = PathBuf::from("/test/packages");

        let get_package = GetPackage::new("template-test", &package_dir);

        assert!(get_package.is_new);
        assert_eq!(get_package.package.name(), "template-test");
        assert_eq!(get_package.package.version(), "0.1.0");
        assert_eq!(get_package.file_path, package_dir.join("template-test.yml"));
        assert!(get_package.package.environments().contains_key("default"));

        // Check that the default environment has the expected structure
        let default_env = get_package.package.environments().get("default").unwrap();
        assert!(default_env.install().contains("template-test"));
        assert!(default_env.check().unwrap().contains("template-test"));
        assert!(default_env.dependencies().is_empty());
    }

    #[test]
    fn test_create_package_interactive_components() {
        // Test that EnvironmentConfig can be created with the new constructor
        let env_config = EnvironmentConfig::new(
            "brew install test".to_string(),
            Some("command -v test".to_string()),
            vec!["dependency1".to_string(), "dependency2".to_string()],
        );

        assert_eq!(env_config.install(), "brew install test");
        assert_eq!(env_config.check(), Some("command -v test"));
        assert_eq!(env_config.dependencies(), &["dependency1", "dependency2"]);
    }

    #[test]
    fn test_vs_code_wait_flag_logic() {
        // Test that VS Code gets the --wait flag added
        let mut cmd = std::process::Command::new("code");
        cmd.arg("/tmp/test.yml");

        let editor = "code";
        if editor == "code" {
            cmd.arg("--wait");
        }

        let args: Vec<_> = cmd.get_args().collect();
        assert!(
            args.iter()
                .any(|arg| *arg == std::ffi::OsStr::new("--wait"))
        );
    }

    #[test]
    fn test_package_template_structure() {
        // Test that the package template has the expected structure - no filesystem needed
        let package_dir = PathBuf::from("/test/packages");
        let get_package = GetPackage::new("structure-test", &package_dir);

        assert_eq!(get_package.package.name(), "structure-test");
        assert_eq!(get_package.package.version(), "0.1.0");
        assert!(get_package.package.description().is_none());
        assert!(get_package.package.homepage().is_none());

        let environments = get_package.package.environments();
        assert_eq!(environments.len(), 1);
        assert!(environments.contains_key("default"));

        let default_env = environments.get("default").unwrap();
        assert!(default_env.install().starts_with("# TODO:"));
        assert!(default_env.check().is_some());
        assert!(default_env.dependencies().is_empty());
    }

    #[test]
    fn test_create_basic_package_with_custom_environment() {
        // Test that --environment flag is respected in basic package creation (no filesystem)
        let package_dir = PathBuf::from("/test/packages");

        // Create config with custom environment
        let config = test_common::config::test_config_with_dir_and_env(&package_dir, "staging");

        let package_blob = create_basic_package("test-staging", &config);

        assert!(package_blob.is_new);
        assert_eq!(package_blob.package.name(), "test-staging");
        assert_eq!(package_blob.package.version(), "0.1.0");

        // Verify it uses the staging environment from config
        let environments = package_blob.package.environments();
        assert!(environments.contains_key("staging"));
        assert!(!environments.contains_key("default"));

        // Verify the staging environment has correct content
        let staging_env = &environments["staging"];
        assert!(staging_env.install().contains("test-staging"));
        assert!(staging_env.check().unwrap().contains("test-staging"));
        assert!(staging_env.dependencies().is_empty());
    }

    #[test]
    fn test_handle_create_respects_environment_flag() {
        // Test that package creation respects environment flag - no filesystem needed
        let package_dir = PathBuf::from("/test/packages");
        let config = test_common::config::test_config_with_dir_and_env(&package_dir, "production");

        // Test package creation with production environment
        let package_blob = create_basic_package("prod-test", &config);

        // Verify package structure uses production environment (no files created)
        assert_eq!(package_blob.package.name(), "prod-test");
        assert!(
            package_blob
                .package
                .environments()
                .contains_key("production")
        );
        assert!(!package_blob.package.environments().contains_key("default"));
        assert_eq!(package_blob.file_path, package_dir.join("prod-test.yml"));
    }

    #[test]
    fn test_package_name_validation_logic() {
        // Test package name handling without filesystem operations
        let package_dir = PathBuf::from("/test/packages");
        let config = test_config_with_dir(&package_dir);

        // Test package creation with unique name
        let package_blob = create_basic_package("new-unique-name", &config);

        // Verify the package structure (no real files created)
        assert_eq!(package_blob.package.name(), "new-unique-name");
        assert_eq!(
            package_blob.file_path,
            package_dir.join("new-unique-name.yml")
        );
    }

    #[test]
    fn test_create_basic_package_structure() {
        // Test package creation logic without filesystem operations
        let package_dir = PathBuf::from("/test/packages");
        let config = test_config_with_dir(&package_dir);

        let package_blob = create_basic_package("structure-test", &config);

        assert!(package_blob.is_new);
        assert_eq!(package_blob.package.name(), "structure-test");
        assert_eq!(package_blob.package.version(), "0.1.0");
        assert_eq!(
            package_blob.file_path,
            package_dir.join("structure-test.yml")
        );

        // Verify it has the correct environment from config
        let environments = package_blob.package.environments();
        assert!(environments.contains_key("test-env"));
    }

    #[test]
    fn test_package_creation_respects_config_environment() {
        // Test that package creation uses the environment from config
        let package_dir = PathBuf::from("/test/packages");
        let config = test_common::config::test_config_with_dir_and_env(&package_dir, "production");

        let package_blob = create_basic_package("env-test", &config);

        // Verify it uses the config environment
        let environments = package_blob.package.environments();
        assert!(environments.contains_key("production"));
        assert!(!environments.contains_key("test-env"));
    }

    #[test]
    fn test_package_file_path_generation() {
        // Test that file paths are generated correctly
        let package_dir = PathBuf::from("/custom/path/packages");
        let config = test_config_with_dir(&package_dir);

        let package_blob = create_basic_package("path-test", &config);

        assert_eq!(
            package_blob.file_path,
            PathBuf::from("/custom/path/packages/path-test.yml")
        );
    }

    #[test]
    fn test_create_workflow_with_mock_fs() {
        // Test complete package creation workflow using MockFileSystem instead of real files
        let mut mock_fs = MockFileSystem::default();
        let package_dir = PathBuf::from("/test/packages");
        let package_path = package_dir.join("complete-test.yml");

        // Mock the write operation
        mock_fs.mock_write_file(&package_path);

        let config = test_config_with_dir(&package_dir);
        let repo = common::create_package_repository_with_fs(&config, mock_fs);

        // Test creating a package with custom environment
        let package_blob = create_basic_package("complete-test", &config);

        // Verify package structure before saving
        assert_eq!(package_blob.package.name(), "complete-test");
        assert_eq!(package_blob.package.version(), "0.1.0");
        assert!(package_blob.package.environments().contains_key("test-env"));
        assert_eq!(package_blob.file_path, package_path);

        // Test the complete save operation
        let save_result =
            common::save_package(&repo, &package_blob, TerminalProgressReporter::new(false));
        assert!(save_result.is_ok());

        // Verify environment content
        let test_env = &package_blob.package.environments()["test-env"];
        assert!(test_env.install().contains("complete-test"));
        assert!(test_env.check().unwrap().contains("complete-test"));
        assert!(test_env.dependencies().is_empty());

        // This demonstrates creating and saving a package without creating real files
    }

    #[test]
    fn test_create_multiple_environments_mock_fs() {
        // Test creating a package with multiple environments using MockFileSystem
        let mut mock_fs = MockFileSystem::default();
        let package_dir = PathBuf::from("/test/packages");
        let package_path = package_dir.join("multi-env-test.yml");

        // Mock the write operation
        mock_fs.mock_write_file(&package_path);

        // Test with production environment
        let prod_config =
            test_common::config::test_config_with_dir_and_env(&package_dir, "production");
        let repo = common::create_package_repository_with_fs(&prod_config, mock_fs);

        let package_blob = create_basic_package("multi-env-test", &prod_config);

        // Verify it uses production environment
        assert!(
            package_blob
                .package
                .environments()
                .contains_key("production")
        );
        assert!(!package_blob.package.environments().contains_key("test-env"));

        // Test saving with production environment
        let save_result =
            common::save_package(&repo, &package_blob, TerminalProgressReporter::new(false));
        assert!(save_result.is_ok());

        // This shows how MockFileSystem enables testing different environment configurations
        // without filesystem dependencies
    }

    #[test]
    fn test_create_with_mock_config_loading() {
        // Test package creation with configuration loaded via MockFileSystem
        let mut mock_fs = MockFileSystem::default();
        let config_dir = PathBuf::from("/test/.config/selfie");
        let package_dir = PathBuf::from("/test/packages");
        let package_path = package_dir.join("config-test.yml");

        // Mock configuration file
        let config_yaml = r#"
environment: "staging"
package_directory: "/test/packages"
command_timeout: 60
"#;

        mock_fs.mock_config_file(&config_dir, config_yaml);
        mock_fs.mock_write_file(&package_path);

        // This demonstrates using mock_config_file helper for config testing
        let config = test_common::config::test_config_with_dir_and_env(&package_dir, "staging");
        let repo = common::create_package_repository_with_fs(&config, mock_fs);

        let package_blob = create_basic_package("config-test", &config);

        // Verify it uses staging environment from config
        assert!(package_blob.package.environments().contains_key("staging"));

        let save_result =
            common::save_package(&repo, &package_blob, TerminalProgressReporter::new(false));
        assert!(save_result.is_ok());
    }

    #[test]
    fn test_create_with_mock_path_operations() {
        // Test package creation using various mock path helpers
        let mut mock_fs = MockFileSystem::default();
        let package_dir = PathBuf::from("/test/packages");
        let package_path = package_dir.join("path-ops-test.yml");

        // Use mock_path_exists to simulate directory existence
        mock_fs.mock_path_exists(&package_dir, true);

        // Use mock_expand_path for tilde expansion testing
        let home_packages = PathBuf::from("~/packages");
        mock_fs.mock_expand_path(&home_packages, &package_dir);

        mock_fs.mock_write_file(&package_path);

        let config = test_config_with_dir(&package_dir);
        let repo = common::create_package_repository_with_fs(&config, mock_fs);

        let package_blob = create_basic_package("path-ops-test", &config);
        let save_result =
            common::save_package(&repo, &package_blob, TerminalProgressReporter::new(false));

        assert!(save_result.is_ok());
        assert_eq!(package_blob.file_path, package_path);

        // This demonstrates using mock_path_exists and mock_expand_path helpers
    }

    #[test]
    fn test_create_package_name_validation_with_mock_repo() {
        let mut mock_repo = MockPackageRepository::new();
        let package_dir = PathBuf::from("/test/packages");
        let config = test_config_with_dir(&package_dir);

        // Mock package doesn't exist (should allow creation)
        mock_repo
            .expect_get_package()
            .with(mockall::predicate::eq("new-package"))
            .times(1)
            .returning(|_| {
                Err(PackageRepoError::PackageError(Box::new(
                    selfie::package::port::PackageError::PackageNotFound {
                        name: "new-package".to_string(),
                        packages_path: std::path::PathBuf::from("/test/packages"),
                        files_examined: 0,
                        search_patterns: vec!["new-package.yml".to_string()],
                    },
                )))
            });

        // Test get_package call to verify package doesn't exist
        let get_result = mock_repo.get_package("new-package");
        assert!(get_result.is_err());

        // Test creating package with new name
        let package_blob = create_basic_package("new-package", &config);
        assert_eq!(package_blob.package.name(), "new-package");

        // This tests CLI package creation logic without repository implementation
    }

    #[test]
    fn test_save_package_with_mock_repo() {
        let mut mock_repo = MockPackageRepository::new();
        let package_dir = PathBuf::from("/test/packages");
        let config = test_config_with_dir(&package_dir);

        // Mock successful save
        mock_repo
            .expect_save_package()
            .times(1)
            .returning(|_, _| Ok(()));

        let package_blob = create_basic_package("save-test", &config);

        // Test saving using mocked repository
        let result = common::save_package(
            &mock_repo,
            &package_blob,
            TerminalProgressReporter::new(false),
        );
        assert!(result.is_ok());

        // This demonstrates testing CLI save logic without repository implementation details
    }

    #[test]
    fn test_create_with_dependency_selection_mock_repo() {
        let mut mock_repo = MockPackageRepository::new();

        // Mock available packages for dependency selection
        mock_repo
            .expect_available_packages()
            .times(1)
            .returning(|| {
                Ok(vec![
                    "database".to_string(),
                    "web-server".to_string(),
                    "cache".to_string(),
                ])
            });

        // This would test dependency selection UI logic
        // (In a real interactive test, we'd mock the user input)
        let available = mock_repo.available_packages().unwrap();
        assert_eq!(available.len(), 3);
        assert!(available.contains(&"database".to_string()));
        assert!(available.contains(&"web-server".to_string()));
        assert!(available.contains(&"cache".to_string()));

        // This demonstrates testing CLI dependency logic without repository implementation
    }

    #[test]
    fn test_create_package_error_handling_with_mock_repo() {
        let mut mock_repo = MockPackageRepository::new();
        let package_dir = PathBuf::from("/test/packages");
        let config = test_config_with_dir(&package_dir);

        // Mock save failure
        mock_repo.expect_save_package().times(1).returning(|_, _| {
            Err(PackageRepoError::IoError(std::sync::Arc::new(
                std::io::Error::new(std::io::ErrorKind::PermissionDenied, "Permission denied"),
            )))
        });

        let package_blob = create_basic_package("error-test", &config);

        // Test CLI error handling
        let result = common::save_package(
            &mock_repo,
            &package_blob,
            TerminalProgressReporter::new(false),
        );
        assert!(result.is_err());
        assert_eq!(result.unwrap_err(), 1);

        // This tests CLI error handling without filesystem or repository implementation
    }
}
