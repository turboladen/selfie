use selfie::package::{
    event::{AuditResult, AuditResultData, OperationFailure, OperationResult, PackageEvent},
    port::PackageError,
    service::PackageService,
};

use crate::formatters::name_list;
use crate::terminal_text::{Literal, Name, Output, Paint, Shown, interim_one_line, lit, shown};
use crate::{
    commands::presentation,
    config::CliConfig,
    display_manager::DisplayManager,
    event_processor::{EventProcessor, Exit},
    status_style,
};

pub(crate) async fn handle_audit(
    service: &impl PackageService,
    package_name: &str,
    config: &CliConfig,
    display: &DisplayManager,
) -> Exit {
    tracing::debug!("Running audit command for package: {}", package_name);

    let event_stream = service.audit(package_name).await;

    let processor = EventProcessor::new(display.clone());
    let result = processor
        .process_events(event_stream, |event| match event {
            PackageEvent::AuditResultCompleted { audit_result, .. } => {
                if display.is_verbose() {
                    display_audit_result_card(audit_result, config, display);
                } else {
                    display_audit_output_only(audit_result, display);
                }
                true
            }
            // The summary is left to the shared handler at every outcome: it is
            // the line that names the environment the audit ran in.
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

pub(crate) async fn handle_audit_all(
    service: &impl PackageService,
    config: &CliConfig,
    display: &DisplayManager,
) -> Exit {
    tracing::debug!("Running audit command for all packages");

    let event_stream = service.audit_all().await;

    let processor = EventProcessor::new(display.clone());
    let result = processor
        .process_events(event_stream, |event| match event {
            PackageEvent::AuditResultCompleted { audit_result, .. } => {
                display_audit_summary_line(audit_result, config, display);
                true
            }
            _ => false,
        })
        .await;

    result.exit
}

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
            presentation::PackageCommand::Audit,
        );
    } else {
        presentation::display_generic_environment_suggestion(
            package_name,
            config.environment(),
            config,
            display,
            presentation::PackageCommand::Audit,
        );
    }
}

fn display_audit_output_only(audit_result: &AuditResultData, display: &DisplayManager) {
    let name = Name::new(&audit_result.package_name);
    match &audit_result.result {
        AuditResult::Clean { sources } => {
            display.print_success(crate::terminal_text::Prose::message(format!(
                "{name} — clean ({})",
                name_list(sources)
            )));
        }
        AuditResult::Conflicts {
            sources, expected, ..
        } => {
            display.print_warning(crate::terminal_text::Prose::message(format!(
                "{name} — conflicts detected: unexpected source(s): {}",
                name_list(unexpected(sources, expected))
            )));
        }
        AuditResult::NotInstalled => {
            display.print_info(crate::terminal_text::Prose::message(format!(
                "{name} — not installed"
            )));
        }
        AuditResult::NoAuditCommand => {
            display.print_info(crate::terminal_text::Prose::message(format!(
                "{name} — no audit command defined"
            )));
        }
        AuditResult::Error(err) => {
            display.print_error(crate::terminal_text::Prose::message(format!(
                "{name} — error: {}",
                Output::new(err)
            )));
        }
        AuditResult::CouldNotRun(failure) => {
            display.print_error(crate::terminal_text::Prose::message(format!(
                "{name} — error: {failure}"
            )));
        }
    }
}

fn display_audit_summary_line(
    audit_result: &AuditResultData,
    config: &CliConfig,
    display: &DisplayManager,
) {
    let use_colors = config.use_colors();
    let label = |text: Literal, paint: Paint| Shown::from(text).paint(use_colors, paint);
    let error = |err: &str| {
        shown!(
            "{}: {}",
            label(lit!("error"), |s| s.red()),
            Output::new(err)
        )
    };

    let status = match &audit_result.result {
        AuditResult::Clean { sources } => {
            shown!(
                "{} ({})",
                label(lit!("clean"), |s| s.green()),
                name_list(sources)
            )
        }
        AuditResult::Conflicts {
            sources, expected, ..
        } => shown!(
            "{} — unexpected: {}",
            label(lit!("CONFLICTS"), |s| s.red().bold()),
            name_list(unexpected(sources, expected))
        ),
        AuditResult::NotInstalled => label(lit!("not installed"), |s| s.dim()),
        AuditResult::NoAuditCommand => label(lit!("no audit command"), |s| s.dim()),
        AuditResult::Error(err) => error(err),
        AuditResult::CouldNotRun(failure) => error(&failure.to_string()),
    };

    display.println_after(
        shown!("  {}: ", Name::new(&audit_result.package_name)),
        status,
    );
}

// The sources an audit found that the spec does not expect.
fn unexpected<'a>(sources: &'a [String], expected: &[String]) -> Vec<&'a String> {
    sources
        .iter()
        .filter(|s| !expected.iter().any(|e| s.eq_ignore_ascii_case(e)))
        .collect()
}

