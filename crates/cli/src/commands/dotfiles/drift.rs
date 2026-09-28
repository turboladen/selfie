//! Drift check command handler for dotfiles
//!
//! This module handles the `selfie dotfiles drift` CLI command, which checks
//! all deployed dotfiles for drift between repo sources, deployed targets,
//! and the last-known deploy state checksums.

use selfie::dotfile_service::port::DotfileService;
use tokio_util::sync::CancellationToken;
use tracing::info;

use crate::{
    commands::common::create_dotfile_service, config::CliConfig, display_manager::DisplayManager,
    event_processor::EventProcessor,
};

/// Handle the `selfie dotfiles drift` command
///
/// Creates a `DotfileServiceImpl` and calls `check_drift()`, which walks all
/// dotfile entries across packages and the standalone dotfiles directory,
/// comparing current file contents against stored deploy-state checksums.
pub(crate) async fn handle_drift(
    config: &CliConfig,
    display: &DisplayManager,
    cancellation_token: CancellationToken,
) -> i32 {
    info!("Checking dotfile drift");

    let service = create_dotfile_service(config, cancellation_token);
    let event_stream = service.check_drift().await;

    // The default rendering prints the summary at the level of the library's
    // verdict: drift or an orphan found is a warning, a refusal an error.
    let processor = EventProcessor::new(display.clone());
    let result = processor.process_events(event_stream, |_event| false).await;

    result.exit_code
}
