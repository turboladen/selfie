use selfie::package::{
    event::{OperationResult, Outcome, PackageEvent},
    service::PackageService,
};

use crate::{
    commands::spec::info::create_environment_table,
    config::CliConfig,
    display_manager::DisplayManager,
    event_processor::{EventProcessor, Exit},
};

pub(crate) async fn handle_status(
    service: &impl PackageService,
    package_name: &str,
    config: &CliConfig,
    display: &DisplayManager,
) -> Exit {
    tracing::debug!("Getting runtime status for: {package_name}");

    let event_stream = service.status(package_name).await;

    let processor = EventProcessor::new(display.clone());
    let result = processor
        .process_events(event_stream, |event| match event {
            PackageEvent::EnvironmentStatusChecked {
                environment_status, ..
            } => {
                let table = create_environment_table(environment_status, config);
                display.println(crate::terminal_text::shown!("\n{}", table));
                true
            }
            // The table is the answer for a clean status. A status with gaps also
            // needs its summary, which names each package it could not answer for.
            PackageEvent::Completed {
                result: OperationResult::Success(success),
                ..
            } => success.outcome() == Outcome::Clean,
            PackageEvent::Completed { .. } => false,
            _ => false,
        })
        .await;
    result.exit
}
