//! Handler for `selfie sync status`.

use crate::terminal_text::{Name, Shown, lit, shown};
use tokio_util::sync::CancellationToken;

use selfie::{
    package::event::{OperationResult, OperationSuccess, Outcome, PackageEvent},
    sync_service::SyncService,
};

use crate::{
    commands::common::create_sync_service,
    config::CliConfig,
    display_manager::{DisplayManager, INDENT, shorten_path},
    event_processor::{EventProcessor, Exit},
};

pub(crate) async fn handle_status(
    config: &CliConfig,
    display: &DisplayManager,
    cancellation_token: CancellationToken,
) -> Exit {
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

    result.exit
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
            let short_root = shorten_path(&repo_root.to_string_lossy());
            let branch_str = branch.as_deref().map_or_else(
                || lit!("(detached)").into(),
                |branch| Shown::from(Name::new(branch)),
            );

            let total_changes = modified_count + staged_count + untracked_count + deleted_count;
            let is_clean = total_changes == 0;

            if is_clean && *ahead == 0 && *behind == 0 {
                display.print_success(shown!("Repository: {} ({})", short_root, branch_str));
                display.println(shown!(
                    "{}No uncommitted changes, up to date with remote",
                    INDENT
                ));
            } else {
                if is_clean {
                    display.print_success(shown!("Repository: {} ({})", short_root, branch_str));
                } else {
                    display.print_info(shown!("Repository: {} ({})", short_root, branch_str));
                }

                // File changes summary
                if total_changes > 0 {
                    let parts = [
                        (*modified_count, lit!("modified")),
                        (*staged_count, lit!("staged")),
                        (*untracked_count, lit!("untracked")),
                        (*deleted_count, lit!("deleted")),
                    ]
                    .into_iter()
                    .filter(|(count, _)| *count > 0)
                    .map(|(count, word)| shown!("{} {}", count, word));
                    display.println(shown!(
                        "{}{}",
                        INDENT,
                        crate::formatters::comma_joined(parts)
                    ));
                }

                // Remote tracking
                if *ahead > 0 && *behind > 0 {
                    display.println(shown!(
                        "{}{} ahead, {} behind remote",
                        INDENT,
                        *ahead,
                        *behind
                    ));
                } else if *ahead > 0 {
                    let label = if *ahead == 1 {
                        lit!("commit")
                    } else {
                        lit!("commits")
                    };
                    display.println(shown!("{}{} {} ahead of remote", INDENT, *ahead, label));
                } else if *behind > 0 {
                    let label = if *behind == 1 {
                        lit!("commit")
                    } else {
                        lit!("commits")
                    };
                    display.println(shown!("{}{} {} behind remote", INDENT, *behind, label));
                } else {
                    display.println(shown!("{}Up to date with remote", INDENT));
                }
            }
            true
        }

        PackageEvent::SyncDriftSummary {
            operation_info,
            drifted_targets,
            total_deployed,
            refused_count,
            warned,
            unverified_count: unverified,
            orphan_count,
            unjudged_count,
            drift_outcome,
            ..
        } => {
            display.println(lit!(""));
            // Something drift could not check is neither drifted nor clean, and
            // saying "no drift" for it answers a question nobody asked. The
            // count is reported first because it bounds what the rest of the
            // line is worth: the deployed total covers only what was examined.
            // It counts specs that could not be loaded, packages and unlistable
            // directories alike, so the line names none of them.
            if *refused_count > 0 {
                display.print_warning(shown!(
                    "{} refusal(s) left dotfiles unchecked -- see the warnings above",
                    refused_count
                ));
            }
            // The drift line is part of this command's answer, so it is on
            // stdout, marked by what it found.
            if drifted_targets.is_empty() {
                let (line, clean) =
                    no_drift_line(*total_deployed, *drift_outcome, *warned, *unverified);
                let outcome = if clean {
                    Outcome::Clean
                } else {
                    Outcome::Found
                };
                // Drift depends on the environment, so the line names it.
                display.print_result(
                    outcome,
                    shown!(
                        "{} in environment '{}'",
                        line,
                        crate::received::Operation::from(operation_info).environment
                    ),
                );
            } else {
                let count = drifted_targets.len();
                display.print_result(
                    Outcome::Found,
                    shown!(
                        "Dotfile drift in environment '{}': {} drifted out of {}",
                        crate::received::Operation::from(operation_info).environment,
                        count,
                        deployed_phrase(*total_deployed, *unverified)
                    ),
                );
                // Show drifted file paths (shortened)
                for target in drifted_targets {
                    let short = Shown::from(shorten_path(target)).paint(use_colors, |s| s.yellow());
                    display.println(shown!("{}{}", INDENT, short));
                }
                display.print_suggestion(lit!(
                    "Run 'selfie apply' to redeploy or 'selfie dotfiles drift' for details"
                ));
            }
            // Their own line: an orphan is neither drift nor a gap in the check.
            if *orphan_count > 0 {
                display.print_warning(shown!(
                    "{} orphaned target(s): no entry deploys to them now -- run \
                     'selfie dotfiles drift' for details",
                    orphan_count
                ));
            }
            if *unjudged_count > 0 {
                display.print_warning(shown!(
                    "{} deployed target(s) not checked for orphans -- see the \
                     warnings above",
                    unjudged_count
                ));
            }
            true
        }

        // Suppress the generic "Sync status complete" completion message
        PackageEvent::Completed {
            result: OperationResult::Success(OperationSuccess::Generic(_)),
            ..
        } => true,

        _ => false,
    }
}

