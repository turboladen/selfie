use selfie::package::{
    event::{CheckResult, CheckResultData, OperationFailure, OperationResult, PackageEvent},
    port::PackageError,
    service::PackageService,
};

use crate::terminal_text::{Shown, shown, tool_output};
use crate::{
    commands::presentation,
    config::CliConfig,
    display_manager::{DisplayManager, INDENT},
    event_processor::{EventProcessor, Exit},
    formatters::format_key,
    status_style,
};

pub(crate) async fn handle_check(
    service: &impl PackageService,
    package_name: &str,
    config: &CliConfig,
    display: &DisplayManager,
) -> Exit {
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

    result.exit
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
        presentation::display_environment_summary(
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
        presentation::display_missing_command(
            display,
            "check",
            package_name,
            environment,
            other_envs_with_check,
        );
    } else {
        presentation::display_generic_environment_suggestion(
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
                display.print_info(format!("Check output: {}", tool_output(stdout)));
            } else if !stderr.trim().is_empty() {
                display.print_info(format!("Check output: {}", tool_output(stderr)));
            }
        }
        CheckResult::Failed { stdout, stderr, .. } => {
            // Not installed: the check's own output explains the answer, so it
            // is a warning, on stderr, beside the result line on stdout.
            if !stderr.is_empty() {
                display.print_warning(format!("Check failed: {}", tool_output(stderr)));
            } else if !stdout.is_empty() {
                display.print_warning(format!("Check failed: {}", tool_output(stdout)));
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

    // Status line stays inline — complex branching with conditional sub-fields
    let key = |field: &'static str| shown!("{}{}: ", INDENT, format_key(field, use_colors));
    let output = |field: &'static str, text: &str| {
        shown!("\n{}{}", key(field), Shown::tool_output(text, "    "))
    };

    let status_line = match &check_result.result {
        CheckResult::Success { stdout, stderr } => {
            let status = shown!(
                "{}{}",
                key("Status"),
                status_style::format_installed(use_colors)
            );
            if !stdout.trim().is_empty() {
                shown!("{}{}", status, output("Output", stdout))
            } else if !stderr.trim().is_empty() {
                shown!("{}{}", status, output("Output", stderr))
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
            let status = shown!(
                "{}{}",
                key("Status"),
                status_style::format_not_installed(use_colors)
            );
            if !stderr.is_empty() {
                shown!("{}{}", status, output("Details", stderr))
            } else if !stdout.is_empty() {
                shown!("{}{}", status, output("Details", stdout))
            } else if let Some(code) = exit_code {
                shown!("{}\n{}Exit code {}", status, key("Details"), *code)
            } else {
                status
            }
        }
        // No answer, so no card: the failure that follows says why on stderr.
        CheckResult::NoCheckCommand | CheckResult::CouldNotRun(_) => return,
    };

    display
        .result_card("Check Results")
        .field("Package", &check_result.package_name)
        .field("Environment", &check_result.environment)
        .field_if("Command", check_result.check_command.as_deref())
        .print();
    display.println(status_line);
}
