//! Drift check command handler for dotfiles
//!
//! This module handles the `selfie dotfiles drift` CLI command, which checks
//! all deployed dotfiles for drift between repo sources, deployed targets,
//! and the last-known deploy state checksums.

use selfie::{
    dotfile_service::port::DotfileService,
    package::event::{OperationResult, OperationSuccess, PackageEvent},
};
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

    let display_for_handler = display.clone();
    let processor = EventProcessor::new(display.clone());
    let result = processor
        .process_events(event_stream, |event| match event {
            // The summary is green when nothing drifted or was orphaned, and yellow
            // otherwise. A success carrying a refusal is not claimed here:
            // `process_events` skips its default handler for any event a custom
            // handler returns `true` for, and only that handler writes the exit
            // code. Claiming a refusal would print the warning and exit 0, while
            // the MCP server, which reads `had_refusals`, reports the run as
            // refused. `commands/apply.rs` leaves refusals to the default handler
            // for the same reason.
            PackageEvent::Completed {
                result: OperationResult::Success(success),
                ..
            } if !success.had_refusals() => {
                // Drift or an orphan found is worth the reader's attention. An unverified
                // entry is not: it is unverifiable by design, and the summary
                // names the count. Anything refused, including a spec that could
                // not be loaded, never reaches here: it fails the check above.
                let unclean = matches!(
                    success,
                    OperationSuccess::DotfileDriftChecked { drift_count, orphan_count, .. }
                        if *drift_count > 0 || *orphan_count > 0
                );

                if unclean {
                    display_for_handler.print_warning(success.to_string());
                } else {
                    display_for_handler.print_success(success.to_string());
                }
                true
            }
            _ => false,
        })
        .await;

    result.exit_code
}
