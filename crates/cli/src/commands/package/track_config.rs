//! Track-config command handler for adding dotfiles to packages
//!
//! This module handles the `selfie package track-config <pkg> <file>` CLI
//! command, which adds a config file to an existing package's dotfiles list.

use tracing::info;

use crate::{commands::common, config::CliConfig, display_manager::DisplayManager};

/// Handle the `selfie package track-config` command
pub(crate) async fn handle_track_config(
    package_name: &str,
    file: &str,
    config: &CliConfig,
    display: &DisplayManager,
) -> i32 {
    info!("Adding '{}' to package '{}' dotfiles", file, package_name);
    common::handle_track_for_package(package_name, file, config, display).await
}
