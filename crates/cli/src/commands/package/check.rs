use selfie::package::{
    event::{CheckResult, CheckResultData, OperationFailure, OperationResult, PackageEvent},
    port::PackageError,
    service::PackageService,
};

use crate::{
    commands::common,
    config::CliConfig,
    display_manager::{DisplayManager, INDENT, OperationHandle},
    event_processor::EventProcessor,
    formatters::format_key,
    status_style,
};

pub(crate) async fn handle_check(
    service: &impl PackageService,
    package_name: &str,
    config: &CliConfig,
    display: &DisplayManager,
) -> i32 {
    tracing::debug!("Running check command for package: {}", package_name);

    // Spinner for TTY; static fallback otherwise
    let mut spinner: Option<OperationHandle> = if display.is_tty() {
        Some(display.start_operation(format!("Checking {package_name}...")))
    } else {
        display.print_progress(format!("Checking {package_name}..."));
        None
    };

    let event_stream = service.check(package_name).await;

    let mut env_error_handled = false;
    let verbose = config.verbose();

    let processor = EventProcessor::new(display.clone());
    let result = processor
        .process_events(event_stream, |event| {
            match event {
                PackageEvent::CheckResultCompleted { check_result, .. } => {
                    // Finalize spinner before displaying results
                    if let Some(s) = spinner.take() {
                        s.finish_clear();
                    }
                    if verbose {
                        display_check_result_card(check_result, config, display);
                    } else {
                        display_check_output_only(check_result, display);
                    }
                    true
                }
                PackageEvent::Progress {
                    step,
                    total_steps,
                    message,
                    ..
                } => {
                    if verbose {
                        false // Use default progress handling
                    } else if let Some(s) = spinner.as_ref() {
                        s.update_progress(*step, *total_steps, message);
                        true
                    } else {
                        display.print_progress(message);
                        true
                    }
                }
                PackageEvent::Completed { result, .. } => {
                    // Finalize spinner on completion if not already done
                    if let Some(s) = spinner.take() {
                        s.finish_clear();
                    }
                    if let OperationResult::Failure(failure) = result
                        && failure.is_environment_error()
                    {
                        display_environment_error(package_name, failure, config, display);
                        env_error_handled = true;
                        return true;
                    }
                    false
                }
                _ => false,
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
            // Not installed — use warning (not error) to match verbose mode's
            // "Not installed" severity and keep output on stdout
            if !stderr.is_empty() {
                display.print_warning(format!("Check failed: {}", stderr.trim()));
            } else if !stdout.is_empty() {
                display.print_warning(format!("Check failed: {}", stdout.trim()));
            } else {
                display.print_warning("Check failed with no output");
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
    let use_colors = config.use_colors();

    // Common fields via ResultCard
    display
        .result_card("Check Results")
        .field("Package", &check_result.package_name)
        .field("Environment", &check_result.environment)
        .field_if("Command", check_result.check_command.as_deref())
        .print();

    // Status line stays inline — complex branching with conditional sub-fields
    let format_key_fn =
        |field: &str| -> String { format!("{}{}: ", INDENT, format_key(field, use_colors)) };

    let status_line = match &check_result.result {
        CheckResult::Success { stdout, stderr } => {
            let status = format!(
                "{}{}",
                format_key_fn("Status"),
                status_style::format_installed(use_colors)
            );
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
            format!(
                "{}{}",
                format_key_fn("Status"),
                status_style::format_no_check(use_colors)
            )
        }
        CheckResult::CommandNotFound => {
            format!(
                "{}{}",
                format_key_fn("Status"),
                status_style::format_cmd_not_found(use_colors)
            )
        }
        CheckResult::Error(error) => {
            format!(
                "{}{}\n{}{}",
                format_key_fn("Status"),
                status_style::format_status_error(use_colors),
                format_key_fn("Details"),
                error
            )
        }
    };

    display.println(status_line);
}
