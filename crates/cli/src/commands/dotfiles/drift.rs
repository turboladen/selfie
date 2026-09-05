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

    let service = create_dotfile_service(config, display, cancellation_token);
    let event_stream = service.check_drift().await;

    let display_for_handler = display.clone();
    let processor = EventProcessor::new(display.clone());
    let result = processor
        .process_events(event_stream, |event| match event {
            // Render the completion summary with color that matches the outcome:
            //   0 drifted → green ✔ (all clean)
            //   N drifted → yellow ⚠ (attention needed)
            // A success carrying a refusal is deliberately not claimed here.
            // `process_events` skips its default handler for any event a custom
            // handler returns `true` for, and that default handler is the only
            // thing that writes the exit code. Rendering a refusal prettily here
            // would print the warning and exit 0, while the MCP server -- which
            // reads `had_refusals` directly -- would report the same run as
            // refused. Apply learned this at `commands/apply.rs`; this is the
            // same half of the same problem.
            PackageEvent::Completed {
                result: OperationResult::Success(success),
                ..
            } if !success.had_refusals() => {
                let has_drift = matches!(
                    success,
                    OperationSuccess::DotfileDriftChecked {
                        drift_count,
                        ..
                    } if *drift_count > 0
                );

                if has_drift {
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