// The wording and the level for a run that found no drift.
//
// The check mark may not appear over a run that skipped something: a failed
// check, such as one that refused a spec it could not load, or another relayed
// warning keeps the line a warning that names what it covers. An orphan leaves
// the line clean, since it is reported on a line of its own, and so does an
// unverifiable secret-bearing entry, which is unverifiable by design.
//
// Split out because it is the only part of this renderer a test can see:
// `DisplayManager` writes through a `MultiProgress`, which a test cannot capture.
fn no_drift_line(
    total_deployed: usize,
    drift_outcome: Outcome,
    warned: usize,
    unverified: usize,
) -> (Shown, bool) {
    let deployed = deployed_phrase(total_deployed, unverified);
    if drift_outcome == Outcome::Failed || warned > 0 {
        (
            shown!("No drift among what could be checked ({})", deployed),
            false,
        )
    } else {
        (shown!("No dotfile drift ({})", deployed), true)
    }
}

// The deployed total, and beside it the entries drift could not verify, which
// the total leaves out.
fn deployed_phrase(total_deployed: usize, unverified: usize) -> Shown {
    if unverified > 0 {
        shown!("{} deployed, {} not verifiable", total_deployed, unverified)
    } else {
        shown!("{} deployed", total_deployed)
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
        let (line, clean) = super::no_drift_line(5, Outcome::Clean, 0, 0);
        assert!(clean, "nothing was skipped, so the check mark is earned");
        assert!(line.contains("No dotfile drift"), "got: {line}");
    }

    #[test]
    fn a_run_that_skipped_a_package_may_not_report_success() {
        let (line, clean) = super::no_drift_line(5, Outcome::Failed, 0, 0);
        assert!(!clean, "a skipped package must not be reported as clean");
        assert!(
            line.contains("could be checked"),
            "the line must name what it covers: {line}"
        );
    }

    // A warning alone, with zero refusals, has to move the gate by itself -- a case
    // that also set a refusal would pass even if `warned` were never read.
    #[test]
    fn a_relayed_warning_alone_may_not_report_success() {
        let (line, clean) = super::no_drift_line(5, Outcome::Clean, 1, 0);
        assert!(!clean, "a relayed warning must not be reported as clean");
        assert!(
            !line.contains("No dotfile drift"),
            "the line must not claim no drift: {line}"
        );
    }

    // Orphans alone: the check was complete, so the line keeps its check mark,
    // and the orphans get a line of their own rather than a claim that the check
    // covered less than it did.
    #[test]
    fn an_orphan_alone_leaves_the_line_clean() {
        let (line, clean) = super::no_drift_line(5, Outcome::Found, 0, 0);
        assert!(clean, "an orphan must not read as a partial check");
        assert_eq!(line, "No dotfile drift (5 deployed)");
    }

    // Entries drift could not verify, and nothing else: a machine whose dotfiles
    // all come from providers. They are unverifiable by design, so the line stays
    // clean and names them.
    #[test]
    fn an_unverified_entry_is_counted_on_a_clean_line() {
        let (line, clean) = super::no_drift_line(1, Outcome::Clean, 0, 2);
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
            warned: 0,
            unverified_count: 0,
            orphan_count: 0,
            unjudged_count: 0,
            drift_outcome: Outcome::Clean,
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
            warned: 0,
            unverified_count: 0,
            orphan_count: 0,
            unjudged_count: 0,
            drift_outcome: Outcome::Clean,
        };

        assert!(handle_status_event(&event, &display, false));
    }

    #[test]
    fn handles_drift_summary_with_an_unloaded_spec() {
        // A spec drift could not load reaches status as one refusal, and the line
        // under it must not read as clean.
        let display = DisplayManager::new(false);
        let event = PackageEvent::SyncDriftSummary {
            operation_info: make_operation_info(),
            drifted_targets: vec![],
            total_deployed: 5,
            refused_count: 1,
            warned: 0,
            unverified_count: 0,
            orphan_count: 0,
            unjudged_count: 0,
            drift_outcome: Outcome::Failed,
        };

        // What it renders is asserted against the binary's own output, since
        // `DisplayManager` cannot be captured: see
        // `sync_status_does_not_report_success_over_a_spec_it_could_not_load`.
        assert!(handle_status_event(&event, &display, false));
    }

    // The header is the shared handler's, shown only under `--verbose`.
    #[test]
    fn leaves_started_to_the_shared_handler() {
        let display = DisplayManager::new(false);
        let started = PackageEvent::Started {
            operation_info: make_operation_info(),
        };
        assert!(!handle_status_event(&started, &display, false));
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
