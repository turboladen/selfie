use selfie::package::{
    event::{CheckResult, CheckResultData, OperationFailure, OperationResult, PackageEvent},
    port::PackageError,
    service::PackageService,
};

use crate::{
    commands::common, config::CliConfig, display_manager::DisplayManager,
    event_processor::EventProcessor, formatters::format_key, status_style,
};

pub(crate) async fn handle_check(
    service: &impl PackageService,
    package_name: &str,
    config: &CliConfig,
    display: &DisplayManager,
) -> i32 {
    tracing::debug!("Running check command for package: {}", package_name);

    // Create animated spinner for check operation
    display.print_progress(format!("Checking {package_name}..."));

    // Call the service's check method to get an event stream
    let event_stream = service.check(package_name).await;

    // Track whether we handled an environment error in the Completed arm
    let mut env_error_handled = false;
    let verbose = config.verbose();

    // Process the event stream with custom handling for structured data
    let processor = EventProcessor::new(display.clone());
    let result = processor
        .process_events(event_stream, |event| {
            match event {
                PackageEvent::CheckResultCompleted { check_result, .. } => {
                    if verbose {
                        display_check_result_card(check_result, config, display);
                    } else {
                        display_check_output_only(check_result, display);
                    }
                    true // Handled
                }
                PackageEvent::Progress { .. } => {
                    if verbose {
                        false // Use default progress handling
                    } else {
                        true // Suppress in non-verbose mode
                    }
                }
                PackageEvent::Completed { result, .. } => {
                    if let OperationResult::Failure(failure) = result
                        && failure.is_environment_error()
                    {
                        display_environment_error(package_name, failure, config, display);
                        env_error_handled = true;
                        return true; // Handled
                    }
                    false // Use default handling for other completion events
                }
                _ => false, // Use default handling for other events
            }
        })
        .await;

    if env_error_handled {
        1
    } else {
        result.exit_code
    }
}

/// Display environment error with helpful suggestions from the typed failure data
fn display_environment_error(
    package_name: &str,
    failure: &OperationFailure,
    config: &CliConfig,
    display: &DisplayManager,
) {
    display.println("");

    if let OperationFailure::Package(PackageError::EnvironmentNotFound {
        available_environments,
        ..
    }) = failure
    {
        common::display_environment_summary(
            package_name,
            config.environment(),
            available_environments,
            config,
            display,
            "check",
        );
    } else {
        common::display_generic_environment_suggestion(
            package_name,
            config.environment(),
            config,
            display,
            "check",
        );
    }
}

fn display_check_output_only(check_result: &CheckResultData, display: &DisplayManager) {
    match &check_result.result {
        CheckResult::Success { stdout, stderr } => {
            // Show stdout output if present
            if !stdout.trim().is_empty() {
                display.print_info(format!("Check output: {}", stdout.trim()));
            } else if !stderr.trim().is_empty() {
                display.print_info(format!("Check output: {}", stderr.trim()));
            }
        }
        CheckResult::Failed { stdout, stderr, .. } => {
            // Show error output
            if !stderr.is_empty() {
                display.print_error(format!("Check failed: {}", stderr.trim()));
            } else if !stdout.is_empty() {
                display.print_error(format!("Check failed: {}", stdout.trim()));
            } else {
                display.print_error("Check failed with no output");
            }
        }
        _ => {
            // For other cases, don't show additional output in non-verbose mode
        }
    }
}

fn display_check_result_card(
    check_result: &CheckResultData,
    config: &CliConfig,
    display: &DisplayManager,
) {
    display.println("");
    display.print_section_header("Check Results");

    let format_key_fn =
        |field: &str| -> String { format!("   {}: ", format_key(field, config.use_colors())) };

    display.println(format!(
        "{}{}",
        format_key_fn("Package"),
        check_result.package_name
    ));
    display.println(format!(
        "{}{}",
        format_key_fn("Environment"),
        check_result.environment
    ));

    if let Some(cmd) = &check_result.check_command {
        display.println(format!("{}{}", format_key_fn("Command"), cmd));
    }

    let use_colors = config.use_colors();

    // Format status with appropriate icon and color
    let status_line = match &check_result.result {
        CheckResult::Success { stdout, stderr } => {
            let status = format!(
                "{}{}",
                format_key_fn("Status"),
                status_style::format_installed(use_colors)
            );

            // Show stdout output if present
            if !stdout.trim().is_empty() {
                format!("{}\n{}{}", status, format_key_fn("Output"), stdout.trim())
            } else if !stderr.trim().is_empty() {
                format!("{}\n{}{}", status, format_key_fn("Output"), stderr.trim())
            } else {
                status
            }
        }
        CheckResult::Failed {
            stdout,
            stderr,
            exit_code,
            ..
        } => {
            let status = format!(
                "{}{}",
                format_key_fn("Status"),
                status_style::format_not_installed(use_colors)
            );

            if !stderr.is_empty() {
                format!("{}\n{}{}", status, format_key_fn("Details"), stderr.trim())
            } else if !stdout.is_empty() {
                format!("{}\n{}{}", status, format_key_fn("Details"), stdout.trim())
            } else if let Some(code) = exit_code {
                format!("{}\n{}Exit code {}", status, format_key_fn("Details"), code)
            } else {
                status
            }
        }
        CheckResult::NoCheckCommand => {
            let status_key = if use_colors {
                console::style("Status").cyan().bold().to_string()
            } else {
                "Status".to_string()
            };
            format!(
                "   {}: {}",
                status_key,
                status_style::format_no_check(use_colors)
            )
        }
        CheckResult::CommandNotFound => {
            let status_key = if use_colors {
                console::style("Status").cyan().bold().to_string()
            } else {
                "Status".to_string()
            };
            format!(
                "   {}: {}",
                status_key,
                status_style::format_cmd_not_found(use_colors)
            )
        }
        CheckResult::Error(error) => {
            let status_key = if use_colors {
                console::style("Status").cyan().bold().to_string()
            } else {
                "Status".to_string()
            };
            let details_key = if use_colors {
                console::style("Details").cyan().bold().to_string()
            } else {
                "Details".to_string()
            };
            format!(
                "   {}: {}\n   {}: {}",
                status_key,
                status_style::format_status_error(use_colors),
                details_key,
                error
            )
        }
    };

    display.println(status_line);
}
