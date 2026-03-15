use dialoguer::{Confirm, Input, MultiSelect, Select, theme::SimpleTheme};
use selfie::package::{
    EnvironmentConfig, PackageService,
    event::{OperationResult, OperationSuccess, PackageEvent},
    port::PackageRepository,
};
use std::{collections::HashMap, path::PathBuf};
use tracing::info;

use crate::{config::CliConfig, display_manager::DisplayManager, event_processor::EventProcessor};

use super::common;

const MAX_NAME_RETRIES: usize = 3;

enum PackageNameResult {
    CreateNew(String),     // Use this name to create a new package
    EditExisting(PathBuf), // User wants to edit the existing package at this path
    Cancelled,             // User cancelled the operation
}

pub(crate) async fn handle_create(
    service: &impl PackageService,
    package_name: &str,
    config: &CliConfig,
    display: &DisplayManager,
    interactive: bool,
) -> i32 {
    info!("Creating package: {}", package_name);

    // Create repository for name validation (UI flow decisions)
    let repo = common::create_package_repository(config);

    // Get a valid package name or handle existing package scenarios
    let package_name = match get_valid_package_name(package_name, &repo, display) {
        Ok(PackageNameResult::CreateNew(name)) => name,
        Ok(PackageNameResult::EditExisting(path)) => {
            display.print_info(format!(
                "Opening existing package for editing at {}",
                path.display()
            ));
            let success_message = format!("Package editing completed at {}", path.display());
            return common::open_editor(&path, display, Some(success_message));
        }
        Ok(PackageNameResult::Cancelled) => {
            display.print_info("Package creation cancelled.");
            return 0;
        }
        Err(exit_code) => return exit_code,
    };

    // Build the Package (CLI handles interactive prompting)
    let package = if interactive {
        match create_package_interactive(&package_name, config, display) {
            Ok(pkg) => pkg,
            Err(exit_code) => return exit_code,
        }
    } else {
        create_basic_package(&package_name, config)
    };

    // Use PackageService::create to persist (hexagonal pattern)
    let event_stream = service.create(package).await;

    // Process the event stream with custom handling for create-specific events
    let mut created_file_path: Option<PathBuf> = None;
    let processor = EventProcessor::new(display.clone());
    let result = processor
        .process_events(event_stream, |event| match event {
            PackageEvent::Completed {
                result: OperationResult::Success(OperationSuccess::PackageCreated {
                    file_path, ..
                }),
                ..
            } => {
                created_file_path = Some(file_path.clone());
                false // Let default handler print success message
            }
            PackageEvent::Progress { .. } if !config.verbose() => true, // Suppress in non-verbose
            _ => false, // Default handling for everything else
        })
        .await;

    if result.exit_code != 0 {
        return result.exit_code;
    }

    // Ask if user wants to edit the file (only in interactive mode)
    if interactive {
        if let Some(ref file_path) = created_file_path {
            let edit_now = Confirm::with_theme(&SimpleTheme)
                .with_prompt("Would you like to open the package file for editing now?")
                .default(true)
                .interact();

            match edit_now {
                Ok(true) => {
                    let success_message = format!(
                        "Package '{}' created and saved at {}",
                        package_name,
                        file_path.display()
                    );
                    common::open_editor(file_path, display, Some(success_message))
                }
                Ok(false) => {
                    display.print_info(
                        "Package created. You can edit it later with 'selfie package edit'.",
                    );
                    0
                }
                Err(_) => {
                    display.print_error("Failed to read user input.");
                    1
                }
            }
        } else {
            0
        }
    } else {
        display.print_info("Package created. Use 'selfie package edit' to customize it.");
        0
    }
}

