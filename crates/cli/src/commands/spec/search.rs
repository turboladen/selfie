use selfie::package::service::SpecService;

use crate::{config::CliConfig, display_manager::DisplayManager, event_processor::EventProcessor};

use super::list::handle_spec_list_event;

pub(crate) async fn handle_search(
    service: &impl SpecService,
    pattern: &str,
    config: &CliConfig,
    display: &DisplayManager,
) -> i32 {
    tracing::debug!("Running spec search command (pattern={pattern:?})");

    display.print_progress(format!("Searching specs for \"{pattern}\"..."));

    let event_stream = service.search(pattern).await;

    let processor = EventProcessor::new(display.clone());
    let result = processor
        .process_events(event_stream, |event| {
            handle_spec_list_event(event, config, display)
        })
        .await;
    result.exit_code
}
