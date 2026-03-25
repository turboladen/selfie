use selfie::package::{event::PackageEvent, service::PackageService};

use crate::{
    commands::spec::info::create_environment_table, config::CliConfig,
    display_manager::DisplayManager, event_processor::EventProcessor,
};

pub(crate) async fn handle_status(
    service: &impl PackageService,
    package_name: &str,
    config: &CliConfig,
    display: &DisplayManager,
) -> i32 {
    tracing::debug!("Getting runtime status for: {package_name}");

    display.print_progress(format!("Checking status of {package_name}..."));

    let event_stream = service.status(package_name).await;

    let processor = EventProcessor::new(display.clone());
    let result = processor
        .process_events(event_stream, |event| match event {
            PackageEvent::EnvironmentStatusChecked {
                environment_status, ..
            } => {
                let table = create_environment_table(environment_status, config);
                display.println(format!("\n{table}"));
                true
            }
            PackageEvent::Progress { .. } => true,
            PackageEvent::Completed {
                result: selfie::package::event::OperationResult::Success(_),
                ..
            } => true,
            PackageEvent::Completed { .. } => false,
            _ => false,
        })
        .await;
    result.exit_code
}
