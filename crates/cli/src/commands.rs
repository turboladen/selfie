//! Command dispatching and routing
//!
//! This module provides the central command dispatching system for the selfie CLI.
//! It routes parsed command-line arguments to the appropriate command handlers
//! and manages the execution flow for different types of operations.
//!
//! # Architecture
//!
//! The dispatcher follows a hierarchical routing pattern:
//! 1. Top-level command dispatch (package vs config)
//! 2. Subcommand dispatch within each category
//! 3. Individual command handler execution
//!
//! # Error Handling
//!
//! Commands return integer exit codes following Unix conventions:
//! - 0: Success
//! - 1: General error
//! - 2: Validation/usage error
//! - Other codes: Command-specific errors

pub(crate) mod completion;
pub(crate) mod config;
pub(crate) mod package;

use package::{common::create_package_service, list::ListCommand};
use tokio_util::sync::CancellationToken;
use tracing::debug;

use crate::config::CliConfig;

use crate::{
    cli::{ClapCommands, ConfigSubcommands, PackageSubcommands},
    display_manager::DisplayManager,
};

use completion::generate_completion;

/// Primary command dispatcher that routes to the appropriate command handler
///
/// This function serves as the main entry point for command execution after
/// CLI parsing is complete. It routes commands to specialized handlers based
/// on the command type and manages the overall execution flow.
///
/// # Arguments
///
/// * `command` - The parsed command to execute
/// * `config` - CLI configuration with overrides applied
/// * `display` - Display manager for user feedback
///
/// # Returns
///
/// Exit code indicating command success (0) or failure (non-zero)
///
/// # Command Categories
///
/// - **Package commands**: Install, check, list, info, create, validate packages
/// - **Config commands**: Validate configuration files and settings
pub(crate) async fn dispatch_command(
    command: &ClapCommands,
    config: &CliConfig,
    display: DisplayManager,
    cancellation_token: CancellationToken,
) -> i32 {
    debug!("Dispatching command: {:?}", command);

    match command {
        ClapCommands::Package(package_cmd) => {
            dispatch_package_command(&package_cmd.command, config, display, cancellation_token)
                .await
        }
        ClapCommands::Config(config_cmd) => {
            dispatch_config_command(&config_cmd.command, config, display)
        }
        ClapCommands::Completion { shell } => {
            generate_completion(*shell);
            0 // Success exit code
        }
    }
}

/// Handle package management commands
///
/// Routes package-related subcommands to their specific handlers. All package
/// operations use the modified configuration (with CLI overrides) and provide
/// progress feedback through the terminal reporter.
///
/// # Arguments
///
/// * `command` - The specific package subcommand to execute
/// * `config` - Application configuration with CLI overrides applied
/// * `display` - Display manager for user feedback
///
/// # Returns
///
/// Exit code indicating command success (0) or failure (non-zero)
///
/// # Supported Operations
///
/// - `install`: Install packages using configured installation methods
/// - `check`: Verify if packages are already installed
/// - `list`: Display all available packages
/// - `info`: Show detailed package information
/// - `create`: Create new package definition templates
/// - `validate`: Validate package definition files
async fn dispatch_package_command(
    command: &PackageSubcommands,
    config: &CliConfig,
    display: DisplayManager,
    cancellation_token: CancellationToken,
) -> i32 {
    debug!("Handling package command: {:?}", command);

    let service = create_package_service(config, cancellation_token);

    match command {
        PackageSubcommands::Install { package_name } => {
            package::install::handle_install(&service, package_name, config, &display).await
        }
        PackageSubcommands::Check { package_name } => {
            package::check::handle_check(&service, package_name, config, &display).await
        }
        PackageSubcommands::Audit { package_name, all } => {
            if *all {
                package::audit::handle_audit_all(&service, config, &display).await
            } else if let Some(name) = package_name {
                package::audit::handle_audit(&service, name, config, &display).await
            } else {
                display.print_error("Package name is required unless --all is used.");
                1
            }
        }
        PackageSubcommands::List { all } => {
            ListCommand::new(config, display, *all)
                .handle_command(&service)
                .await
        }
        PackageSubcommands::Info { package_name } => {
            package::info::handle_info(&service, package_name, config, &display).await
        }
        PackageSubcommands::Create {
            package_name,
            interactive,
        } => {
            package::create::handle_create(&service, package_name, config, &display, *interactive)
                .await
        }
        PackageSubcommands::Edit { package_name } => {
            package::edit::handle_edit(package_name, config, &display)
        }
        PackageSubcommands::Remove { package_name } => {
            package::remove::handle_remove(&service, package_name, config, &display).await
        }
        PackageSubcommands::Validate { package_name } => {
            package::validate::handle_validate(&service, package_name, config, &display).await
        }
    }
}

/// Handle configuration management commands
///
/// Routes configuration-related subcommands to their specific handlers.
///
/// # Arguments
///
/// * `command` - The specific config subcommand to execute
/// * `config` - CLI configuration with overrides applied
/// * `display` - Display manager for user feedback
///
/// # Returns
///
/// Exit code indicating command success (0) or failure (non-zero)
///
/// # Supported Operations
///
/// - `validate`: Validate the configuration file structure and values
fn dispatch_config_command(
    command: &ConfigSubcommands,
    config: &CliConfig,
    display: DisplayManager,
) -> i32 {
    debug!("Handling config command: {:?}", command);

    match command {
        ConfigSubcommands::Validate => config::handle_validate(config, &display),
    }
}

/// Report a styled key-value pair to the terminal
///
/// Provides a consistent way to display formatted messages with visual styling.
/// Delegates to `DisplayManager::print_field()` which displays the first parameter
/// in italic/dim style and the second parameter in bold style.
///
/// # Arguments
///
/// * `display` - Display manager for output
/// * `param1` - First part of the message (key, displayed italic/dim)
/// * `param2` - Second part of the message (value, displayed bold)
///
/// # Example
///
/// ```
/// report_with_style(&display, "Installing", "package-name");
/// // Displays:   Installing package-name (with appropriate styling)
/// ```
fn report_with_style(
    display: &DisplayManager,
    param1: impl std::fmt::Display,
    param2: impl std::fmt::Display,
) {
    display.print_field(param1, param2);
}
