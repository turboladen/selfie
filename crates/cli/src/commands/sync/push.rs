//! Handler for `selfie sync push`.

use console::style;

use selfie::{
    package::event::{OperationResult, OperationSuccess, PackageEvent},
    sync_service::{ConfirmedCommit, PrepareResult, PushOptions, SyncService},
};

use crate::{
    commands::common::create_sync_service, config::CliConfig, display_manager::DisplayManager,
    event_processor::EventProcessor,
};

pub(crate) struct PushArgs {
    pub batch: bool,
    pub message: Option<String>,
    pub yes: bool,
    pub include_ungrouped: bool,
}

pub(crate) async fn handle_push(
    args: &PushArgs,
    config: &CliConfig,
    display: &DisplayManager,
) -> i32 {
    let service = create_sync_service(config);

    let options = PushOptions {
        batch: args.batch,
        message: args.message.clone(),
        auto_accept: args.yes,
        include_ungrouped: args.include_ungrouped,
    };

    // Phase 1: Prepare commits (non-mutating)
    let PrepareResult {
        pending_commits,
        ahead,
        warnings,
    } = match service.prepare_push(&options).await {
        Ok(result) => result,
        Err(e) => {
            display.print_error(e.to_string());
            return 1;
        }
    };

    // Surface any warnings from the preparation phase
    for warning in &warnings {
        display.print_warning(warning);
    }

    if pending_commits.is_empty() && ahead == 0 {
        display.print_info("Nothing to push — working tree is clean");
        return 0;
    }

    let use_colors = config.use_colors();

    // If no new commits but ahead > 0, push existing commits directly
    if pending_commits.is_empty() && ahead > 0 {
        let label = if ahead == 1 { "commit" } else { "commits" };
        display.print_info(format!(
            "{ahead} existing {label} not yet pushed — pushing now"
        ));

        let event_stream = service.execute_push(vec![]).await;
        let display_for_handler = display.clone();
        let processor = EventProcessor::new(display.clone());
        let result = processor
            .process_events(event_stream, move |event| {
                handle_push_event(event, &display_for_handler, use_colors)
            })
            .await;

        return result.exit_code;
    }

    // Show what will be committed
    display.println("");
    for commit in &pending_commits {
        let file_count = commit.files.len();
        let label = if file_count == 1 { "file" } else { "files" };
        if use_colors {
            display.println(format!(
                "  {} ({file_count} {label})",
                style(&commit.message).bold()
            ));
        } else {
            display.println(format!("  {} ({file_count} {label})", commit.message));
        }
    }
    display.println("");

    // Phase 1.5: Confirm/edit commit messages
    let confirmed_commits = if args.yes {
        // Auto-accept all
        pending_commits
            .into_iter()
            .map(|c| ConfirmedCommit {
                files: c.files,
                message: c.message,
            })
            .collect()
    } else {
        // Prompt for each commit message
        let mut confirmed = Vec::new();
        for commit in pending_commits {
            let edited_message: String =
                match dialoguer::Input::with_theme(&dialoguer::theme::ColorfulTheme::default())
                    .with_prompt("Commit message")
                    .default(commit.message.clone())
                    .interact_text()
                {
                    Ok(msg) => msg,
                    Err(_) => {
                        display.print_warning("Cancelled");
                        return 130;
                    }
                };

            confirmed.push(ConfirmedCommit {
                files: commit.files,
                message: edited_message,
            });
        }
        confirmed
    };

    // Phase 2: Execute commits and push
    let event_stream = service.execute_push(confirmed_commits).await;

    let display_for_handler = display.clone();
    let processor = EventProcessor::new(display.clone());
    let result = processor
        .process_events(event_stream, move |event| {
            handle_push_event(event, &display_for_handler, use_colors)
        })
        .await;

    result.exit_code
}

fn handle_push_event(event: &PackageEvent, display: &DisplayManager, use_colors: bool) -> bool {
    match event {
        PackageEvent::SyncCommitCreated {
            package_name,
            message,
            ..
        } => {
            if use_colors {
                display.print_success(format!("{}: {}", style(package_name).bold(), message));
            } else {
                display.print_success(format!("{package_name}: {message}"));
            }
            true
        }

        PackageEvent::Completed {
            result:
                OperationResult::Success(OperationSuccess::SyncPushComplete { commits_pushed, .. }),
            ..
        } => {
            let label = if *commits_pushed == 1 {
                "commit"
            } else {
                "commits"
            };
            display.println("");
            display.print_success(format!("Pushed {commits_pushed} {label} to remote"));
            true
        }

        PackageEvent::Completed {
            result: OperationResult::Success(OperationSuccess::SyncNothingToPush { .. }),
            ..
        } => {
            display.print_info("Nothing to push");
            true
        }

        // Suppress started/progress — we show our own commit-by-commit output
        PackageEvent::Started { .. } | PackageEvent::Progress { .. } => true,

        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use selfie::package::event::{OperationContext, OperationInfo, OperationType, StepCount};

    fn make_operation_info() -> OperationInfo {
        OperationInfo {
            id: uuid::Uuid::new_v4(),
            operation_type: OperationType::SyncPush,
            package_name: String::new(),
            environment: "test".to_string(),
            context: OperationContext::default(),
            timestamp: std::time::Instant::now(),
        }
    }

    #[test]
    fn handles_commit_created() {
        let display = DisplayManager::new(false);
        let event = PackageEvent::SyncCommitCreated {
            operation_info: make_operation_info(),
            package_name: "starship".to_string(),
            message: "abc1234 feat(starship): add package spec".to_string(),
        };

        assert!(handle_push_event(&event, &display, false));
    }

    #[test]
    fn handles_push_complete() {
        let display = DisplayManager::new(false);
        let event = PackageEvent::Completed {
            operation_info: make_operation_info(),
            result: OperationResult::Success(OperationSuccess::SyncPushComplete {
                commits_pushed: 3,
                steps_completed: StepCount::new(4, 4),
            }),
        };

        assert!(handle_push_event(&event, &display, false));
    }

    #[test]
    fn handles_nothing_to_push() {
        let display = DisplayManager::new(false);
        let event = PackageEvent::Completed {
            operation_info: make_operation_info(),
            result: OperationResult::Success(OperationSuccess::SyncNothingToPush {
                steps_completed: StepCount::new(0, 0),
            }),
        };

        assert!(handle_push_event(&event, &display, false));
    }

    #[test]
    fn suppresses_started_and_progress() {
        let display = DisplayManager::new(false);
        let started = PackageEvent::Started {
            operation_info: make_operation_info(),
        };
        assert!(handle_push_event(&started, &display, false));
    }
}
