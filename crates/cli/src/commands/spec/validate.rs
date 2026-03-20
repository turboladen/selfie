use comfy_table::{ContentArrangement, Table, modifiers, presets};
use console::style;
use selfie::package::{
    event::{PackageEvent, ValidationLevel, ValidationResultData, ValidationStatus},
    service::SpecService,
};

use crate::{
    config::CliConfig, display_manager::DisplayManager, event_processor::EventProcessor,
    formatters::format_key, status_style,
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
    display.println("");
    display.print_section_header("Validation Results");

    let format_key_fn =
        |field: &str| -> String { format!("   {}: ", format_key(field, config.use_colors())) };

    display.println(format!(
        "{}{}",
        format_key_fn("Package"),
        validation_result.package_name
    ));
    display.println(format!(
        "{}{}",
        format_key_fn("Environment"),
        validation_result.environment
    ));

    display.println(format!(
        "{}{}",
        format_key_fn("Status"),
        status_style::format_valid(config.use_colors())
    ));
}

fn display_validation_issues_table(
    validation_result: &ValidationResultData,
    config: &CliConfig,
    display: &DisplayManager,
) {
    if validation_result.issues.is_empty() {
        return;
    }

    display.println("");

    // Show summary
    let error_count = validation_result
        .issues
        .iter()
        .filter(|i| matches!(i.level, ValidationLevel::Error))
        .count();
    let warning_count = validation_result
        .issues
        .iter()
        .filter(|i| matches!(i.level, ValidationLevel::Warning))
        .count();

    let header = if error_count > 0 && warning_count > 0 {
        format!("Validation Issues ({error_count} error(s), {warning_count} warning(s))")
    } else if error_count > 0 {
        format!("Validation Errors ({error_count})")
    } else {
        format!("Validation Warnings ({warning_count})")
    };

    display.print_section_header(header);

    let mut table = create_validation_table();
    table.set_header(vec!["Level", "Category", "Field", "Message", "Suggestion"]);

    for issue in &validation_result.issues {
        let level = match issue.level {
            ValidationLevel::Error => {
                if config.use_colors() {
                    style("ERROR").red().bold().to_string()
                } else {
                    "ERROR".to_string()
                }
            }
            ValidationLevel::Warning => {
                if config.use_colors() {
                    style("WARN").yellow().bold().to_string()
                } else {
                    "WARN".to_string()
                }
            }
        };

        let category = if config.use_colors() {
            style(&issue.category).magenta().to_string()
        } else {
            issue.category.clone()
        };

        let field = if config.use_colors() {
            style(&issue.field).cyan().to_string()
        } else {
            issue.field.clone()
        };

        let suggestion = issue.suggestion.as_deref().unwrap_or("-");

        table.add_row(vec![
            level,
            category,
            field,
            issue.message.clone(),
            suggestion.to_string(),
        ]);
    }

    display.println(format!("{table}"));
}

fn create_validation_table() -> Table {
    let mut table = Table::new();
    table
        .load_preset(presets::UTF8_FULL_CONDENSED)
        .apply_modifier(modifiers::UTF8_ROUND_CORNERS)
        .set_content_arrangement(ContentArrangement::Dynamic);
    table
}
