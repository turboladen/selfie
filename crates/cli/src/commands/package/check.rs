use selfie::{
    config::AppConfig,
    package::{
        event::{
            CheckResult, CheckResultData, OperationFailure, OperationResult, PackageEvent,
            error::StreamedError,
        },
        port::{PackageError, PackageRepoError},
        service::PackageService,
    },
};

use crate::{
    commands::package::common::{self, create_package_service, report_status},
    event_processor::EventProcessor,
    formatters::format_key,
    terminal_progress_reporter::TerminalProgressReporter,
};

pub(crate) async fn handle_check(
    package_name: &str,
    config: &AppConfig,
    reporter: TerminalProgressReporter,
) -> i32 {
    tracing::debug!("Running check command for package: {}", package_name);

    // Create animated spinner for check operation
    report_status(&format!("Checking {package_name}..."));

    // Create the package service
    let service = create_package_service(config);

    // Call the service's check method to get an event stream
    let event_stream = service.check(package_name).await;

    // Process the event stream with custom handling for structured data
    let processor = EventProcessor::new(reporter);
    let result = processor
        .process_events(event_stream, |event| {
            match event {
                PackageEvent::CheckResultCompleted { check_result, .. } => {
                    display_check_result_card(check_result, config);
                    true // Handled
                }
                PackageEvent::Progress { .. } => {
                    true // Handled
                }
                PackageEvent::Error { error, .. } => {
                    // Handle environment configuration errors specially
                    match error {
                        StreamedError::PackageRepoError(PackageRepoError::PackageError(
                            pkg_error,
                        )) => {
                            match pkg_error.as_ref() {
                                PackageError::EnvironmentNotFound { .. } => {
                                    handle_environment_error(package_name, error, config);
                                    true // Handled completely - prevent duplicate error display
                                }
                                _ => false, // Use default handling for other errors
                            }
                        }
                        _ => false, // Use default handling for other errors
                    }
                }
                PackageEvent::Completed {
                    result: op_result, ..
                } => {
                    // Skip duplicate error display for environment configuration errors
                    match op_result {
                        OperationResult::Failure(failure) => {
                            match failure {
                                OperationFailure::EnvironmentError(_) => {
                                    true // Handled - we already showed the error message above
                                }
                                _ => false,
                            }
                        }
                        OperationResult::Success(_) => false,
                    }
                }
                _ => false, // Use default handling for other events
            }
        })
        .await;

    // Return proper exit code - 1 for environment errors, otherwise use result from processor
    if result.environment_error_handled {
        1
    } else {
        result.exit_code
    }
}

/// Handle environment configuration errors with helpful suggestions
fn handle_environment_error(package_name: &str, error: &StreamedError, config: &AppConfig) {
    // Show helpful information about available environments
    println!();

    // Try to extract environment information from the structured error
    if let StreamedError::PackageRepoError(PackageRepoError::PackageError(package_error)) = error {
        if let PackageError::EnvironmentNotFound {
            available_environments,
            ..
        } = package_error.as_ref()
        {
            common::display_environment_summary(
                package_name,
                config.environment(),
                available_environments,
                config,
                "check",
            );
            return;
        }
    }

    // Fallback to generic suggestion if we can't extract environment info
    common::display_generic_environment_suggestion(
        package_name,
        config.environment(),
        config,
        "check",
    );
}

