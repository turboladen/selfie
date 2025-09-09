use comfy_table::{ContentArrangement, Table, modifiers, presets};
use console::style;
use selfie::{
    config::AppConfig,
    package::{
        event::{PackageEvent, ValidationLevel, ValidationResultData, ValidationStatus},
        service::PackageService,
    },
};

use crate::{
    commands::package::common::{self, create_package_service, report_status},
    event_processor::EventProcessor,
    formatters::format_key,
    terminal_progress_reporter::TerminalProgressReporter,
};

pub(crate) async fn handle_validate(
    package_name: &str,
    config: &AppConfig,
    reporter: TerminalProgressReporter,
) -> i32 {
    tracing::debug!("Running validate command for package: {}", package_name);

    report_status(&format!("Validating {package_name}..."));

    // Create the package service
    let service = create_package_service(config);

    // Create tracker for consistent error handling
    let mut tracker = common::PackageNotFoundTracker::new();

    // Call the service's validate method to get an event stream
    match service.validate(package_name, None).await {
        Ok(event_stream) => {
            // Process the event stream with custom handling for structured data
            let processor = EventProcessor::new(reporter);
            let result = processor
                .process_events(event_stream, |event| {
                    handle_validate_event(event, config, &mut tracker)
                })
                .await;
            result.exit_code
        }
        Err(e) => {
            // Handle the initial error - likely PackageNotFound
            let streamed_error = selfie::package::event::error::StreamedError::PackageRepoError(
                selfie::package::port::PackageRepoError::PackageError(Box::new(e)),
            );
            if tracker.handle_package_not_found_error(&streamed_error) {
                1
            } else {
                reporter.report_error(format!("Failed to validate package: {streamed_error}"));
                1
            }
        }
    }
}

fn handle_validate_event(
    event: &PackageEvent,
    config: &AppConfig,
    tracker: &mut common::PackageNotFoundTracker,
) -> bool {
    match event {
        PackageEvent::ValidationResultCompleted {
            validation_result, ..
        } => {
            display_validation_result(validation_result, config);
            true // Handled
        }
        PackageEvent::Error { error, .. } => {
            // Handle PackageNotFound errors consistently
            if tracker.handle_package_not_found_error(error) {
                return true; // Handled - prevent duplicate error display
            }
            false // Use default handling for other errors
        }
        PackageEvent::Completed { result, .. } => {
            // Suppress completion errors if we already handled PackageNotFound
            if tracker.should_suppress_completion_error(result) {
                return true; // Handled - suppress duplicate error
            }
            false // Use default handling for other completion events
        }
        PackageEvent::Progress { .. } => {
            true // Handled - suppress progress for validate
        }
        _ => false, // Use default handling for other events
    }
}

fn display_validation_result(validation_result: &ValidationResultData, config: &AppConfig) {
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

fn display_validation_success_card(validation_result: &ValidationResultData, config: &AppConfig) {
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

    let reporter = TerminalProgressReporter::new(config.use_colors());
    let status_key = if config.use_colors() {
        console::style("Status").cyan().bold().to_string()
    } else {
        "Status".to_string()
    };
    let status = format!("   {}: {}", status_key, reporter.format_success("Valid"));
    println!("{status}");
}

fn display_validation_issues_table(validation_result: &ValidationResultData, config: &AppConfig) {
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
    use test_common::{TEST_ENV, test_config, test_config_with_colors};

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
        let config = test_config();
        let validation_result = create_test_validation_result(ValidationStatus::Valid);

        // Should not panic
        display_validation_result(&validation_result, &config);
    }

    #[test]
    fn test_display_validation_success_card() {
        let config = test_config();
        let validation_result = create_test_validation_result(ValidationStatus::Valid);

        // Should not panic
        display_validation_success_card(&validation_result, &config);
    }

    #[test]
    fn test_display_validation_result_with_colors() {
        let config = test_config_with_colors();
        let validation_result = create_test_validation_result(ValidationStatus::Valid);

        // Should not panic with colors enabled
        display_validation_success_card(&validation_result, &config);
    }

    #[test]
    fn test_display_validation_issues_table_empty() {
        let config = test_config();
        let validation_result = create_test_validation_result(ValidationStatus::Valid);

        // Should not display anything for empty issues
        display_validation_issues_table(&validation_result, &config);
    }

    #[test]
    fn test_display_validation_result_with_issues() {
        let config = test_config();
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
