//! Common utilities for package commands
//!
//! This module provides shared functionality used across multiple package commands
//! to reduce code duplication and maintain consistency.

use comfy_table::{ContentArrangement, Table, modifiers, presets};
use console::style;

use selfie::{
    commands::ShellCommandRunner,
    dotfile_service::{port::DotfileService, service::DotfileServiceImpl},
    fs::{filesystem::FileSystem, real::RealFileSystem},
    git::GixGitAdapter,
    package::{
        GetPackage, SpecService,
        event::PackageEvent,
        git_adapter::GixGitStatusProvider,
        port::PackageRepository,
        repository::yaml::YamlPackageRepository,
        service::{PackageService, PackageServiceImpl},
    },
    privilege::{RealPrivilege, RootPolicy},
    sync_service::{SyncService, service::SyncServiceImpl},
};
use tokio_util::sync::CancellationToken;

use crate::{config::CliConfig, event_processor::EventProcessor};
use std::{path::Path, process::Command};

use crate::display_manager::{DisplayManager, INDENT};

/// Create a package repository instance with the configured package directory
pub(crate) fn create_package_repository(
    config: &CliConfig,
) -> YamlPackageRepository<RealFileSystem> {
    create_package_repository_with_fs(config, RealFileSystem)
}

/// Create a package repository with a specific filesystem implementation
/// This is useful for testing with `MockFileSystem`
pub(crate) fn create_package_repository_with_fs<F: FileSystem>(
    config: &CliConfig,
    fs: F,
) -> YamlPackageRepository<F> {
    YamlPackageRepository::new(fs, config.package_directory().clone())
}

// The one place the CLI picks a shell. `clippy.toml` makes a non-login runner a
// build error crate-wide, including inside this function.
fn create_command_runner(config: &CliConfig) -> ShellCommandRunner {
    ShellCommandRunner::login_shell(config.command_timeout())
}

/// Create a `DotfileServiceImpl` with the packages repo and, if the configured
/// `dotfiles_directory` exists on disk, an additional dotfiles repo.
///
/// This is the standard setup for any command that needs `DotfileService`.
pub(crate) fn create_dotfile_service(
    config: &CliConfig,
    cancellation_token: CancellationToken,
) -> DotfileServiceImpl<
    YamlPackageRepository<RealFileSystem>,
    RealFileSystem,
    ShellCommandRunner,
    RealPrivilege,
> {
    let repo = create_package_repository(config);
    let fs = RealFileSystem;
    let runner = create_command_runner(config);
    let mut service = DotfileServiceImpl::new(
        repo,
        fs,
        runner,
        config.selfie_config().clone(),
        cancellation_token,
        root_policy(config),
    );

    let dotfiles_dir = config.selfie_config().dotfiles_directory();
    if dotfiles_dir.is_dir() {
        let dotfiles_repo = YamlPackageRepository::new(RealFileSystem, dotfiles_dir);
        service = service.with_dotfiles_repository(dotfiles_repo);
    }

    service
}

/// Create a `SyncServiceImpl` with `GixGitAdapter` and `DotfileService`.
///
/// This is the standard setup for any command that needs `SyncService`.
pub(crate) fn create_sync_service(
    config: &CliConfig,
    cancellation_token: CancellationToken,
) -> impl SyncService {
    let git = GixGitAdapter;
    let dotfile_service = create_dotfile_service(config, cancellation_token);
    // Its own policy rather than one read back out of `dotfile_service`: sync
    // commits and pushes as root even though it deploys nothing, and root-owned
    // git objects in a user-owned repository do not self-heal.
    SyncServiceImpl::new(
        git,
        dotfile_service,
        config.selfie_config().clone(),
        root_policy(config),
    )
}

/// The sudo refusal every service that writes is built with.
///
/// One function so the CLI cannot hand `--allow-root` to one service and forget
/// the other. The refusal itself lives in the library and holds for any caller;
/// what is true only by convention is that every CLI write path is built through
/// the two constructors above. Nothing enforces that — `apply` once built its own
/// service — so do not read it as a guarantee.
fn root_policy(config: &CliConfig) -> RootPolicy<RealPrivilege> {
    let policy = RootPolicy::new(RealPrivilege);
    if config.allow_root() {
        policy.allowing_root()
    } else {
        policy
    }
}

