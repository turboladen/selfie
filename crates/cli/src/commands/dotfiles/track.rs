//! Track command handler for standalone dotfiles
//!
//! This module handles the `selfie dotfiles track <name> <file>` CLI command,
//! which starts tracking a file as a standalone dotfile by copying it into the
//! dotfiles directory and creating a YAML spec for it.

use selfie::namespace;
use tokio_util::sync::CancellationToken;
use tracing::info;

use crate::{
    commands::common::{self, create_package_repository, dotfiles_repository},
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

    // Ahead of the directory check, for the reason `handle_track_standalone`
    // states: under sudo, `dotfiles_directory` resolves under `/root`, and the
    // suggestion below would tell the user to create the root-owned directory
    // the refusal exists to prevent.
    if let Some(code) = common::refuse_under_sudo(config, display) {
        return code;
    }

    // Refuse here rather than letting `handle_track_standalone` do it further
    // down. This command copies the file *into* the dotfiles directory, so a
    // missing one stops the run either way — and checking first means the
    // helper below finds the directory present and stays quiet, instead of
    // warning about it a line before the refusal says the same thing.
    if let Err(code) = common::require_dotfiles_dir(config, display) {
        return code;
    }

    // Validate namespace before creating
    let repo = create_package_repository(config);
    let dotfiles_repo = dotfiles_repository(config, display);
    if let Err(e) = namespace::validate_unique_name(name, &repo, dotfiles_repo.as_ref()) {
        display.print_error(format!("Cannot use name '{name}': {e}"));
        return 1;
    }

    common::handle_track_standalone(name, file, config, display, cancellation_token).await
}
