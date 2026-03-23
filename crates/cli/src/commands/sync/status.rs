//! Handler for `selfie sync status`.

use console::style;

use selfie::{
    git::GixGitAdapter,
    package::event::{OperationResult, OperationSuccess, PackageEvent},
    sync_service::{SyncService, service::SyncServiceImpl},
};

use crate::{
    commands::common::create_dotfile_service,
    config::CliConfig,
    display_manager::{DisplayManager, INDENT, shorten_path},
    event_processor::EventProcessor,
};

pub(crate) async fn handle_status(config: &CliConfig, display: &DisplayManager) -> i32 {
    let git = GixGitAdapter;
    let dotfile_service = create_dotfile_service(config);
    let service = SyncServiceImpl::new(git, dotfile_service, config.selfie_config().clone());

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
                    let label = if *ahead == 1 { "commit" } else { "commits" };
                    display.println(format!("{INDENT}{ahead} {label} ahead of remote"));
                } else if *behind > 0 {
                    let label = if *behind == 1 { "commit" } else { "commits" };
                    display.println(format!("{INDENT}{behind} {label} behind remote"));
                } else {
                    display.println(format!("{INDENT}Up to date with remote"));
                }
            }
            true
        }

        PackageEvent::SyncDriftSummary {
            drifted_packages,
            total_deployed,
            ..
        } => {
            display.println("");
            if drifted_packages.is_empty() {
                display.print_success(format!("No dotfile drift ({total_deployed} deployed)"));
            } else {
                let count = drifted_packages.len();
                display.print_warning(format!(
                    "Dotfile drift: {count} drifted out of {total_deployed} deployed"
                ));
                // Show drifted file paths (shortened)
                for target in drifted_packages {
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