fn display_audit_result_card(
    audit_result: &AuditResultData,
    config: &CliConfig,
    display: &DisplayManager,
) {
    let use_colors = config.use_colors();

    // The status and what the audit found, as fields after the common ones.
    let error = |err: &str| {
        vec![
            (
                lit!("Status"),
                status_style::format_status_error(use_colors),
            ),
            (lit!("Details"), Shown::from(Output::new(err))),
        ]
    };
    let status: Vec<(Literal, Shown)> = match &audit_result.result {
        AuditResult::Clean { sources } => vec![
            (lit!("Status"), status_style::format_audit_clean(use_colors)),
            (lit!("Sources"), name_list(sources)),
        ],
        AuditResult::Conflicts {
            sources, expected, ..
        } => vec![
            (
                lit!("Status"),
                status_style::format_audit_conflicts(use_colors),
            ),
            (lit!("All sources"), name_list(sources)),
            (lit!("Expected"), name_list(expected)),
            (lit!("Unexpected"), name_list(unexpected(sources, expected))),
        ],
        AuditResult::NotInstalled => vec![(
            lit!("Status"),
            status_style::format_not_installed(use_colors),
        )],
        AuditResult::NoAuditCommand => {
            vec![(lit!("Status"), status_style::format_no_audit(use_colors))]
        }
        AuditResult::Error(err) => error(err),
        AuditResult::CouldNotRun(failure) => error(&failure.to_string()),
    };

    display
        .result_card(lit!("Audit Results"))
        .field(lit!("Package"), Name::new(&audit_result.package_name))
        .field(lit!("Environment"), Name::new(&audit_result.environment))
        .command_if(
            lit!("Command"),
            audit_result.audit_command.as_deref().map(interim_one_line),
        )
        .styled_fields(status)
        .print();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::display_manager::column;
    use selfie::package::event::CheckFailure;

    // Every view of an audit shows the package's name and its sources as names,
    // so an invisible character in one shows as an escape in each, whatever
    // the audit found.
    #[test]
    fn each_audit_view_shows_names_and_sources_escaped() {
        let failure = CheckFailure::TimedOut(selfie::commands::runner::TimedOut::new(
            "sleep 9",
            std::time::Duration::from_secs(1),
        ));
        let config = CliConfig::wrap_for_test(test_common::test_config_with_dir(
            std::path::Path::new("/packages"),
        ));
        let results = [
            AuditResult::Clean {
                sources: vec!["brew\u{200B}".to_string()],
            },
            AuditResult::Conflicts {
                sources: vec!["brew\u{200B}".to_string(), "npm".to_string()],
                expected: vec!["npm".to_string()],
            },
            AuditResult::NotInstalled,
            AuditResult::NoAuditCommand,
            AuditResult::Error("e".to_string()),
            AuditResult::CouldNotRun(failure),
        ];
        for result in results {
            let names_sources = matches!(
                result,
                AuditResult::Clean { .. } | AuditResult::Conflicts { .. }
            );
            let audit = AuditResultData {
                package_name: "p\u{200B}".to_string(),
                environment: "test".to_string(),
                audit_command: None,
                result,
            };
            let views: [&dyn Fn(&DisplayManager); 3] = [
                &|display| display_audit_output_only(&audit, display),
                &|display| display_audit_summary_line(&audit, &config, display),
                &|display| display_audit_result_card(&audit, &config, display),
            ];
            for view in views {
                let display = DisplayManager::new(false);
                view(&display);
                let text: String = display
                    .printed()
                    .into_iter()
                    .map(|(_, line)| line + "\n")
                    .collect();

                assert!(text.contains("p\\u{200b}"), "{text:?}");
                if names_sources {
                    assert!(text.contains("brew\\u{200b}"), "{text:?}");
                }
                assert!(!text.contains('\u{200B}'), "{text:?}");
            }
        }
    }

    // An audit that could not run prints, in each of the three places an audit
    // result is shown, exactly what an audit error with the same sentence
    // prints.
    #[test]
    fn an_audit_that_could_not_run_prints_as_an_audit_error() {
        let failure = CheckFailure::TimedOut(selfie::commands::runner::TimedOut::new(
            "sleep 9",
            std::time::Duration::from_secs(1),
        ));
        let row = |result| AuditResultData {
            package_name: "p".to_string(),
            environment: "test".to_string(),
            audit_command: Some("sleep 9".to_string()),
            result,
        };
        let config = CliConfig::wrap_for_test(test_common::test_config_with_dir(
            std::path::Path::new("/packages"),
        ));
        let printed = |audit: &AuditResultData| {
            let display = DisplayManager::new(false);
            display_audit_output_only(audit, &display);
            display_audit_summary_line(audit, &config, &display);
            display_audit_result_card(audit, &config, &display);
            display.printed()
        };

        let could_not_run = printed(&row(AuditResult::CouldNotRun(failure.clone())));
        let error = printed(&row(AuditResult::Error(failure.to_string())));

        assert_eq!(could_not_run, error);
        let sentence = failure.to_string();
        assert_eq!(
            could_not_run
                .iter()
                .filter(|(_, line)| line.contains(&sentence))
                .count(),
            3,
            "{could_not_run:?}"
        );
    }

    // An audit error's lines after the first continue under its first: in the
    // summary line under `error`, past the package's name, and in the card
    // under the Details value.
    #[test]
    fn an_audit_error_continues_under_its_text() {
        let config = CliConfig::wrap_for_test(test_common::test_config_with_dir(
            std::path::Path::new("/packages"),
        ));
        let audit = AuditResultData {
            package_name: "pkg".to_string(),
            environment: "test".to_string(),
            audit_command: None,
            result: AuditResult::Error("e1\ne2".to_string()),
        };

        let display = DisplayManager::new(false);
        display_audit_summary_line(&audit, &config, &display);
        let summary = display.written().join("\n");
        assert!(summary.starts_with("  pkg: error: e1"), "{summary:?}");
        assert_eq!(
            column(&summary, "e2"),
            column(&summary, "error"),
            "{summary:?}"
        );

        let display = DisplayManager::new(false);
        display_audit_result_card(&audit, &config, &display);
        let details = display
            .written()
            .into_iter()
            .find(|line| line.contains("Details"))
            .expect("the card has details");
        assert_eq!(
            column(&details, "e2"),
            column(&details, "e1"),
            "{details:?}"
        );
    }
}
