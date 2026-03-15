use comfy_table::{ContentArrangement, Table, modifiers, presets};
use console::style;
use selfie::package::{
    event::{PackageEvent, ValidationLevel, ValidationResultData, ValidationStatus},
    service::PackageService,
};

use crate::{
    config::CliConfig, display_manager::DisplayManager, event_processor::EventProcessor,
    formatters::format_key,
};

pub(crate) async fn handle_validate(
    service: &impl PackageService,
    package_name: &str,
    config: &CliConfig,
    display: &DisplayManager,
) -> i32 {
    tracing::debug!("Running validate command for package: {}", package_name);

    display.print_progress(format!("Validating {package_name}..."));

    // Call the service's validate method to get an event stream
    let event_stream = service.validate(package_name, None).await;

    // Process the event stream with custom handling for structured data
    let processor = EventProcessor::new(display.clone());
    let result = processor
        .process_events(event_stream, |event| handle_validate_event(event, config))
        .await;
    result.exit_code
}

fn handle_validate_event(event: &PackageEvent, config: &CliConfig) -> bool {
    match event {
        PackageEvent::ValidationResultCompleted {
            validation_result, ..
        } => {
            display_validation_result(validation_result, config);
            true // Handled
        }
        PackageEvent::Progress { .. } => {
            true // Handled - suppress progress for validate
        }
        _ => false, // Use default handling for other events
    }
}

fn display_validation_result(validation_result: &ValidationResultData, config: &CliConfig) {
    match validation_result.status {
        ValidationStatus::Valid => {
            // Show success card for valid packages
            display_validation_success_card(validation_result, config);
        }
        ValidationStatus::HasWarnings | ValidationStatus::HasErrors => {
            // Show table for packages with issues
            display_validation_issues_table(validation_result, config);
        }
    }
}

fn display_validation_success_card(validation_result: &ValidationResultData, config: &CliConfig) {
    println!();
    println!("📋 Validation Results:");

    let format_key_fn =
        |field: &str| -> String { format!("   {}: ", format_key(field, config.use_colors())) };

    println!(
        "{}{}",
        format_key_fn("Package"),
        validation_result.package_name
    );
    println!(
        "{}{}",
        format_key_fn("Environment"),
        validation_result.environment
    );

    let status_key = if config.use_colors() {
        style("Status").cyan().bold().to_string()
    } else {
        "Status".to_string()
    };
    let status_text = if config.use_colors() {
        style("✓ Valid").green().to_string()
    } else {
        "✓ Valid".to_string()
    };
    let status = format!("   {}: {}", status_key, status_text);
    println!("{status}");
}

fn display_validation_issues_table(validation_result: &ValidationResultData, config: &CliConfig) {
    if validation_result.issues.is_empty() {
        return;
    }

    println!();

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

    let summary = if error_count > 0 && warning_count > 0 {
        format!("📋 Validation Issues ({error_count} error(s), {warning_count} warning(s)):")
    } else if error_count > 0 {
        format!("📋 Validation Errors ({error_count}):")
    } else {
        format!("📋 Validation Warnings ({warning_count}):")
    };

    println!("{summary}");

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

    println!("{table}");
}

fn create_validation_table() -> Table {
    let mut table = Table::new();
    table
        .load_preset(presets::UTF8_FULL_CONDENSED)
        .apply_modifier(modifiers::UTF8_ROUND_CORNERS)
        .set_content_arrangement(ContentArrangement::Dynamic);
    table
}

#[cfg(test)]
mod tests {
    use super::*;
    use selfie::package::event::{
        ValidationIssueData, ValidationLevel, ValidationResultData, ValidationStatus,
    };
    use test_common::TEST_ENV;

    fn create_test_validation_result(status: ValidationStatus) -> ValidationResultData {
        ValidationResultData {
            package_name: "test-package".to_string(),
            environment: TEST_ENV.to_string(),
            status,
            issues: vec![],
        }
    }

    #[test]
    fn test_display_validation_result_success() {
        let config = CliConfig::wrap_for_test(test_common::test_config());
        let validation_result = create_test_validation_result(ValidationStatus::Valid);

        // Should not panic
        display_validation_result(&validation_result, &config);
    }

    #[test]
    fn test_display_validation_success_card() {
        let config = CliConfig::wrap_for_test(test_common::test_config());
        let validation_result = create_test_validation_result(ValidationStatus::Valid);

        // Should not panic
        display_validation_success_card(&validation_result, &config);
    }

    #[test]
    fn test_display_validation_result_with_colors() {
        let config = CliConfig::wrap_for_test_with_colors(test_common::test_config());
        let validation_result = create_test_validation_result(ValidationStatus::Valid);

        // Should not panic with colors enabled
        display_validation_success_card(&validation_result, &config);
    }

    #[test]
    fn test_display_validation_issues_table_empty() {
        let config = CliConfig::wrap_for_test(test_common::test_config());
        let validation_result = create_test_validation_result(ValidationStatus::Valid);

        // Should not display anything for empty issues
        display_validation_issues_table(&validation_result, &config);
    }

    #[test]
    fn test_display_validation_result_with_issues() {
        let config = CliConfig::wrap_for_test(test_common::test_config());
        let mut validation_result = create_test_validation_result(ValidationStatus::HasErrors);
        validation_result.issues = vec![ValidationIssueData {
            level: ValidationLevel::Error,
            category: "package".to_string(),
            field: "name".to_string(),
            message: "Package name is required".to_string(),
            suggestion: Some("Add a name field".to_string()),
        }];

        // Should not panic
        display_validation_issues_table(&validation_result, &config);
    }

    #[test]
    fn test_create_validation_table() {
        let table = create_validation_table();
        // Just test that table creation doesn't panic
        let _table_str = table.to_string();
    }
}
