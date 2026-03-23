//! Track-config command handler for adding dotfiles to packages
//!
//! This module handles the `selfie package track-config <pkg> <file>` CLI
//! command, which adds a config file to an existing package's dotfiles list.

use selfie::{
    dotfile_service::{port::DotfileService, service::DotfileServiceImpl},
    fs::real::RealFileSystem,
};
use tracing::info;

use crate::{
    commands::common::create_package_repository, config::CliConfig,
    display_manager::DisplayManager, event_processor::EventProcessor,
};

/// Handle the `selfie package track-config` command
pub(crate) async fn handle_track_config(
    package_name: &str,
    file: &str,
    config: &CliConfig,
    display: &DisplayManager,
) -> i32 {
    info!("Adding '{}' to package '{}' dotfiles", file, package_name);

    let repo = create_package_repository(config);
    let fs = RealFileSystem;
    let service = DotfileServiceImpl::new(repo, fs, config.selfie_config().clone());

    let event_stream = service.track_for_package(package_name, file).await;

    let processor = EventProcessor::new(display.clone());
    let result = processor.process_events(event_stream, |_| false).await;

    result.exit_code
}