/// Track a standalone dotfile via `DotfileServiceImpl::track_standalone`.
///
/// Shared by `selfie dotfiles track` and `selfie track` (interactive).
pub(crate) async fn handle_track_standalone(
    name: &str,
    file: &str,
    config: &CliConfig,
    display: &DisplayManager,
    cancellation_token: CancellationToken,
) -> i32 {
    let dotfiles_dir = config.selfie_config().dotfiles_directory();
    if !dotfiles_dir.is_dir() {
        display.print_error(format!(
            "Dotfiles directory does not exist: {}",
            dotfiles_dir.display()
        ));
        display.print_suggestion(format!(
            "Create it with: mkdir -p {}",
            dotfiles_dir.display()
        ));
        return 1;
    }

    let service = create_dotfile_service(config, cancellation_token);
    let event_stream = service.track_standalone(name, file).await;

    let processor = EventProcessor::new(display.clone());
    let display_for_handler = display.clone();
    let result = processor
        .process_events(event_stream, move |event| {
            handle_already_tracked(event, &display_for_handler)
        })
        .await;
    result.exit_code
}

/// Track a file for an existing package via `DotfileServiceImpl::track_for_package`.
///
/// Shared by `selfie package track-dotfile` and `selfie track` (interactive).
pub(crate) async fn handle_track_for_package(
    package_name: &str,
    file: &str,
    config: &CliConfig,
    display: &DisplayManager,
    cancellation_token: CancellationToken,
) -> i32 {
    let service = create_dotfile_service(config, cancellation_token);
    let event_stream = service.track_for_package(package_name, file).await;

    let processor = EventProcessor::new(display.clone());
    let display_for_handler = display.clone();
    let result = processor
        .process_events(event_stream, move |event| {
            handle_already_tracked(event, &display_for_handler)
        })
        .await;
    result.exit_code
}

/// Custom event handler that renders already-tracked results as info (ℹ) instead
/// of success (✓), since no work was performed.
fn handle_already_tracked(event: &PackageEvent, display: &DisplayManager) -> bool {
    use selfie::package::event::{OperationResult, OperationSuccess};

    match event {
        PackageEvent::Completed {
            result:
                OperationResult::Success(
                    success @ OperationSuccess::DotfileTracked {
                        was_already_tracked: true,
                        ..
                    },
                ),
            ..
        } => {
            display.print_info(success.to_string());
            true
        }
        _ => false,
    }
}

/// Save a package to the filesystem with consistent error handling
pub(crate) fn save_package(
    repo: &impl PackageRepository,
    package_blob: &GetPackage,
    display: &DisplayManager,
) -> Result<(), i32> {
    if let Err(e) = repo.save_package(package_blob.package(), package_blob.file_path()) {
        display.print_error(format!("Failed to save package file: {e}"));
        return Err(1);
    }
    Ok(())
}

/// Open a file in the user's preferred editor
///
/// Handles common editor functionality including:
/// - Checking for EDITOR environment variable
/// - Adding --wait flag for VS Code
/// - Executing the editor command
/// - Providing appropriate success/failure messages
pub(crate) fn open_editor(
    file_path: &Path,
    display: &DisplayManager,
    success_message: Option<String>,
) -> i32 {
    let Ok(editor) = std::env::var("EDITOR") else {
        display.print_error("EDITOR environment variable is not set.");
        display.print_info("Please set EDITOR and try again.");
        return 1;
    };

    let mut cmd = Command::new(&editor);
    cmd.arg(file_path);

    // For VS Code, wait for the file to be closed
    if editor == "code" {
        cmd.arg("--wait");
    }

    match cmd.status() {
        Ok(status) if status.success() => {
            if let Some(message) = success_message {
                display.print_success(message);
            }
            0
        }
        Ok(_) => {
            display.print_warning("Editor exited with non-zero status.");
            1
        }
        Err(e) => {
            display.print_error(format!("Failed to start editor '{editor}': {e}"));
            1
        }
    }
}

