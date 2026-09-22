//! Track command handler for standalone dotfiles
//!
//! This module handles the `selfie dotfiles track <name> <file>` CLI command,
//! which starts tracking a file as a standalone dotfile by copying it into the
//! dotfiles directory and creating a YAML spec for it.

use selfie::namespace;
use tokio_util::sync::CancellationToken;
use tracing::info;

use crate::{
    commands::common::{self, create_dotfiles_repository, create_package_repository},
    config::CliConfig,
    display_manager::DisplayManager,
};

/// Handle the `selfie dotfiles track` command
pub(crate) async fn handle_track(
    name: &str,
    file: &str,
    config: &CliConfig,
    display: &DisplayManager,
    cancellation_token: CancellationToken,
) -> i32 {
    info!("Tracking dotfile '{}' as '{}'", file, name);

    // The sudo refusal runs ahead of the name check, which would otherwise read
    // the package and dotfiles repositories only for the service to refuse the
    // track under sudo.
    if let Some(code) = common::refuse_under_sudo(config, display) {
        return code;
    }

    // A dotfiles directory that is genuinely not there holds no names, and the
    // service refuses the track itself. One that will not read cannot say whether
    // the name is free, so the check refuses rather than answering.
    let repo = create_package_repository(config);
    let dotfiles_repo = create_dotfiles_repository(config);
    if let Err(e) = namespace::validate_unique_name(
        name,
        &repo,
        Some(&dotfiles_repo),
        &selfie::fs::RealFileSystem,
        &config.selfie_config().dotfiles_directory(),
    ) {
        display.print_error(format!("Cannot use name '{name}': {e}"));
        return 1;
    }

    common::handle_track_standalone(name, file, config, display, cancellation_token).await
}
