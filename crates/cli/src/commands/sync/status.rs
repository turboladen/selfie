//! Handler for `selfie sync status`.

use console::style;
use tokio_util::sync::CancellationToken;

use selfie::{
    package::event::{OperationResult, OperationSuccess, PackageEvent},
    sync_service::SyncService,
};

use crate::{
    commands::common::create_sync_service,
    config::CliConfig,
    display_manager::{DisplayManager, INDENT, shorten_path},
    event_processor::EventProcessor,
};

pub(crate) async fn handle_status(
    config: &CliConfig,
    display: &DisplayManager,
    cancellation_token: CancellationToken,
) -> i32 {
    let service = create_sync_service(config, cancellation_token);

    let event_stream = service.status().await;

    let display_for_handler = display.clone();
    let use_colors = config.use_colors();
    let processor = EventProcessor::new(display.clone());
    let result = processor
        .process_events(event_stream, move |event| {
            handle_status_event(event, &display_for_handler, use_colors)
        })
        .await;

    result.exit_code
}

fn handle_status_event(event: &PackageEvent, display: &DisplayManager, use_colors: bool) -> bool {
    match event {
        PackageEvent::SyncRepoStatus {
            repo_root,
            branch,
            modified_count,
            staged_count,
            untracked_count,
            deleted_count,
            ahead,
            behind,
            ..
        } => {
            let short_root = shorten_path(&repo_root.display().to_string());
            let branch_str = branch.as_deref().unwrap_or("(detached)");

            let total_changes = modified_count + staged_count + untracked_count + deleted_count;
            let is_clean = total_changes == 0;

            if is_clean && *ahead == 0 && *behind == 0 {
                display.print_success(format!("Repository: {short_root} ({branch_str})"));
                display.println(format!(
                    "{INDENT}No uncommitted changes, up to date with remote"
                ));
            } else {
                if is_clean {
                    display.print_success(format!("Repository: {short_root} ({branch_str})"));
                } else {
                    display.print_info(format!("Repository: {short_root} ({branch_str})"));
                }

                // File changes summary
                if total_changes > 0 {
                    let mut parts = Vec::new();
                    if *modified_count > 0 {
                        parts.push(format!("{modified_count} modified"));
                    }
                    if *staged_count > 0 {
                        parts.push(format!("{staged_count} staged"));
                    }
                    if *untracked_count > 0 {
                        parts.push(format!("{untracked_count} untracked"));
                    }
                    if *deleted_count > 0 {
                        parts.push(format!("{deleted_count} deleted"));
                    }
                    display.println(format!("{INDENT}{}", parts.join(", ")));
                }

                // Remote tracking
                if *ahead > 0 && *behind > 0 {
                    display.println(format!("{INDENT}{ahead} ahead, {behind} behind remote"));
                } else if *ahead > 0 {
                    let label = selfie::pluralize(*ahead, "commit", "commits");
                    display.println(format!("{INDENT}{ahead} {label} ahead of remote"));
                } else if *behind > 0 {
                    let label = selfie::pluralize(*behind, "commit", "commits");
                    display.println(format!("{INDENT}{behind} {label} behind remote"));
                } else {
                    display.println(format!("{INDENT}Up to date with remote"));
                }
            }
            true
        }

        PackageEvent::SyncDriftSummary {
            drifted_targets,
            total_deployed,
            refused_count,
            unloaded_specs,
            warned,
            unverified_count: unverified,
            ..
        } => {
            display.println("");
            // Something drift could not check is neither drifted nor clean, and
            // saying "no drift" for it answers a question nobody asked. The
            // count is reported first because it bounds what the rest of the
            // line is worth: the deployed total covers only what was examined.
            // It counts packages and unlistable directories alike, so the line
            // names neither.
            if *refused_count > 0 {
                display.print_warning(format!(
                    "{refused_count} refusal(s) left dotfiles unchecked -- see the warnings above"
                ));
            }
            if *unloaded_specs > 0 {
                display.print_warning(format!(
                    "{unloaded_specs} spec(s) could not be loaded, so nothing they declare was checked -- see the warnings above"
                ));
            }
            if drifted_targets.is_empty() {
                let (line, clean) = no_drift_line(
                    *total_deployed,
                    *refused_count,
                    *unloaded_specs,
                    *warned,
                    *unverified,
                );
                if clean {
                    display.print_success(line);
                } else {
                    display.print_warning(line);
                }
            } else {
                let count = drifted_targets.len();
                display.print_warning(format!(
                    "Dotfile drift: {count} drifted out of {}",
                    deployed_phrase(*total_deployed, *unverified)
                ));
                // Show drifted file paths (shortened)
                for target in drifted_targets {
                    let short = shorten_path(target);
                    let formatted = if use_colors {
                        format!("{INDENT}{}", style(&short).yellow())
                    } else {
                        format!("{INDENT}{short}")
                    };
                    display.println(formatted);
                }
                display.print_suggestion(
                    "Run 'selfie apply' to redeploy or 'selfie dotfiles drift' for details",
                );
            }
            true
        }

        // Suppress the generic "Sync status complete" completion message
        PackageEvent::Completed {
            result: OperationResult::Success(OperationSuccess::Generic(_)),
            ..
        } => true,

        // Suppress started/progress for status (it's a fast operation)
        PackageEvent::Started { .. } | PackageEvent::Progress { .. } => true,

        _ => false,
    }
}