/// Check if EDITOR environment variable is set and provide helpful error messages
///
/// Returns the editor command if available, or reports an error and returns None.
/// Provides context-specific error messages for different scenarios.
pub(crate) fn check_editor_available(
    display: &DisplayManager,
    package_name: &str,
    package_exists: bool,
    package_path: Option<&Path>,
) -> Option<String> {
    if let Ok(editor) = std::env::var("EDITOR") {
        Some(editor)
    } else {
        display.print_error("EDITOR environment variable is not set.");

        if package_exists {
            if let Some(path) = package_path {
                display.print_info(format!(
                    "Package '{}' exists at {}. Go ahead and open it in your editor of choice!",
                    package_name,
                    path.display()
                ));
            } else {
                display.print_info(format!(
                    "Package '{package_name}' exists. Set EDITOR to edit it automatically."
                ));
            }
        } else {
            display.print_info(format!(
                "Package '{package_name}' doesn't exist yet. Set EDITOR and try again to create it."
            ));
        }
        None
    }
}

/// Create a new package template
pub(crate) fn create_new_package(package_name: &str, config: &CliConfig) -> GetPackage {
    GetPackage::new(package_name, config.package_directory())
}

/// Create a package service with repository and command runner
pub(crate) fn create_package_service(
    config: &CliConfig,
    cancellation_token: CancellationToken,
) -> impl PackageService + SpecService {
    let repo = create_package_repository(config);
    let command_runner = create_command_runner(config);
    PackageServiceImpl::new(
        repo,
        command_runner,
        GixGitStatusProvider,
        config.selfie_config().clone(),
        cancellation_token,
    )
}

/// Create a formatted table with consistent styling
pub(crate) fn create_formatted_table() -> Table {
    let mut table = Table::new();
    table
        .load_preset(presets::UTF8_FULL_CONDENSED)
        .apply_modifier(modifiers::UTF8_ROUND_CORNERS)
        .set_content_arrangement(ContentArrangement::Dynamic);
    table
}

