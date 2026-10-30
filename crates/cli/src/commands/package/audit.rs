use selfie::package::{
    event::{AuditResult, AuditResultData, OperationFailure, OperationResult, PackageEvent},
    port::PackageError,
    service::PackageService,
};

use crate::terminal_text::{Literal, Name, Output, Paint, Shown, lit, shown};
use crate::{
    commands::presentation,
    config::CliConfig,
    display_manager::{DisplayManager, INDENT},
    event_processor::{EventProcessor, Exit},
    formatters::format_key,
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
    let names =
        |sources: Vec<&String>| Shown::join(sources.into_iter().map(|s| Name::new(s)), lit!(", "));
    match &audit_result.result {
        AuditResult::Clean { sources } => {
            display.print_success(format!(
                "{name} — clean ({})",
                names(sources.iter().collect())
            ));
        }
        AuditResult::Conflicts {
            sources, expected, ..
        } => {
            display.print_warning(format!(
                "{name} — conflicts detected: unexpected source(s): {}",
                names(unexpected(sources, expected))
            ));
        }
        AuditResult::NotInstalled => {
            display.print_info(format!("{name} — not installed"));
        }
        AuditResult::NoAuditCommand => {
            display.print_info(format!("{name} — no audit command defined"));
        }
        AuditResult::Error(err) => {
            display.print_error(format!("{name} — error: {}", Output::new(err)));
        }
        AuditResult::CouldNotRun(failure) => {
            display.print_error(format!("{name} — error: {failure}"));
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
            Shown::indented(Output::new(err), lit!("    "))
        )
    };

    let status = match &audit_result.result {
        AuditResult::Clean { sources } => {
            shown!(
                "{} ({})",
                label(lit!("clean"), |s| s.green()),
                Shown::join(sources.iter().map(|name| Name::new(name)), lit!(", "))
            )
        }
        AuditResult::Conflicts {
            sources, expected, ..
        } => shown!(
            "{} — unexpected: {}",
            label(lit!("CONFLICTS"), |s| s.red().bold()),
            Shown::join(
                unexpected(sources, expected)
                    .into_iter()
                    .map(|name| Name::new(name)),
                lit!(", ")
            )
        ),
        AuditResult::NotInstalled => label(lit!("not installed"), |s| s.dim()),
        AuditResult::NoAuditCommand => label(lit!("no audit command"), |s| s.dim()),
        AuditResult::Error(err) => error(err),
        AuditResult::CouldNotRun(failure) => error(&failure.to_string()),
    };

    display.println(shown!(
        "  {}: {}",
        Name::new(&audit_result.package_name),
        status
    ));
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

    // Common fields via ResultCard
    display
        .result_card("Audit Results")
        .field(lit!("Package"), &audit_result.package_name)
        .field(lit!("Environment"), &audit_result.environment)
        .command_if(lit!("Command"), audit_result.audit_command.as_deref())
        .print();

    // Status line stays inline — variant-dependent sub-fields
    let key = |field: Literal| shown!("{}{}: ", INDENT, format_key(field, use_colors));
    let error = |err: &str| {
        shown!(
            "{}{}\n{}{}",
            key(lit!("Status")),
            status_style::format_status_error(use_colors),
            key(lit!("Details")),
            Shown::indented(Output::new(err), lit!("    "))
        )
    };
    let status_line = match &audit_result.result {
        AuditResult::Clean { sources } => shown!(
            "{}{}\n{}{}",
            key(lit!("Status")),
            status_style::format_audit_clean(use_colors),
            key(lit!("Sources")),
            Shown::join(sources.iter().map(|name| Name::new(name)), lit!(", "))
        ),
        AuditResult::Conflicts {
            sources, expected, ..
        } => shown!(
            "{}{}\n{}{}\n{}{}\n{}{}",
            key(lit!("Status")),
            status_style::format_audit_conflicts(use_colors),
            key(lit!("All sources")),
            Shown::join(sources.iter().map(|name| Name::new(name)), lit!(", ")),
            key(lit!("Expected")),
            Shown::join(expected.iter().map(|name| Name::new(name)), lit!(", ")),
            key(lit!("Unexpected")),
            Shown::join(
                unexpected(sources, expected)
                    .into_iter()
                    .map(|name| Name::new(name)),
                lit!(", ")
            )
        ),
        AuditResult::NotInstalled => shown!(
            "{}{}",
            key(lit!("Status")),
            status_style::format_not_installed(use_colors)
        ),
        AuditResult::NoAuditCommand => {
            shown!(
                "{}{}",
                key(lit!("Status")),
                status_style::format_no_audit(use_colors)
            )
        }
        AuditResult::Error(err) => error(err),
        AuditResult::CouldNotRun(failure) => error(&failure.to_string()),
    };

    display.println(status_line);
}

#[cfg(test)]
mod tests {
    use super::*;
    use selfie::package::event::CheckFailure;

    // Every view of an audit shows the package's name and its sources as names,
    // so an invisible character in one shows as an escape in each.
    #[test]
    fn each_audit_view_shows_names_and_sources_escaped() {
        let audit = |result| AuditResultData {
            package_name: "p\u{200B}".to_string(),
            environment: "test".to_string(),
            audit_command: None,
            result,
        };
        let config = CliConfig::wrap_for_test(test_common::test_config_with_dir(
            std::path::Path::new("/packages"),
        ));
        for result in [
            AuditResult::Clean {
                sources: vec!["brew\u{200B}".to_string()],
            },
            AuditResult::Conflicts {
                sources: vec!["brew\u{200B}".to_string(), "npm".to_string()],
                expected: vec!["npm".to_string()],
            },
        ] {
            let audit = audit(result);
            let display = DisplayManager::new(false);
            display_audit_output_only(&audit, &display);
            display_audit_summary_line(&audit, &config, &display);
            let printed = display.printed();

            assert_eq!(printed.len(), 2, "{printed:?}");
            for (_, line) in &printed {
                assert!(line.contains("p\\u{200b}"), "{line:?}");
                assert!(line.contains("brew\\u{200b}"), "{line:?}");
                assert!(!line.contains('\u{200B}'), "{line:?}");
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
}
