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
    let service = create_sync_service(config, display, cancellation_token);

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
            ..
        } => {
            display.println("");
            if drifted_targets.is_empty() {
                display.print_success(format!("No dotfile drift ({total_deployed} deployed)"));
            } else {
                let count = drifted_targets.len();
                display.print_warning(format!(
                    "Dotfile drift: {count} drifted out of {total_deployed} deployed"
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

    #[test]
    fn handles_drift_summary_no_drift() {
        let display = DisplayManager::new(false);
        let event = PackageEvent::SyncDriftSummary {
            operation_info: make_operation_info(),
            drifted_targets: vec![],
            total_deployed: 5,
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
