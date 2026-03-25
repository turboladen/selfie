use selfie::package::{
    event::{AuditResult, AuditResultData, OperationFailure, OperationResult, PackageEvent},
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

pub(crate) async fn handle_audit(
    service: &impl PackageService,
    package_name: &str,
    config: &CliConfig,
    display: &DisplayManager,
) -> i32 {
    tracing::debug!("Running audit command for package: {}", package_name);

    display.print_progress(format!("Auditing {package_name}..."));

    let event_stream = service.audit(package_name).await;

    let mut env_error_handled = false;

    let processor = EventProcessor::new(display.clone());
    let result = processor
        .process_events(event_stream, |event| match event {
            PackageEvent::AuditResultCompleted { audit_result, .. } => {
                if config.verbose() {
                    display_audit_result_card(audit_result, config, display);
                } else {
                    display_audit_output_only(audit_result, display);
                }
                true
            }
            PackageEvent::Progress { .. } => true,
            PackageEvent::Completed { result, .. } => match result {
                OperationResult::Success(_) => true,
                OperationResult::Failure(failure) if failure.is_environment_error() => {
                    display_environment_error(package_name, failure, config, display);
                    env_error_handled = true;
                    true
                }
                _ => false,
            },
            _ => false,
        })
        .await;

    if env_error_handled {
        1
    } else {
        result.exit_code
    }
}

pub(crate) async fn handle_audit_all(
    service: &impl PackageService,
    config: &CliConfig,
    display: &DisplayManager,
) -> i32 {
    tracing::debug!("Running audit command for all packages");

    display.print_progress("Auditing all packages...");

    let event_stream = service.audit_all().await;

    let processor = EventProcessor::new(display.clone());
    let result = processor
        .process_events(event_stream, |event| match event {
            PackageEvent::AuditResultCompleted { audit_result, .. } => {
                display_audit_summary_line(audit_result, config, display);
                true
            }
            PackageEvent::Progress { .. } => true,
            PackageEvent::Completed {
                result: OperationResult::Success(_),
                ..
            } => true,
            _ => false,
        })
        .await;

    result.exit_code
}

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
            "audit",
        );
    } else {
        common::display_generic_environment_suggestion(
            package_name,
            config.environment(),
            config,
            display,
            "audit",
        );
    }
}

fn display_audit_output_only(audit_result: &AuditResultData, display: &DisplayManager) {
    match &audit_result.result {
        AuditResult::Clean { sources } => {
            display.print_success(format!(
                "{} — clean ({})",
                audit_result.package_name,
                sources.join(", ")
            ));
        }
        AuditResult::Conflicts {
            sources, expected, ..
        } => {
            let unexpected: Vec<&String> = sources
                .iter()
                .filter(|s| !expected.iter().any(|e| s.eq_ignore_ascii_case(e)))
                .collect();
            display.print_warning(format!(
                "{} — conflicts detected: unexpected source(s): {}",
                audit_result.package_name,
                unexpected
                    .iter()
                    .map(|s| s.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        AuditResult::NotInstalled => {
            display.print_info(format!("{} — not installed", audit_result.package_name));
        }
        AuditResult::NoAuditCommand => {
            display.print_info(format!(
                "{} — no audit command defined",
                audit_result.package_name
            ));
        }
        AuditResult::Error(err) => {
            display.print_error(format!("{} — error: {}", audit_result.package_name, err));
        }
    }
}

fn display_audit_summary_line(
    audit_result: &AuditResultData,
    config: &CliConfig,
    display: &DisplayManager,
) {
    let use_colors = config.use_colors();
    let name = &audit_result.package_name;

    let status = match &audit_result.result {
        AuditResult::Clean { sources } => {
            let label = if use_colors {
                console::style("clean").green().to_string()
            } else {
                "clean".to_string()
            };
            format!("{label} ({})", sources.join(", "))
        }
        AuditResult::Conflicts {
            sources, expected, ..
        } => {
            let unexpected: Vec<&String> = sources
                .iter()
                .filter(|s| !expected.iter().any(|e| s.eq_ignore_ascii_case(e)))
                .collect();
            let label = if use_colors {
                console::style("CONFLICTS").red().bold().to_string()
            } else {
                "CONFLICTS".to_string()
            };
            format!(
                "{label} — unexpected: {}",
                unexpected
                    .iter()
                    .map(|s| s.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        }
        AuditResult::NotInstalled => {
            if use_colors {
                console::style("not installed").dim().to_string()
            } else {
                "not installed".to_string()
            }
        }
        AuditResult::NoAuditCommand => {
            if use_colors {
                console::style("no audit command").dim().to_string()
            } else {
                "no audit command".to_string()
            }
        }
        AuditResult::Error(err) => {
            let label = if use_colors {
                console::style("error").red().to_string()
            } else {
                "error".to_string()
            };
            format!("{label}: {err}")
        }
    };

    display.println(format!("  {name}: {status}"));
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
        .field("Package", &audit_result.package_name)
        .field("Environment", &audit_result.environment)
        .field_if("Command", audit_result.audit_command.as_deref())
        .print();

    // Status line stays inline — variant-dependent sub-fields
    let format_key_fn =
        |field: &str| -> String { format!("{}{}: ", INDENT, format_key(field, use_colors)) };

    let status_line = match &audit_result.result {
        AuditResult::Clean { sources } => {
            format!(
                "{}{}\n{}{}",
                format_key_fn("Status"),
                status_style::format_audit_clean(use_colors),
                format_key_fn("Sources"),
                sources.join(", ")
            )
        }
        AuditResult::Conflicts {
            sources, expected, ..
        } => {
            let unexpected: Vec<&String> = sources
                .iter()
                .filter(|s| !expected.iter().any(|e| s.eq_ignore_ascii_case(e)))
                .collect();
            format!(
                "{}{}\n{}{}\n{}{}\n{}{}",
                format_key_fn("Status"),
                status_style::format_audit_conflicts(use_colors),
                format_key_fn("All sources"),
                sources.join(", "),
                format_key_fn("Expected"),
                expected.join(", "),
                format_key_fn("Unexpected"),
                unexpected
                    .iter()
                    .map(|s| s.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        }
        AuditResult::NotInstalled => {
            format!(
                "{}{}",
                format_key_fn("Status"),
                status_style::format_not_installed(use_colors)
            )
        }
        AuditResult::NoAuditCommand => {
            format!(
                "{}{}",
                format_key_fn("Status"),
                status_style::format_no_audit(use_colors)
            )
        }
        AuditResult::Error(err) => {
            format!(
                "{}{}\n{}{}",
                format_key_fn("Status"),
                status_style::format_status_error(use_colors),
                format_key_fn("Details"),
                err
            )
        }
    };

    display.println(status_line);
}