fn get_valid_package_name(
    initial_name: &str,
    repo: &impl PackageRepository,
    display: &DisplayManager,
) -> Result<PackageNameResult, i32> {
    let mut current_name = initial_name.to_string();
    let mut retry_count = 0;

    loop {
        // Check if package already exists
        if let Ok(existing_package) = repo.get_package(&current_name) {
            display.print_info(format!("Package '{current_name}' already exists."));

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
                    return Ok(PackageNameResult::EditExisting(
                        existing_package.file_path().to_path_buf(),
                    ));
                }
                Ok(1) => {
                    // Create with different name
                    retry_count += 1;
                    if retry_count > MAX_NAME_RETRIES {
                        display.print_error(format!(
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
                        display.print_error("Failed to read package name.");
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

fn create_basic_package(package_name: &str, config: &CliConfig) -> selfie::package::Package {
    let mut environments = HashMap::new();

    // Use the environment from config (which may be overridden by --environment)
    let env_name = config.environment();
    let env_config = EnvironmentConfig::new(
        format!("# TODO: Add install command for {package_name}"),
        Some(format!("# TODO: Add check command for {package_name}")),
        Vec::new(),
    );

    environments.insert(env_name.to_string(), env_config);

    selfie::package::Package::new(
        package_name.to_string(),
        "0.1.0".to_string(),
        None,
        None,
        environments,
        config
            .package_directory()
            .join(format!("{package_name}.yml")),
    )
}

fn create_package_interactive(
    package_name: &str,
    config: &CliConfig,
    display: &DisplayManager,
) -> Result<selfie::package::Package, i32> {
    display.print_info("Creating package interactively...");

    let name = prompt_package_name(package_name, display)?;
    let version = prompt_package_version(display)?;
    let homepage = prompt_package_homepage(display)?;
    let description = prompt_package_description(display)?;
    let environments = prompt_environments(&name, config, display)?;
    let file_name = prompt_file_name(&name, display)?;

    Ok(selfie::package::Package::new(
        name,
        version,
        homepage,
        description,
        environments,
        config.package_directory().join(format!("{file_name}.yml")),
    ))
}

fn prompt_package_name(default_name: &str, display: &DisplayManager) -> Result<String, i32> {
    Input::with_theme(&SimpleTheme)
        .with_prompt("Package name")
        .default(default_name.to_string())
        .interact()
        .map_err(|_| {
            display.print_error("Failed to read package name.");
            1
        })
}

fn prompt_package_version(display: &DisplayManager) -> Result<String, i32> {
    Input::with_theme(&SimpleTheme)
        .with_prompt("Version")
        .default("0.1.0".to_string())
        .interact()
        .map_err(|_| {
            display.print_error("Failed to read version.");
            1
        })
}

fn prompt_package_homepage(display: &DisplayManager) -> Result<Option<String>, i32> {
    let homepage: String = Input::with_theme(&SimpleTheme)
        .with_prompt("Homepage URL (optional)")
        .allow_empty(true)
        .interact()
        .map_err(|_| {
            display.print_error("Failed to read homepage.");
            1
        })?;

    Ok(if homepage.trim().is_empty() {
        None
    } else {
        Some(homepage)
    })
}

fn prompt_package_description(display: &DisplayManager) -> Result<Option<String>, i32> {
    let description: String = Input::with_theme(&SimpleTheme)
        .with_prompt("Description (optional)")
        .allow_empty(true)
        .interact()
        .map_err(|_| {
            display.print_error("Failed to read description.");
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
    config: &CliConfig,
    display: &DisplayManager,
) -> Result<HashMap<String, EnvironmentConfig>, i32> {
    let mut environments = HashMap::new();

    loop {
        display.print_info("Adding environment configuration...");

        let env_name = prompt_environment_name(&environments, config, display)?;
        let install_cmd = prompt_install_command(display)?;
        let check_cmd = prompt_check_command(package_name, display)?;
        let dependencies = prompt_dependencies(config, display)?;

        let env_config = EnvironmentConfig::new(install_cmd, check_cmd, dependencies);
        environments.insert(env_name, env_config);

        if !prompt_add_another_environment(display)? {
            break;
        }
    }

    Ok(environments)
}

fn prompt_environment_name(
    existing_environments: &HashMap<String, EnvironmentConfig>,
    config: &CliConfig,
    display: &DisplayManager,
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
            display.print_error("Failed to read environment name.");
            1
        })
}

fn prompt_install_command(display: &DisplayManager) -> Result<String, i32> {
    loop {
        let cmd: String = Input::with_theme(&SimpleTheme)
            .with_prompt("Install command (required)")
            .interact()
            .map_err(|_| {
                display.print_error("Failed to read install command.");
                1
            })?;

        if !cmd.trim().is_empty() {
            break Ok(cmd);
        }

        display.print_error("Install command cannot be empty.");
    }
}

fn prompt_check_command(
    package_name: &str,
    display: &DisplayManager,
) -> Result<Option<String>, i32> {
    let default_check = format!("command -v {package_name}");
    let check_cmd: String = Input::with_theme(&SimpleTheme)
        .with_prompt("Check command (optional)")
        .default(default_check)
        .allow_empty(true)
        .interact()
        .map_err(|_| {
            display.print_error("Failed to read check command.");
            1
        })?;

    Ok(if check_cmd.trim().is_empty() {
        None
    } else {
        Some(check_cmd)
    })
}

fn prompt_dependencies(config: &CliConfig, display: &DisplayManager) -> Result<Vec<String>, i32> {
    let repo = common::create_package_repository(config);
    let mut available_packages = repo.available_packages().unwrap_or_default();

    if available_packages.is_empty() {
        return Ok(Vec::new());
    }

    // Sort packages alphabetically for consistent presentation
    available_packages.sort();

    let selected = MultiSelect::with_theme(&SimpleTheme)
        .with_prompt("Dependencies (select with space, confirm with enter)")
        .items(&available_packages)
        .interact()
        .map_err(|_| {
            display.print_error("Failed to read dependencies.");
            1
        })?;

    Ok(selected
        .into_iter()
        .map(|i| available_packages[i].clone())
        .collect())
}

fn prompt_add_another_environment(display: &DisplayManager) -> Result<bool, i32> {
    Confirm::with_theme(&SimpleTheme)
        .with_prompt("Add another environment?")
        .default(false)
        .interact()
        .map_err(|_| {
            display.print_error("Failed to read user input.");
            1
        })
}

fn prompt_file_name(default_name: &str, display: &DisplayManager) -> Result<String, i32> {
    Input::with_theme(&SimpleTheme)
        .with_prompt("File name (without .yml extension)")
        .default(default_name.to_string())
        .interact()
        .map_err(|_| {
            display.print_error("Failed to read file name.");
            1
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;
    use selfie::package::PackageService;
    use selfie::package::event::{OperationResult, OperationSuccess, PackageEvent};
    use selfie::package::port::MockPackageRepository;
    use std::path::PathBuf;
    use test_common::{test_config_with_dir, test_config_with_dir_and_env};

    /// Helper: collect the final `OperationResult` from an event stream.
    async fn collect_result(
        mut stream: selfie::package::event::EventStream,
    ) -> Option<OperationResult> {
        let mut result = None;
        while let Some(event) = stream.next().await {
            if let PackageEvent::Completed {
                result: op_result, ..
            } = event
            {
                result = Some(op_result);
            }
        }
        result
    }

    // ── Package template / structure tests (pure functions, no service needed) ──

    #[test]
    fn test_basic_package_template() {
        let package_dir = PathBuf::from("/test/packages");
        let config = CliConfig::wrap_for_test(test_config_with_dir(&package_dir));

        let package = create_basic_package("template-test", &config);

        assert_eq!(package.name(), "template-test");
        assert_eq!(package.version(), "0.1.0");
        assert!(package.environments().contains_key("test-env"));

        let env = package.environments().get("test-env").unwrap();
        assert!(env.install().contains("template-test"));
        assert!(env.check().unwrap().contains("template-test"));
        assert!(env.dependencies().is_empty());
    }

    #[test]
    fn test_create_package_interactive_components() {
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
        let package_dir = PathBuf::from("/test/packages");
        let config = CliConfig::wrap_for_test(test_config_with_dir(&package_dir));

        let package = create_basic_package("structure-test", &config);

        assert_eq!(package.name(), "structure-test");
        assert_eq!(package.version(), "0.1.0");
        assert!(package.description().is_none());
        assert!(package.homepage().is_none());

        let environments = package.environments();
        assert_eq!(environments.len(), 1);
        assert!(environments.contains_key("test-env"));

        let default_env = environments.get("test-env").unwrap();
        assert!(default_env.install().starts_with("# TODO:"));
        assert!(default_env.check().is_some());
        assert!(default_env.dependencies().is_empty());
    }

    #[test]
    fn test_create_basic_package_with_custom_environment() {
        let package_dir = PathBuf::from("/test/packages");
        let config =
            CliConfig::wrap_for_test(test_config_with_dir_and_env(&package_dir, "staging"));

        let package = create_basic_package("test-staging", &config);

        assert_eq!(package.name(), "test-staging");
        assert_eq!(package.version(), "0.1.0");

        let environments = package.environments();
        assert!(environments.contains_key("staging"));
        assert!(!environments.contains_key("default"));

        let staging_env = &environments["staging"];
        assert!(staging_env.install().contains("test-staging"));
        assert!(staging_env.check().unwrap().contains("test-staging"));
        assert!(staging_env.dependencies().is_empty());
    }

    #[test]
    fn test_handle_create_respects_environment_flag() {
        let package_dir = PathBuf::from("/test/packages");
        let config =
            CliConfig::wrap_for_test(test_config_with_dir_and_env(&package_dir, "production"));

        let package = create_basic_package("prod-test", &config);

        assert_eq!(package.name(), "prod-test");
        assert!(package.environments().contains_key("production"));
        assert!(!package.environments().contains_key("default"));
    }

    #[test]
    fn test_package_name_validation_logic() {
        let package_dir = PathBuf::from("/test/packages");
        let config = CliConfig::wrap_for_test(test_config_with_dir(&package_dir));

        let package = create_basic_package("new-unique-name", &config);

        assert_eq!(package.name(), "new-unique-name");
    }

    #[test]
    fn test_create_basic_package_structure() {
        let package_dir = PathBuf::from("/test/packages");
        let config = CliConfig::wrap_for_test(test_config_with_dir(&package_dir));

        let package = create_basic_package("structure-test", &config);

        assert_eq!(package.name(), "structure-test");
        assert_eq!(package.version(), "0.1.0");

        let environments = package.environments();
        assert!(environments.contains_key("test-env"));
    }

    #[test]
    fn test_package_creation_respects_config_environment() {
        let package_dir = PathBuf::from("/test/packages");
        let config =
            CliConfig::wrap_for_test(test_config_with_dir_and_env(&package_dir, "production"));

        let package = create_basic_package("env-test", &config);

        let environments = package.environments();
        assert!(environments.contains_key("production"));
        assert!(!environments.contains_key("test-env"));
    }

    // ── Service-layer tests (persistence goes through PackageService::create) ──

    #[tokio::test]
    async fn test_create_via_service_success() {
        let temp_dir = tempfile::tempdir().unwrap();
        let service = test_common::create_test_service(&temp_dir);
        let config = CliConfig::wrap_for_test(test_config_with_dir(temp_dir.path()));

        let package = create_basic_package("test-package", &config);

        let stream = service.create(package).await;
        let result = collect_result(stream).await.unwrap();

        match result {
            OperationResult::Success(OperationSuccess::PackageCreated {
                package_name,
                file_path,
                ..
            }) => {
                assert_eq!(package_name, "test-package");
                assert_eq!(file_path, temp_dir.path().join("test-package.yml"));
                assert!(file_path.exists(), "Package file should be created on disk");
            }
            other => panic!("Expected PackageCreated, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_create_via_service_already_exists() {
        let temp_dir = tempfile::tempdir().unwrap();
        let config = CliConfig::wrap_for_test(test_config_with_dir(temp_dir.path()));

        // Pre-create a package file so the service finds it
        let _ = test_common::create_service_test_package_file(&temp_dir, "existing-pkg", true);

        let service = test_common::create_test_service(&temp_dir);
        let package = create_basic_package("existing-pkg", &config);

        let stream = service.create(package).await;
        let result = collect_result(stream).await.unwrap();

        assert!(
            matches!(result, OperationResult::Failure(_)),
            "Expected failure for existing package"
        );
    }

    #[tokio::test]
    async fn test_create_via_service_with_custom_environment() {
        let temp_dir = tempfile::tempdir().unwrap();
        let config =
            CliConfig::wrap_for_test(test_config_with_dir_and_env(temp_dir.path(), "production"));
        let service = test_common::create_test_service_for_env(&temp_dir, "production");

        let package = create_basic_package("prod-pkg", &config);

        assert!(package.environments().contains_key("production"));
        assert!(!package.environments().contains_key("default"));

        let stream = service.create(package).await;
        let result = collect_result(stream).await.unwrap();

        match result {
            OperationResult::Success(OperationSuccess::PackageCreated { environment, .. }) => {
                assert_eq!(environment, "production");
            }
            other => panic!("Expected PackageCreated, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_create_via_service_file_written_correctly() {
        let temp_dir = tempfile::tempdir().unwrap();
        let service = test_common::create_test_service(&temp_dir);
        let config = CliConfig::wrap_for_test(test_config_with_dir(temp_dir.path()));

        let package = create_basic_package("file-check", &config);

        let stream = service.create(package).await;
        let result = collect_result(stream).await.unwrap();

        assert!(matches!(result, OperationResult::Success(_)));

        // Verify the file was actually written with correct content
        let file_path = temp_dir.path().join("file-check.yml");
        let contents = std::fs::read_to_string(&file_path).unwrap();
        assert!(contents.contains("file-check"));
        assert!(contents.contains("0.1.0"));
    }

    // ── UI-concern tests (name validation uses repo directly for interactive flow) ──

    #[test]
    fn test_create_package_name_validation_with_mock_repo() {
        let mut mock_repo = MockPackageRepository::new();
        let package_dir = PathBuf::from("/test/packages");
        let config = CliConfig::wrap_for_test(test_config_with_dir(&package_dir));

        mock_repo
            .expect_get_package()
            .with(mockall::predicate::eq("new-package"))
            .times(1)
            .returning(|_| {
                Err(selfie::package::port::PackageError::PackageNotFound {
                    name: "new-package".to_string(),
                    packages_path: std::path::PathBuf::from("/test/packages"),
                    files_examined: 0,
                    search_patterns: vec!["new-package.yml".to_string()],
                }
                .into())
            });

        let get_result = mock_repo.get_package("new-package");
        assert!(get_result.is_err());

        let package = create_basic_package("new-package", &config);
        assert_eq!(package.name(), "new-package");
    }

    #[test]
    fn test_create_with_dependency_selection_mock_repo() {
        let mut mock_repo = MockPackageRepository::new();

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

        let available = mock_repo.available_packages().unwrap();
        assert_eq!(available.len(), 3);
        assert!(available.contains(&"database".to_string()));
        assert!(available.contains(&"web-server".to_string()));
        assert!(available.contains(&"cache".to_string()));
    }
}
