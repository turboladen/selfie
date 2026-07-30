//! Track-dotfile command handler for adding dotfiles to packages
//!
//! This module handles the `selfie package track-dotfile <pkg> <file>` CLI
//! command, which adds a config file to an existing package's dotfiles list.

use tokio_util::sync::CancellationToken;
use tracing::info;

use crate::{commands::common, config::CliConfig, display_manager::DisplayManager};

/// Handle the `selfie package track-dotfile` command
pub(crate) async fn handle_track_dotfile(
    package_name: &str,
    file: &str,
    config: &CliConfig,
    display: &DisplayManager,
    cancellation_token: CancellationToken,
) -> i32 {
    info!("Adding '{}' to package '{}' dotfiles", file, package_name);
    common::handle_track_for_package(package_name, file, config, display, cancellation_token).await
}
