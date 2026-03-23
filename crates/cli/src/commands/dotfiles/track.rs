//! Track command handler for standalone dotfiles
//!
//! This module handles the `selfie dotfiles track <name> <file>` CLI command,
//! which starts tracking a file as a standalone dotfile by copying it into the
//! dotfiles directory and creating a YAML spec for it.

use selfie::{
    fs::real::RealFileSystem, namespace, package::repository::yaml::YamlPackageRepository,
};
use tracing::info;

use crate::{
    commands::common::{self, create_package_repository},
    config::CliConfig,
    display_manager::DisplayManager,
};

/// Handle the `selfie dotfiles track` command
pub(crate) async fn handle_track(
    name: &str,
    file: &str,
    config: &CliConfig,
    display: &DisplayManager,
) -> i32 {
    info!("Tracking dotfile '{}' as '{}'", file, name);

    // Validate namespace before creating
    let repo = create_package_repository(config);
    let dotfiles_dir = config.selfie_config().dotfiles_directory();
    let dotfiles_repo = if dotfiles_dir.is_dir() {
        Some(YamlPackageRepository::new(RealFileSystem, dotfiles_dir))
    } else {
        None
    };
    if let Err(e) = namespace::validate_unique_name(name, &repo, dotfiles_repo.as_ref()) {
        display.print_error(format!("Cannot use name '{name}': {e}"));
        return 1;
    }

    common::handle_track_standalone(name, file, config, display).await
}
