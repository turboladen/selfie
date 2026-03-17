use selfie::package::{
    event::{AuditResult, AuditResultData, OperationFailure, OperationResult, PackageEvent},
    port::PackageError,
    service::PackageService,
};

use crate::{
    commands::package::common, config::CliConfig, display_manager::DisplayManager,
    event_processor::EventProcessor, formatters::format_key,
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
                    display_audit_result_card(audit_result, config);
                } else {
                    display_audit_output_only(audit_result);
                }
                true
            }
            PackageEvent::Progress { .. } => true,
            PackageEvent::Completed { result, .. } => {
                if let OperationResult::Failure(failure) = result
                    && failure.is_environment_error()
                {
                    display_environment_error(package_name, failure, config);
                    env_error_handled = true;
                    return true;
                }
                false
            }
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
                display_audit_summary_line(audit_result, config);
                true
            }
            PackageEvent::Progress { .. } => true,
            _ => false,
        })
        .await;

    result.exit_code
}

fn display_environment_error(package_name: &str, failure: &OperationFailure, config: &CliConfig) {
    println!();

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
            "audit",
        );
    } else {
        common::display_generic_environment_suggestion(
            package_name,
            config.environment(),
            config,
            "audit",
        );
    }
}

fn display_audit_output_only(audit_result: &AuditResultData) {
    match &audit_result.result {
        AuditResult::Clean { sources } => {
            println!(
                "✅ {} — clean ({})",
                audit_result.package_name,
                sources.join(", ")
            );
        }
        AuditResult::Conflicts {
            sources, expected, ..
        } => {
            let unexpected: Vec<&String> = sources
                .iter()
                .filter(|s| !expected.iter().any(|e| s.eq_ignore_ascii_case(e)))
                .collect();
            println!(
                "⚠️  {} — conflicts detected: unexpected source(s): {}",
                audit_result.package_name,
                unexpected
                    .iter()
                    .map(|s| s.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
        AuditResult::NotInstalled => {
            println!("📦 {} — not installed", audit_result.package_name);
        }
        AuditResult::NoAuditCommand => {
            println!(
                "⏭️  {} — no audit command defined",
                audit_result.package_name
            );
        }
        AuditResult::Error(err) => {
            println!("❌ {} — error: {}", audit_result.package_name, err);
        }
    }
}

fn display_audit_summary_line(audit_result: &AuditResultData, config: &CliConfig) {
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

    println!("  {name}: {status}");
}

fn display_audit_result_card(audit_result: &AuditResultData, config: &CliConfig) {
    println!();
    println!("🔍 Audit Results:");

    let format_key_fn =
        |field: &str| -> String { format!("   {}: ", format_key(field, config.use_colors())) };

    println!("{}{}", format_key_fn("Package"), audit_result.package_name);
    println!(
        "{}{}",
        format_key_fn("Environment"),
        audit_result.environment
    );

    if let Some(cmd) = &audit_result.audit_command {
        println!("{}{}", format_key_fn("Command"), cmd);
    }

    let use_colors = config.use_colors();

    let status_line = match &audit_result.result {
        AuditResult::Clean { sources } => {
            let status = if use_colors {
                console::style("✅ Clean").green().bold().to_string()
            } else {
                "Clean".to_string()
            };
            format!(
                "{}{}\n{}{}",
                format_key_fn("Status"),
                status,
                format_key_fn("Sources"),
                sources.join(", ")
            )
        }
        AuditResult::Conflicts {
            sources, expected, ..
        } => {
            let status = if use_colors {
                console::style("⚠️  Conflicts Detected")
                    .red()
                    .bold()
                    .to_string()
            } else {
                "Conflicts Detected".to_string()
            };
            let unexpected: Vec<&String> = sources
                .iter()
                .filter(|s| !expected.iter().any(|e| s.eq_ignore_ascii_case(e)))
                .collect();
            format!(
                "{}{}\n{}{}\n{}{}\n{}{}",
                format_key_fn("Status"),
                status,
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
            let status = if use_colors {
                console::style("📦 Not Installed").yellow().to_string()
            } else {
                "Not Installed".to_string()
            };
            format!("{}{}", format_key_fn("Status"), status)
        }
        AuditResult::NoAuditCommand => {
            let status = if use_colors {
                console::style("No audit command defined").dim().to_string()
            } else {
                "No audit command defined".to_string()
            };
            format!("{}{}", format_key_fn("Status"), status)
        }
        AuditResult::Error(err) => {
            let status = if use_colors {
                console::style("❌ Error").red().bold().to_string()
            } else {
                "Error".to_string()
            };
            format!(
                "{}{}\n{}{}",
                format_key_fn("Status"),
                status,
                format_key_fn("Details"),
                err
            )
        }
    };

    println!("{status_line}");
}
