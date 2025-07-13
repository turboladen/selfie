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

pub(crate) mod config;
pub(crate) mod package;

use package::list::ListCommand;
use selfie::config::AppConfig;
use tracing::debug;

use crate::{
    cli::{ClapCommands, ConfigSubcommands, PackageSubcommands},
    terminal_progress_reporter::TerminalProgressReporter,
};

/// Primary command dispatcher that routes to the appropriate command handler
///
/// This function serves as the main entry point for command execution after
/// CLI parsing is complete. It routes commands to specialized handlers based
/// on the command type and manages the overall execution flow.
///
/// # Arguments
///
/// * `command` - The parsed command to execute
/// * `config` - Application configuration with CLI overrides applied
/// * `original_config` - Original configuration from file (for config commands)
/// * `reporter` - Terminal progress reporter for user feedback
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
    config: &AppConfig,
    original_config: AppConfig,
    reporter: TerminalProgressReporter,
) -> i32 {
    debug!("Dispatching command: {:?}", command);

    match command {
        ClapCommands::Package(package_cmd) => {
            dispatch_package_command(&package_cmd.command, config, reporter).await
        }
        ClapCommands::Config(config_cmd) => {
            dispatch_config_command(&config_cmd.command, &original_config, reporter)
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
/// * `reporter` - Terminal progress reporter for user feedback
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
    config: &AppConfig,
    reporter: TerminalProgressReporter,
) -> i32 {
    debug!("Handling package command: {:?}", command);

    match command {
        PackageSubcommands::Install { package_name } => {
            package::install::handle_install(package_name, config, reporter).await
        }
        PackageSubcommands::Check { package_name } => {
            package::check::handle_check(package_name, config, reporter).await
        }
        PackageSubcommands::List => ListCommand::new(config, reporter).handle_command().await,
        PackageSubcommands::Info { package_name } => {
            package::info::handle_info(package_name, config, reporter).await
        }
        PackageSubcommands::Create {
            package_name,
            interactive,
        } => package::create::handle_create(package_name, config, reporter, *interactive),
        PackageSubcommands::Edit { package_name } => {
            package::edit::handle_edit(package_name, config, reporter)
        }
        PackageSubcommands::Remove { package_name } => {
            package::remove::handle_remove(package_name, config, reporter)
        }
        PackageSubcommands::Validate { package_name } => {
            package::validate::handle_validate(package_name, config, reporter).await
        }
    }
}

/// Handle configuration management commands
///
/// Routes configuration-related subcommands to their specific handlers.
/// Config operations use the original configuration (without CLI overrides)
/// to validate the actual configuration file contents.
///
/// # Arguments
///
/// * `command` - The specific config subcommand to execute
/// * `original_config` - Original configuration from file (no CLI overrides)
/// * `reporter` - Terminal progress reporter for user feedback
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
    original_config: &AppConfig,
    reporter: TerminalProgressReporter,
) -> i32 {
    debug!("Handling config command: {:?}", command);

    match command {
        ConfigSubcommands::Validate => config::handle_validate(original_config, reporter),
    }
}

/// Report a styled message to the terminal
///
/// Provides a consistent way to display formatted messages with visual styling.
/// The first parameter is displayed in italic/dim style, and the second parameter
/// is displayed in bold style.
///
/// # Arguments
///
/// * `param1` - First part of the message (displayed italic/dim)
/// * `param2` - Second part of the message (displayed bold)
///
/// # Example
///
/// ```
/// report_with_style("Installing", "package-name");
/// // Displays: Installing package-name (with appropriate styling)
/// ```
fn report_with_style(param1: impl std::fmt::Display, param2: impl std::fmt::Display) {
    TerminalProgressReporter::report(
        2,
        format!(
            "{} {}",
            console::style(param1).italic().dim(),
            console::style(param2).bold()
        ),
    );
}
