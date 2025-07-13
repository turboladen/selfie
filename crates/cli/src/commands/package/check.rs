use selfie::{
    config::AppConfig,
    package::{
        event::{CheckResult, CheckResultData, PackageEvent},
        service::PackageService,
    },
};

use crate::{
    commands::package::common::{create_package_service, report_status},
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

    #[allow(clippy::match_same_arms)]
    processor
        .process_events(event_stream, |event| {
            match event {
                PackageEvent::CheckResultCompleted { check_result, .. } => {
                    display_check_result_card(check_result, config);
                    true // Handled
                }
                PackageEvent::Progress { .. } => {
                    true // Handled
                }
                PackageEvent::Completed { .. } => {
                    false // Use default completion handling
                }
                _ => false, // Use default handling for other events
            }
        })
        .await
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
