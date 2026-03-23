//! Command dispatching and routing
//!
//! This module provides the central command dispatching system for the selfie CLI.
//! It routes parsed command-line arguments to the appropriate command handlers
//! and manages the execution flow for different types of operations.
//!
//! # Architecture
//!
//! The dispatcher follows a hierarchical routing pattern:
//! 1. Top-level command dispatch (spec, package, config)
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

pub(crate) mod apply;
pub(crate) mod common;
pub(crate) mod completion;
pub(crate) mod config;
pub(crate) mod dotfiles;
pub(crate) mod package;
pub(crate) mod spec;
pub(crate) mod sync;
pub(crate) mod track;

use common::create_package_service;
use package::list::ListCommand;
use tokio_util::sync::CancellationToken;
use tracing::debug;

use crate::config::CliConfig;

use crate::{
    cli::{
        ClapCommands, ConfigSubcommands, DotfilesSubcommands, PackageSubcommands, SpecSubcommands,
        SyncSubcommands,
    },
    display_manager::DisplayManager,
};
use apply::handle_apply;

use completion::generate_completion;

/// Primary command dispatcher that routes to the appropriate command handler
pub(crate) async fn dispatch_command(
    command: &ClapCommands,
    config: &CliConfig,
    display: DisplayManager,
    cancellation_token: CancellationToken,
    fs: &impl selfie::fs::FileSystem,
) -> i32 {
    debug!("Dispatching command: {:?}", command);

    match command {
        ClapCommands::Apply(args) => handle_apply(args, config, &display).await,
        ClapCommands::Dotfiles(dotfiles_cmd) => {
            dispatch_dotfiles_command(&dotfiles_cmd.command, config, &display).await
        }
        ClapCommands::Spec(spec_cmd) => {
            dispatch_spec_command(&spec_cmd.command, config, display, cancellation_token).await
        }
        ClapCommands::Package(package_cmd) => {
            dispatch_package_command(&package_cmd.command, config, display, cancellation_token)
                .await
        }
        ClapCommands::Sync(sync_cmd) => {
            dispatch_sync_command(&sync_cmd.command, config, &display).await
        }
        ClapCommands::Track { file } => track::handle_track(file, config, &display).await,
        ClapCommands::Config(config_cmd) => {
            dispatch_config_command(&config_cmd.command, config, display, fs)
        }
        ClapCommands::Completion { shell } => {
            generate_completion(*shell);
            0
        }
    }
}

/// Handle spec (definition) commands
///
/// Routes spec-related subcommands to their handlers. Spec operations work
/// with package definition files without executing system commands.
async fn dispatch_spec_command(
    command: &SpecSubcommands,
    config: &CliConfig,
    display: DisplayManager,
    cancellation_token: CancellationToken,
) -> i32 {
    debug!("Handling spec command: {:?}", command);

    match command {
        SpecSubcommands::Edit { package_name } => {
            // Edit doesn't need a service — it works directly with files
            spec::edit::handle_edit(package_name, config, &display)
        }
        _ => {
            let service = create_package_service(config, cancellation_token);
            match command {
                SpecSubcommands::Create {
                    package_name,
                    interactive,
                } => {
                    spec::create::handle_create(
                        &service,
                        package_name,
                        config,
                        &display,
                        *interactive,
                    )
                    .await
                }
                SpecSubcommands::Remove { package_name, yes } => {
                    spec::remove::handle_remove(&service, package_name, config, &display, *yes)
                        .await
                }
                SpecSubcommands::Validate { package_name, all } => {
                    if *all {
                        spec::validate::handle_validate_all(&service, config, &display).await
                    } else {
                        // clap enforces: package_name is required unless --all
                        let name = package_name.as_ref().unwrap();
                        spec::validate::handle_validate(&service, name, config, &display).await
                    }
                }
                SpecSubcommands::List { all } => {
                    spec::list::handle_list(&service, config, &display, *all).await
                }
                SpecSubcommands::Info { package_name } => {
                    spec::info::handle_info(&service, package_name, config, &display).await
                }
                SpecSubcommands::Edit { .. } => unreachable!(),
            }
        }
    }
}

/// Handle package (runtime) commands
///
/// Routes package-related subcommands to their handlers. Package operations
/// execute configured commands on the system.
async fn dispatch_package_command(
    command: &PackageSubcommands,
    config: &CliConfig,
    display: DisplayManager,
    cancellation_token: CancellationToken,
) -> i32 {
    debug!("Handling package command: {:?}", command);

    let service = create_package_service(config, cancellation_token);

    match command {
        PackageSubcommands::Install {
            package_name,
            no_recommends,
        } => {
            let options = selfie::package::service::InstallOptions {
                skip_recommends: *no_recommends,
            };
            package::install::handle_install(&service, package_name, options, config, &display)
                .await
        }
        PackageSubcommands::Check { package_name } => {
            package::check::handle_check(&service, package_name, config, &display).await
        }
        PackageSubcommands::Audit { package_name, all } => {
            if *all {
                package::audit::handle_audit_all(&service, config, &display).await
            } else {
                // clap enforces: package_name is required unless --all
                let name = package_name.as_ref().unwrap();
                package::audit::handle_audit(&service, name, config, &display).await
            }
        }
        PackageSubcommands::List { all } => {
            ListCommand::new(config, display, *all)
                .handle_command(&service)
                .await
        }
        PackageSubcommands::Status { package_name } => {
            package::status::handle_status(&service, package_name, config, &display).await
        }
        PackageSubcommands::TrackDotfile { package_name, file } => {
            package::track_dotfile::handle_track_dotfile(package_name, file, config, &display).await
        }
    }
}

/// Handle dotfile management commands
async fn dispatch_dotfiles_command(
    command: &DotfilesSubcommands,
    config: &CliConfig,
    display: &DisplayManager,
) -> i32 {
    debug!("Handling dotfiles command: {:?}", command);

    match command {
        DotfilesSubcommands::Drift => dotfiles::drift::handle_drift(config, display).await,
        DotfilesSubcommands::List => dotfiles::list::handle_list(config, display),
        DotfilesSubcommands::Track { name, file } => {
            dotfiles::track::handle_track(name, file, config, display).await
        }
    }
}

/// Handle sync commands
async fn dispatch_sync_command(
    command: &SyncSubcommands,
    config: &CliConfig,
    display: &DisplayManager,
) -> i32 {
    debug!("Handling sync command: {:?}", command);

    match command {
        SyncSubcommands::Status => sync::status::handle_status(config, display).await,
        SyncSubcommands::Push {
            batch,
            message,
            yes,
            include_untracked,
        } => {
            let args = sync::push::PushArgs {
                batch: *batch,
                message: message.clone(),
                yes: *yes,
                include_untracked: *include_untracked,
            };
            sync::push::handle_push(&args, config, display).await
        }
        SyncSubcommands::Pull => sync::pull::handle_pull(config, display).await,
    }
}

/// Handle configuration management commands
fn dispatch_config_command(
    command: &ConfigSubcommands,
    config: &CliConfig,
    display: DisplayManager,
    fs: &impl selfie::fs::FileSystem,
) -> i32 {
    debug!("Handling config command: {:?}", command);

    match command {
        ConfigSubcommands::Validate => config::handle_validate(config, &display, fs),
    }
}
