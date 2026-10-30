use selfie::package::{
    event::{CheckResult, CheckResultData, OperationFailure, OperationResult, PackageEvent},
    port::PackageError,
    service::PackageService,
};

use crate::received::{Check, CheckAnswer};
use crate::terminal_text::{Literal, Shown, lit, shown};
use crate::{
    commands::presentation,
    config::CliConfig,
    display_manager::DisplayManager,
    event_processor::{EventProcessor, Exit},
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
    display.print_note(lit!(""));

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
            presentation::PackageCommand::Check,
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
            presentation::PackageCommand::Check,
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
            presentation::PackageCommand::Check,
        );
    }
}

fn display_check_output_only(check_result: &CheckResultData, display: &DisplayManager) {
    match Check::from(check_result).answer {
        CheckAnswer::Installed { output } => {
            if let Some(output) = output {
                display.print_info(shown!("Check output: {}", output));
            }
        }
        // Not installed: the check's own output explains the answer, so it
        // is a warning, on stderr, beside the result line on stdout.
        CheckAnswer::NotInstalled { output, .. } => match output {
            Some(output) => display.print_warning(shown!("Check failed: {}", output)),
            None => display.print_warning(lit!("Check failed with no output")),
        },
        // For other cases, don't show additional output in non-verbose mode
        CheckAnswer::None => {}
    }
}

fn display_check_result_card(
    check_result: &CheckResultData,
    config: &CliConfig,
    display: &DisplayManager,
) {
    let use_colors = config.use_colors();
    let check = Check::from(check_result);

    // The status and what the check printed, as fields after the common ones.
    let status: Vec<(Literal, Shown)> = match check.answer {
        CheckAnswer::Installed { output } => {
            let mut fields = vec![(lit!("Status"), status_style::format_installed(use_colors))];
            if let Some(output) = output {
                fields.push((lit!("Output"), output.into()));
            }
            fields
        }
        CheckAnswer::NotInstalled { output, exit_code } => {
            let mut fields = vec![(
                lit!("Status"),
                status_style::format_not_installed(use_colors),
            )];
            if let Some(output) = output {
                fields.push((lit!("Details"), output.into()));
            } else if let Some(code) = exit_code {
                fields.push((lit!("Details"), shown!("Exit code {}", code)));
            }
            fields
        }
        // No answer, so no card: the failure that follows says why on stderr.
        CheckAnswer::None => return,
    };

    display
        .result_card(lit!("Check Results"))
        .field(lit!("Package"), check.package)
        .field(lit!("Environment"), check.environment)
        .command_if(lit!("Command"), check.command)
        .styled_fields(status)
        .print();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::display_manager::{Channel, column};

    // What a check printed continues under its first line in the card, past
    // its key, whether the check passed or failed.
    #[test]
    fn a_check_output_continues_under_its_first_line() {
        let config = CliConfig::wrap_for_test(test_common::test_config_with_dir(
            std::path::Path::new("/packages"),
        ));
        let results = [
            (
                CheckResult::Success {
                    stdout: "o1\no2\n".to_string(),
                    stderr: String::new(),
                },
                "Output",
            ),
            (
                CheckResult::Failed {
                    stdout: String::new(),
                    stderr: "o1\no2\n".to_string(),
                    exit_code: Some(1),
                },
                "Details",
            ),
        ];
        for (result, key) in results {
            let check = CheckResultData {
                package_name: "pkg".to_string(),
                environment: "test".to_string(),
                check_command: None,
                result,
            };
            let display = DisplayManager::new(false);

            display_check_result_card(&check, &config, &display);

            let line = display
                .written()
                .into_iter()
                .find(|line| line.contains(key))
                .expect("the card shows the output");
            assert!(line.starts_with(&format!("   {key}: o1")), "{line:?}");
            assert_eq!(column(&line, "o2"), column(&line, "o1"), "{line:?}");
        }
    }

    // A failed check whose stderr is only blank space shows in its card what it
    // printed on stdout instead, and with neither, its exit code; never an empty
    // Details.
    #[test]
    fn a_failed_check_with_blank_stderr_falls_back() {
        let config = CliConfig::wrap_for_test(test_common::test_config_with_dir(
            std::path::Path::new("/packages"),
        ));
        let details = |stdout: &str| {
            let check = CheckResultData {
                package_name: "pkg".to_string(),
                environment: "test".to_string(),
                check_command: None,
                result: CheckResult::Failed {
                    stdout: stdout.to_string(),
                    stderr: " \n\n".to_string(),
                    exit_code: Some(3),
                },
            };
            let display = DisplayManager::new(false);
            display_check_result_card(&check, &config, &display);
            display
                .written()
                .into_iter()
                .find(|line| line.contains("Details"))
                .expect("the card has details")
        };

        assert_eq!(details("from stdout\n"), "   Details: from stdout");
        assert_eq!(details("\n"), "   Details: Exit code 3");
    }

    // Without the card, a failed check whose stderr is only blank space warns
    // with what it printed on stdout instead, and with neither, says it printed
    // nothing; never an empty warning.
    #[test]
    fn a_failed_check_with_blank_stderr_warns_with_what_it_printed() {
        let warning = |stdout: &str| {
            let check = CheckResultData {
                package_name: "pkg".to_string(),
                environment: "test".to_string(),
                check_command: None,
                result: CheckResult::Failed {
                    stdout: stdout.to_string(),
                    stderr: " \n\n".to_string(),
                    exit_code: Some(3),
                },
            };
            let display = DisplayManager::new(false);
            display_check_output_only(&check, &display);
            display.printed()
        };

        assert_eq!(
            warning("from stdout\n"),
            [(Channel::Stderr, "Check failed: from stdout".to_string())]
        );
        assert_eq!(
            warning("\n"),
            [(Channel::Stderr, "Check failed with no output".to_string())]
        );
    }
}
