use selfie::package::{
    event::{PackageEvent, ValidationResultData, ValidationStatus},
    service::SpecService,
};

use crate::{
    commands::validation_display::{ValidationGroup, ValidationRow, display_validation_groups},
    config::CliConfig,
    display_manager::DisplayManager,
    event_processor::EventProcessor,
    status_style,
};

pub(crate) async fn handle_validate(
    service: &impl SpecService,
    package_name: &str,
    config: &CliConfig,
    display: &DisplayManager,
) -> i32 {
    tracing::debug!("Running validate command for package: {}", package_name);

    display.print_progress(format!("Validating {package_name}..."));

    let event_stream = service.validate(package_name, None).await;

    let processor = EventProcessor::new(display.clone());
    let result = processor
        .process_events(event_stream, |event| {
            handle_validate_event(event, config, display)
        })
        .await;
    result.exit_code
}

pub(crate) async fn handle_validate_all(
    service: &impl SpecService,
    config: &CliConfig,
    display: &DisplayManager,
) -> i32 {
    tracing::debug!("Running validate --all command");

    display.print_progress("Validating all packages...");

    let event_stream = service.validate_all().await;

    let processor = EventProcessor::new(display.clone());
    let result = processor
        .process_events(event_stream, |event| {
            handle_validate_event(event, config, display)
        })
        .await;
    result.exit_code
}

fn handle_validate_event(
    event: &PackageEvent,
    config: &CliConfig,
    display: &DisplayManager,
) -> bool {
    match event {
        PackageEvent::ValidationResultCompleted {
            validation_result, ..
        } => {
            display_validation_result(validation_result, config, display);
            true // Handled
        }
        PackageEvent::Progress { .. } => {
            true // Handled - suppress progress for validate
        }
        _ => false, // Use default handling for other events
    }
}

fn display_validation_result(
    validation_result: &ValidationResultData,
    config: &CliConfig,
    display: &DisplayManager,
) {
    match validation_result.status {
        ValidationStatus::Valid => {
            // Show success card for valid packages
            display_validation_success_card(validation_result, config, display);

            // A valid package can still carry informational notices — the
            // apply-time command count is one, and it exists precisely to be
            // seen. Gating the table on the status would hide it from every
            // package that has nothing else wrong, which is most of them.
            display_validation_issues_table(validation_result, config, display);
        }
        ValidationStatus::HasWarnings | ValidationStatus::HasErrors => {
            // Show table for packages with issues
            display_validation_issues_table(validation_result, config, display);
        }
    }
}

fn display_validation_success_card(
    validation_result: &ValidationResultData,
    config: &CliConfig,
    display: &DisplayManager,
) {
    display
        .result_card("Validation Results")
        .field("Package", &validation_result.package_name)
        .field("Environment", &validation_result.environment)
        .field("Status", status_style::format_valid(config.use_colors()))
        .print();
}

fn display_validation_issues_table(
    validation_result: &ValidationResultData,
    config: &CliConfig,
    display: &DisplayManager,
) {
    if validation_result.issues.is_empty() {
        return;
    }

    let rows: Vec<ValidationRow<'_>> = validation_result
        .issues
        .iter()
        .map(|issue| {
            let level = match issue.level {
                selfie::package::event::ValidationLevel::Error => "ERROR",
                selfie::package::event::ValidationLevel::Warning => "WARN",
                selfie::package::event::ValidationLevel::Info => "INFO",
            };
            ValidationRow {
                level,
                category: &issue.category,
                field: &issue.field,
                message: &issue.message,
                location: issue.location.as_deref(),
            }
        })
        .collect();

    let groups = vec![ValidationGroup {
        label: &validation_result.package_name,
        rows,
    }];

    display_validation_groups(&groups, config.use_colors(), display);
}