fn display_check_result_card(check_result: &CheckResultData, config: &AppConfig) {
    println!();
    println!("📋 Check Results:");

    let format_key_fn =
        |field: &str| -> String { format!("   {}: ", format_key(field, config.use_colors())) };

    println!("{}{}", format_key_fn("Package"), check_result.package_name);
    println!(
        "{}{}",
        format_key_fn("Environment"),
        check_result.environment
    );

    if let Some(cmd) = &check_result.check_command {
        println!("{}{}", format_key_fn("Command"), cmd);
    }

    // Format status with appropriate icon and color
    let status_line = match &check_result.result {
        CheckResult::Success => {
            if config.use_colors() {
                format!(
                    "{}{}",
                    format_key_fn("Status"),
                    console::style("✅ Installed").green().bold()
                )
            } else {
                format!("{}✅ Installed", format_key_fn("Status"))
            }
        }
        CheckResult::Failed {
            stderr, exit_code, ..
        } => {
            let status = if config.use_colors() {
                format!(
                    "{}{}",
                    format_key_fn("Status"),
                    console::style("❌ Not installed").red().bold()
                )
            } else {
                format!("{}❌ Not installed", format_key_fn("Status"))
            };

            if !stderr.is_empty() {
                format!("{}\n{}{}", status, format_key_fn("Details"), stderr.trim())
            } else if let Some(code) = exit_code {
                format!("{}\n{}Exit code {}", status, format_key_fn("Details"), code)
            } else {
                status
            }
        }
        CheckResult::NoCheckCommand => {
            if config.use_colors() {
                format!(
                    "   {}: {}",
                    console::style("Status").cyan().bold(),
                    console::style("⚠️ No check command defined").yellow()
                )
            } else {
                "   Status: ⚠️ No check command defined".to_string()
            }
        }
        CheckResult::CommandNotFound => {
            if config.use_colors() {
                format!(
                    "   {}: {}",
                    console::style("Status").cyan().bold(),
                    console::style("❌ Command not found").red().bold()
                )
            } else {
                "   Status: ❌ Command not found".to_string()
            }
        }
        CheckResult::Error(error) => {
            if config.use_colors() {
                format!(
                    "   {}: {}\n   {}: {}",
                    console::style("Status").cyan().bold(),
                    console::style("❌ Error").red().bold(),
                    console::style("Details").cyan().bold(),
                    error
                )
            } else {
                format!("   Status: ❌ Error\n   Details: {error}")
            }
        }
    };

    println!("{status_line}");
}

#[cfg(test)]
mod tests {
    use super::*;
    use selfie::package::event::{CheckResult, CheckResultData};
    use test_common::{TEST_ENV, test_config, test_config_with_colors};

    #[test]
    fn test_display_check_result_card_success() {
        let config = test_config();
        let check_result = CheckResultData {
            package_name: "test-package".to_string(),
            environment: TEST_ENV.to_string(),
            check_command: Some("which test-command".to_string()),
            result: CheckResult::Success,
        };

        // Just test that the function doesn't panic
        display_check_result_card(&check_result, &config);
    }

    #[test]
    fn test_display_check_result_card_failed() {
        let config = test_config();
        let check_result = CheckResultData {
            package_name: "test-package".to_string(),
            environment: TEST_ENV.to_string(),
            check_command: Some("which missing-command".to_string()),
            result: CheckResult::Failed {
                stdout: String::new(),
                stderr: "command not found".to_string(),
                exit_code: Some(1),
            },
        };

        // Just test that the function doesn't panic
        display_check_result_card(&check_result, &config);
    }

    #[test]
    fn test_display_check_result_card_no_command() {
        let config = test_config();
        let check_result = CheckResultData {
            package_name: "test-package".to_string(),
            environment: TEST_ENV.to_string(),
            check_command: None,
            result: CheckResult::NoCheckCommand,
        };

        // Just test that the function doesn't panic
        display_check_result_card(&check_result, &config);
    }

    #[test]
    fn test_display_check_result_card_with_colors() {
        let config = test_config_with_colors();
        let check_result = CheckResultData {
            package_name: "test-package".to_string(),
            environment: TEST_ENV.to_string(),
            check_command: Some("which test-command".to_string()),
            result: CheckResult::Success,
        };

        // Just test that the function doesn't panic with colors enabled
        display_check_result_card(&check_result, &config);
    }

    #[test]
    fn test_display_check_result_card_error() {
        let config = test_config();
        let check_result = CheckResultData {
            package_name: "test-package".to_string(),
            environment: TEST_ENV.to_string(),
            check_command: Some("some-command".to_string()),
            result: CheckResult::Error("Network timeout".to_string()),
        };

        // Just test that the function doesn't panic
        display_check_result_card(&check_result, &config);
    }

    #[test]
    fn test_display_check_result_card_command_not_found() {
        let config = test_config();
        let check_result = CheckResultData {
            package_name: "test-package".to_string(),
            environment: TEST_ENV.to_string(),
            check_command: Some("missing-cmd".to_string()),
            result: CheckResult::CommandNotFound,
        };

        // Just test that the function doesn't panic
        display_check_result_card(&check_result, &config);
    }
}