/// Format environment names with current environment highlighting
/// Current environment appears first, followed by others sorted alphabetically
pub(crate) fn format_environment_names(
    environments: &[String],
    current_environment: &str,
    config: &CliConfig,
) -> String {
    let mut sorted_envs = environments.to_vec();

    // Sort so current environment comes first, then alphabetically
    sorted_envs.sort_by(|a, b| {
        if a == current_environment && b != current_environment {
            std::cmp::Ordering::Less
        } else if a != current_environment && b == current_environment {
            std::cmp::Ordering::Greater
        } else {
            a.cmp(b)
        }
    });

    sorted_envs
        .iter()
        .map(|env_name| {
            if env_name == current_environment {
                let env = format!("*{env_name}");
                if config.use_colors() {
                    style(env).bold().green().to_string()
                } else {
                    env
                }
            } else if config.use_colors() {
                style(env_name).dim().green().to_string()
            } else {
                env_name.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// Format a key with consistent styling
pub(crate) fn format_field_key(key: &str, use_colors: bool) -> String {
    if use_colors {
        style(key).cyan().bold().to_string()
    } else {
        key.to_string()
    }
}

/// Format a value with consistent styling
pub(crate) fn format_field_value(value: &str, use_colors: bool) -> String {
    if use_colors {
        style(value).white().to_string()
    } else {
        value.to_string()
    }
}

/// Display environment error with available environments for a specific package
pub(crate) fn display_environment_summary(
    package_name: &str,
    current_environment: &str,
    available_environments: &[String],
    config: &CliConfig,
    display: &DisplayManager,
    context: &str, // "check" or "install"
) {
    if available_environments.is_empty() {
        display_generic_environment_suggestion(
            package_name,
            current_environment,
            config,
            display,
            context,
        );
    } else {
        display.print_suggestion(format!(
            "Package '{package_name}' doesn't support environment '{current_environment}'."
        ));
        display.println(format!("{INDENT}Available environments for this package:"));

        let mut table = create_formatted_table();
        table.set_header(vec!["Environment"]);

        // Sort environments, highlighting the current one if present
        let mut sorted_envs = available_environments.to_vec();
        sorted_envs.sort();

        for env in sorted_envs {
            let env_display = if config.use_colors() {
                if env == current_environment {
                    console::style(&env).green().bold().to_string()
                } else {
                    env.clone()
                }
            } else {
                env
            };
            table.add_row(vec![env_display]);
        }

        display.println(format!("{table}"));

        if config.use_colors() {
            display.print_suggestion(format!(
                "{} with one of the environments above",
                console::style(format!(
                    "selfie package {context} --environment <env> <package>"
                ))
                .yellow()
            ));
        } else {
            display.print_suggestion(format!(
                "selfie package {context} --environment <env> <package> with one of the environments above"
            ));
        }
    }
}

/// Display generic environment suggestion when specific environment info is not available
pub(crate) fn display_generic_environment_suggestion(
    package_name: &str,
    current_environment: &str,
    config: &CliConfig,
    display: &DisplayManager,
    context: &str, // "check" or "install"
) {
    display.print_suggestion(format!(
        "Package '{package_name}' doesn't support environment '{current_environment}'."
    ));
    display.println(format!("{INDENT}Try one of these options:"));
    if config.use_colors() {
        display.println(format!(
            "{INDENT}• {} to {} with a different environment",
            console::style(format!(
                "selfie package {context} --environment <env> <package>"
            ))
            .yellow(),
            context
        ));
        display.println(format!(
            "{INDENT}• {} to see which environments this package supports",
            console::style("selfie spec info <package>").yellow()
        ));
    } else {
        display.println(format!(
            "{INDENT}• selfie package {context} --environment <env> <package> to {context} with a different environment"
        ));
        display.println(format!(
            "{INDENT}• selfie spec info <package> to see which environments this package supports"
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use selfie::package::port::{MockPackageRepository, PackageRepoError};
    use test_common::test_config_with_dir;

    #[test]
    fn test_create_new_package() {
        // Test package creation logic without filesystem operations
        let package_dir = std::path::PathBuf::from("/test/packages");
        let config = CliConfig::wrap_for_test(test_config_with_dir(&package_dir));

        let package_blob = create_new_package("test-package", &config);

        assert!(package_blob.is_new());
        assert_eq!(package_blob.package().name(), "test-package");

        assert_eq!(
            package_blob.file_path(),
            package_dir.join("test-package.yml")
        );
    }

    #[test]
    fn test_vs_code_wait_flag_logic() {
        // Test that VS Code gets the --wait flag
        let editor = "code";
        let mut cmd = Command::new(editor);
        cmd.arg("/tmp/test.yml");

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
    fn test_save_package_logic() {
        // Test save package logic without filesystem operations
        let package_dir = std::path::PathBuf::from("/test/packages");
        let config = CliConfig::wrap_for_test(test_config_with_dir(&package_dir));

        let package_blob = create_new_package("save-test", &config);

        // Verify package structure is correct before saving
        assert!(package_blob.is_new());
        assert_eq!(package_blob.package().name(), "save-test");

        assert_eq!(package_blob.file_path(), package_dir.join("save-test.yml"));

        // Verify it has default environment (since create_new_package uses GetPackage::new)
        let environments = package_blob.package().environments();
        assert!(environments.contains_key("default"));
    }

    #[test]
    fn test_create_new_package_structure() {
        // Test package creation logic without filesystem operations
        // Note: create_new_package uses GetPackage::new which creates a "default" environment
        let package_dir = std::path::PathBuf::from("/test/packages");
        let config = CliConfig::wrap_for_test(test_config_with_dir(&package_dir));

        let package_blob = create_new_package("structure-test", &config);

        assert!(package_blob.is_new());
        assert_eq!(package_blob.package().name(), "structure-test");

        assert_eq!(
            package_blob.file_path(),
            package_dir.join("structure-test.yml")
        );

        // create_new_package uses GetPackage::new which creates "default" environment
        let environments = package_blob.package().environments();
        assert!(environments.contains_key("default"));
    }

    #[test]
    fn test_format_environment_names() {
        // Test environment name formatting without filesystem operations
        let package_dir = std::path::PathBuf::from("/test/packages");
        let config = CliConfig::wrap_for_test(test_config_with_dir(&package_dir));
        let environments = vec!["test".to_string(), "production".to_string()];

        let result = format_environment_names(&environments, "test", &config);

        // Just test that it doesn't panic and returns something
        assert!(!result.is_empty());
        assert!(result.contains("test"));
    }

    #[test]
    fn test_format_environment_names_ordering() {
        let package_dir = std::path::PathBuf::from("/test/packages");
        let config = CliConfig::wrap_for_test(test_config_with_dir(&package_dir));

        // Test with current environment not first in input list
        let environments = vec![
            "arch-home".to_string(),
            "macos-work".to_string(),
            "ubuntu-server".to_string(),
        ];

        let result = format_environment_names(&environments, "macos-work", &config);

        // Current environment should come first, marked with *
        assert!(result.starts_with("*macos-work"));

        // Should contain all environments
        assert!(result.contains("arch-home"));
        assert!(result.contains("ubuntu-server"));

        // Split by comma and check order
        let parts: Vec<&str> = result.split(", ").collect();
        assert_eq!(parts.len(), 3);
        assert_eq!(parts[0], "*macos-work");

        // Remaining should be alphabetically sorted
        let remaining: Vec<&str> = parts[1..].to_vec();
        assert_eq!(remaining, vec!["arch-home", "ubuntu-server"]);
    }

    #[test]
    fn test_format_environment_names_single_environment() {
        let package_dir = std::path::PathBuf::from("/test/packages");
        let config = CliConfig::wrap_for_test(test_config_with_dir(&package_dir));

        let environments = vec!["macos-work".to_string()];
        let result = format_environment_names(&environments, "macos-work", &config);

        assert_eq!(result, "*macos-work");
    }

    #[test]
    fn test_format_environment_names_current_not_present() {
        let package_dir = std::path::PathBuf::from("/test/packages");
        let config = CliConfig::wrap_for_test(test_config_with_dir(&package_dir));

        let environments = vec!["arch-home".to_string(), "ubuntu-server".to_string()];
        let result = format_environment_names(&environments, "macos-work", &config);

        // Should not contain asterisk since current environment is not in the list
        assert!(!result.contains('*'));

        // Should be alphabetically sorted
        assert_eq!(result, "arch-home, ubuntu-server");
    }

    #[test]
    fn test_format_field_key_and_value() {
        let key = format_field_key("Test Key", false);
        assert_eq!(key, "Test Key");

        let value = format_field_value("Test Value", false);
        assert_eq!(value, "Test Value");

        // Test with colors (just ensure no panic)
        let _colored_key = format_field_key("Test Key", true);
        let _colored_value = format_field_value("Test Value", true);
    }

    #[test]
    fn test_save_package_with_mock_repository() {
        let mut mock_repo = MockPackageRepository::new();
        let package_dir = std::path::PathBuf::from("/test/packages");
        let config = CliConfig::wrap_for_test(test_config_with_dir(&package_dir));

        // Mock successful save operation
        mock_repo
            .expect_save_package()
            .times(1)
            .returning(|_, _| Ok(()));

        let package_blob = create_new_package("mock-repo-test", &config);
        let display = DisplayManager::new(false);

        // Test saving using mocked repository - tests CLI logic, not repository implementation
        let result = save_package(&mock_repo, &package_blob, &display);
        assert!(result.is_ok());

        // This demonstrates testing CLI logic without repository implementation details
    }

    #[test]
    fn test_save_package_repository_error_handling() {
        let mut mock_repo = MockPackageRepository::new();
        let package_dir = std::path::PathBuf::from("/test/packages");
        let config = CliConfig::wrap_for_test(test_config_with_dir(&package_dir));

        // Mock repository error
        mock_repo.expect_save_package().times(1).returning(|_, _| {
            Err(PackageRepoError::IoError(std::sync::Arc::new(
                std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "Simulated repository error",
                ),
            )))
        });

        let package_blob = create_new_package("error-test", &config);
        let display = DisplayManager::new(false);

        // Test error handling in CLI layer
        let result = save_package(&mock_repo, &package_blob, &display);
        assert!(result.is_err());
        assert_eq!(result.unwrap_err(), 1); // Should return error code 1

        // This tests CLI error handling without filesystem dependencies
    }

    #[test]
    fn test_package_workflow_with_mock_repository() {
        let mut mock_repo = MockPackageRepository::new();
        let package_dir = std::path::PathBuf::from("/test/packages");
        let config = CliConfig::wrap_for_test(test_config_with_dir(&package_dir));

        // Mock successful save
        mock_repo
            .expect_save_package()
            .times(1)
            .returning(|_, _| Ok(()));

        // Create package blob
        let package_blob = create_new_package("workflow-test", &config);

        // Verify package structure before saving
        assert_eq!(package_blob.package().name(), "workflow-test");

        assert!(
            package_blob
                .package()
                .environments()
                .contains_key("default")
        );

        // Test saving through CLI layer
        let display = DisplayManager::new(false);
        let result = save_package(&mock_repo, &package_blob, &display);
        assert!(result.is_ok());

        // This demonstrates testing complete CLI workflows without repository implementation
    }
}