// The wording and the level for a run that found no drift.
//
// The check mark may not appear over a run that skipped something: a refusal, an
// unloaded spec, or another relayed warning keeps the line a warning that names
// what it covers. A secret-bearing entry drift cannot verify is unverifiable by
// design, so its count is named and the line stays clean.
//
// Split out because it is the only part of this renderer a test can see:
// `DisplayManager` writes through a `MultiProgress`, whose output a test cannot
// capture.
fn no_drift_line(
    total_deployed: usize,
    refused_count: usize,
    unloaded_specs: usize,
    warned: usize,
    unverified: usize,
) -> (String, bool) {
    let deployed = deployed_phrase(total_deployed, unverified);
    if refused_count > 0 || unloaded_specs > 0 || warned > 0 {
        (
            format!("No drift among what could be checked ({deployed})"),
            false,
        )
    } else {
        (format!("No dotfile drift ({deployed})"), true)
    }
}

// The deployed total, and beside it the entries drift could not verify, which
// the total leaves out.
fn deployed_phrase(total_deployed: usize, unverified: usize) -> String {
    if unverified > 0 {
        format!("{total_deployed} deployed, {unverified} not verifiable")
    } else {
        format!("{total_deployed} deployed")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use selfie::package::event::{OperationContext, OperationInfo, OperationType};
    use std::path::PathBuf;

    fn make_operation_info() -> OperationInfo {
        OperationInfo {
            id: uuid::Uuid::new_v4(),
            operation_type: OperationType::SyncStatus,
            package_name: String::new(),
            environment: "test".to_string(),
            context: OperationContext::default(),
            timestamp: std::time::Instant::now(),
        }
    }

    #[test]
    fn handles_sync_repo_status_clean() {
        let display = DisplayManager::new(false);
        let event = PackageEvent::SyncRepoStatus {
            operation_info: make_operation_info(),
            repo_root: PathBuf::from("/tmp/repo"),
            branch: Some("main".to_string()),
            modified_count: 0,
            staged_count: 0,
            untracked_count: 0,
            deleted_count: 0,
            ahead: 0,
            behind: 0,
        };

        assert!(handle_status_event(&event, &display, false));
    }

    #[test]
    fn handles_sync_repo_status_with_changes() {
        let display = DisplayManager::new(false);
        let event = PackageEvent::SyncRepoStatus {
            operation_info: make_operation_info(),
            repo_root: PathBuf::from("/tmp/repo"),
            branch: Some("main".to_string()),
            modified_count: 3,
            staged_count: 1,
            untracked_count: 0,
            deleted_count: 0,
            ahead: 2,
            behind: 0,
        };

        assert!(handle_status_event(&event, &display, false));
    }

    // Copilot flagged the shipped version of this: a green check beside a
    // warning saying packages were skipped is the contradiction this branch
    // removes from `dotfiles drift`, reproduced one command over.
    #[test]
    fn a_run_that_skipped_nothing_may_report_success() {
        let (line, clean) = super::no_drift_line(5, 0, 0, 0, 0);
        assert!(clean, "nothing was skipped, so the check mark is earned");
        assert!(line.contains("No dotfile drift"), "got: {line}");
    }

    #[test]
    fn a_run_that_skipped_a_package_may_not_report_success() {
        let (line, clean) = super::no_drift_line(5, 2, 0, 0, 0);
        assert!(!clean, "a skipped package must not be reported as clean");
        assert!(
            line.contains("could be checked"),
            "the line must name what it covers: {line}"
        );
    }

    #[test]
    fn a_run_with_an_unloaded_spec_may_not_report_success() {
        let (line, clean) = super::no_drift_line(5, 0, 1, 0, 0);
        assert!(!clean, "an unloaded spec must not be reported as clean");
        assert!(
            !line.contains("No dotfile drift"),
            "the line must not claim no drift: {line}"
        );
    }

    #[test]
    fn a_refusal_and_an_unloaded_spec_together_stay_non_clean() {
        let (line, clean) = super::no_drift_line(5, 2, 1, 0, 0);
        assert!(
            !clean,
            "a refusal and an unloaded spec together must not be reported as clean"
        );
        assert!(
            !line.contains("No dotfile drift"),
            "the line must not claim no drift: {line}"
        );
    }

    // A warning alone, with zero refusals and zero unloaded specs, has to
    // move the gate by itself -- a case that also set a refusal would pass
    // even if `warned` were never read.
    #[test]
    fn a_relayed_warning_alone_may_not_report_success() {
        let (line, clean) = super::no_drift_line(5, 0, 0, 1, 0);
        assert!(!clean, "a relayed warning must not be reported as clean");
        assert!(
            !line.contains("No dotfile drift"),
            "the line must not claim no drift: {line}"
        );
    }

    // Entries drift could not verify, and nothing else: a machine whose dotfiles
    // all come from providers. They are unverifiable by design, so the line stays
    // clean and names them.
    #[test]
    fn an_unverified_entry_is_counted_on_a_clean_line() {
        let (line, clean) = super::no_drift_line(1, 0, 0, 0, 2);
        assert!(
            clean,
            "an unverified entry must not keep the line off the check mark"
        );
        assert_eq!(line, "No dotfile drift (1 deployed, 2 not verifiable)");
    }

    #[test]
    fn handles_drift_summary_no_drift() {
        let display = DisplayManager::new(false);
        let event = PackageEvent::SyncDriftSummary {
            operation_info: make_operation_info(),
            drifted_targets: vec![],
            total_deployed: 5,
            refused_count: 0,
            unloaded_specs: 0,
            warned: 0,
            unverified_count: 0,
        };

        assert!(handle_status_event(&event, &display, false));
    }

    #[test]
    fn handles_drift_summary_with_drift() {
        let display = DisplayManager::new(false);
        let event = PackageEvent::SyncDriftSummary {
            operation_info: make_operation_info(),
            drifted_targets: vec!["~/.config/starship.toml".to_string()],
            total_deployed: 5,
            refused_count: 0,
            unloaded_specs: 0,
            warned: 0,
            unverified_count: 0,
        };

        assert!(handle_status_event(&event, &display, false));
    }

    #[test]
    fn handles_drift_summary_with_an_unloaded_spec() {
        let display = DisplayManager::new(false);
        let event = PackageEvent::SyncDriftSummary {
            operation_info: make_operation_info(),
            drifted_targets: vec![],
            total_deployed: 5,
            refused_count: 0,
            unloaded_specs: 1,
            warned: 0,
            unverified_count: 0,
        };

        assert!(handle_status_event(&event, &display, false));
    }

    #[test]
    fn suppresses_started_and_progress() {
        let display = DisplayManager::new(false);
        let started = PackageEvent::Started {
            operation_info: make_operation_info(),
        };
        assert!(handle_status_event(&started, &display, false));
    }

    #[test]
    fn does_not_handle_unknown_events() {
        let display = DisplayManager::new(false);
        let event = PackageEvent::Warning {
            operation_info: make_operation_info(),
            message: "test".to_string(),
        };
        assert!(!handle_status_event(&event, &display, false));
    }
}
