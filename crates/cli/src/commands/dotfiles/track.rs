//! Track command handler for standalone dotfiles
//!
//! This module handles the `selfie dotfiles track <name> <file>` CLI command,
//! which starts tracking a file as a standalone dotfile by copying it into the
//! dotfiles directory and creating a YAML spec for it.

use selfie::{
    dotfile_service::{port::DotfileService, service::DotfileServiceImpl},
    fs::real::RealFileSystem,
    package::repository::yaml::YamlPackageRepository,
};
use tracing::info;

use crate::{
    commands::common::create_package_repository, config::CliConfig,
    display_manager::DisplayManager, event_processor::EventProcessor,
};

/// Handle the `selfie dotfiles track` command
pub(crate) async fn handle_track(
    name: &str,
    file: &str,
    config: &CliConfig,
    display: &DisplayManager,
) -> i32 {
    info!("Tracking dotfile '{}' as '{}'", file, name);

    let repo = create_package_repository(config);
    let fs = RealFileSystem;
    let mut service = DotfileServiceImpl::new(repo, fs, config.selfie_config().clone());

    // The dotfiles directory must exist for standalone tracking
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

    let dotfiles_repo = YamlPackageRepository::new(RealFileSystem, dotfiles_dir);
    service = service.with_dotfiles_repository(dotfiles_repo);

    let event_stream = service.track_standalone(name, file).await;

    let processor = EventProcessor::new(display.clone());
    let result = processor.process_events(event_stream, |_| false).await;

    result.exit_code
}
