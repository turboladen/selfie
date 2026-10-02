use selfie::package::{
    event::{CheckResult, CheckResultData, OperationFailure, OperationResult, PackageEvent},
    port::PackageError,
    service::PackageService,
};

use crate::{
    commands::common,
    config::CliConfig,
    display_manager::{DisplayManager, INDENT},
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

    let event_stream = service.check(package_name).await;

    let verbose = display.is_verbose();

    let processor = EventProcessor::new(display.clone());
    let result = processor
        .process_events(event_stream, |event| match event {
            // No check command means no answer: the failure that follows
            // explains it on stderr, and a card on stdout would offer an answer.
            PackageEvent::CheckResultCompleted { check_result, .. }
                if matches!(check_result.result, CheckResult::NoCheckCommand) =>
            {
                true
            }
            PackageEvent::CheckResultCompleted { check_result, .. } => {
                if verbose {
                    display_check_result_card(check_result, config, display);
                } else {
                    display_check_output_only(check_result, display);
                }
                true
            }
            // The summary is the result every run prints, so a success is left to
            // the shared handler.
            PackageEvent::Completed { result, .. } => match result {
                OperationResult::Failure(failure) if failure.is_environment_error() => {
                    display_environment_error(package_name, failure, config, display);
                    true
                }
                _ => false,
            },
            _ => false,
        })
        .await;

    result.exit_code
}

/// Display environment error with helpful suggestions from the typed failure data
fn display_environment_error(
    package_name: &str,
    failure: &OperationFailure,
    config: &CliConfig,
    display: &DisplayManager,
) {
    display.print_note("");

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
    } else if let OperationFailure::Package(PackageError::NoCheckCommand {
        environment,
        other_envs_with_check,
        ..
    }) = failure
    {
        // The environment is there; only its check command is missing, so the
        // environment advice below would be false.
        common::display_missing_command(
            display,
            "check",
            package_name,
            environment,
            other_envs_with_check,
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
            // Not installed: the check's own output explains the answer, so it
            // is a warning, on stderr, beside the result line on stdout.
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
