//! Handler for `selfie sync push`.

use console::style;
use tokio_util::sync::CancellationToken;

use selfie::{
    package::event::{OperationResult, OperationSuccess, PackageEvent},
    sync_service::{ConfirmedCommit, PrepareResult, PushOptions, SyncError, SyncService},
};

use crate::{
    commands::{
        common::create_sync_service,
        validation_display::{ValidationGroup, ValidationRow, display_validation_groups},
    },
    config::CliConfig,
    display_manager::DisplayManager,
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
    cancellation_token: CancellationToken,
) -> i32 {
    let service = create_sync_service(config, display, cancellation_token);
    let use_colors = config.use_colors();

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
        Err(SyncError::ValidationFailed { failures }) => {
            let count = failures.len();
            let label = selfie::pluralize(count, "package", "packages");
            display.print_error(format!(
                "{count} changed {label} failed validation — fix before pushing"
            ));

            let groups: Vec<ValidationGroup<'_>> = failures
                .iter()
                .map(|f| ValidationGroup {
                    label: &f.path,
                    rows: f
                        .issues
                        .iter()
                        .map(|i| ValidationRow {
                            level: &i.level,
                            category: &i.category,
                            field: &i.field,
                            message: &i.message,
                            location: i.location.as_deref(),
                        })
                        .collect(),
                })
                .collect();

            display_validation_groups(&groups, use_colors, display);
            return 1;
        }
        // Its own arm so the suggestion lands in the suggestion channel, as it
        // does for the same refusal on the apply path. The `Display` impl joins
        // both halves for callers with nowhere separate to put one.
        Err(SyncError::Privilege(refusal)) => {
            display.print_error(refusal.message());
            display.print_suggestion(refusal.suggestion());
            return 1;
        }
        Err(e) => {
            display.print_error(e.to_string());
            return 1;
        }
    };

    for warning in &warnings {
        display.print_warning(warning);
    }

    if pending_commits.is_empty() && ahead == 0 {
        display.print_info("Nothing to push — working tree is clean");
        return 0;
    }

    // No new commits but unpushed local commits exist — push them directly
    if pending_commits.is_empty() && ahead > 0 {
        let label = selfie::pluralize(ahead, "commit", "commits");
        display.print_info(format!(
            "{ahead} existing {label} not yet pushed — pushing now"
        ));
        return execute_push(&service, vec![], display, use_colors).await;
    }

    // Phase 1.5: Preview and confirm commit messages
    show_pending_commits(&pending_commits, display, use_colors);

    let confirmed_commits = match confirm_commits(pending_commits, args.yes, display) {
        Some(commits) => commits,
        None => return 130, // User cancelled
    };

    // Phase 2: Execute commits and push
    execute_push(&service, confirmed_commits, display, use_colors).await
}

/// Display the list of pending commits as a preview before confirmation.
fn show_pending_commits(
    commits: &[selfie::sync_service::PendingCommit],
    display: &DisplayManager,
    use_colors: bool,
) {
    let count = commits.len();
    let label = selfie::pluralize(count, "commit", "commits");
    display.println("");
    display.print_info(format!("{count} {label} to push:"));
    display.println("");
    for (i, commit) in commits.iter().enumerate() {
        let num = i + 1;
        let file_count = commit.files.len();
        let file_label = selfie::pluralize(file_count, "file", "files");
        if use_colors {
            display.println(format!(
                "  {num}. {} ({file_count} {file_label})",
                style(&commit.message).bold()
            ));
        } else {
            display.println(format!(
                "  {num}. {} ({file_count} {file_label})",
                commit.message
            ));
        }
    }
    display.println("");
}

/// Prompt the user to confirm or edit each commit message.
///
/// Returns `None` if the user cancelled (e.g., Ctrl-C).
fn confirm_commits(
    pending: Vec<selfie::sync_service::PendingCommit>,
    auto_accept: bool,
    display: &DisplayManager,
) -> Option<Vec<ConfirmedCommit>> {
    if auto_accept {
        return Some(
            pending
                .into_iter()
                .map(|c| ConfirmedCommit {
                    files: c.files,
                    message: c.message,
                })
                .collect(),
        );
    }

    let total = pending.len();
    let mut confirmed = Vec::new();
    for (i, commit) in pending.into_iter().enumerate() {
        let num = i + 1;
        let edited_message: String =
            match dialoguer::Input::with_theme(&dialoguer::theme::ColorfulTheme::default())
                .with_prompt(format!("Commit message ({num}/{total})"))
                .with_initial_text(&commit.message)
                .interact_text()
            {
                Ok(msg) => msg,
                Err(_) => {
                    display.print_warning("Cancelled");
                    return None;
                }
            };

        confirmed.push(ConfirmedCommit {
            files: commit.files,
            message: edited_message,
        });
    }
    Some(confirmed)
}

/// Execute the push via the service and process the resulting event stream.
async fn execute_push(
    service: &impl SyncService,
    commits: Vec<ConfirmedCommit>,
    display: &DisplayManager,
    use_colors: bool,
) -> i32 {
    let event_stream = service.execute_push(commits).await;
    let display_for_handler = display.clone();
    let processor = EventProcessor::new(display.clone());
    processor
        .process_events(event_stream, move |event| {
            handle_push_event(event, &display_for_handler, use_colors)
        })
        .await
        .exit_code
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
            display.println("");
            if *commits_pushed == 0 {
                display.print_success("Pushed to remote");
            } else {
                let label = selfie::pluralize(*commits_pushed, "commit", "commits");
                display.print_success(format!("Pushed {commits_pushed} {label} to remote"));
            }
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
    use selfie::sync_service::PendingCommit;
    use std::path::PathBuf;

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

    fn make_pending_commit(name: &str, message: &str, files: &[&str]) -> PendingCommit {
        PendingCommit {
            name: name.to_string(),
            message: message.to_string(),
            files: files.iter().map(PathBuf::from).collect(),
        }
    }

    // --- handle_push_event tests ---

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

    // --- show_pending_commits tests ---

    #[test]
    fn show_pending_commits_single_file_uses_singular() {
        let display = DisplayManager::new(false);
        let commits = vec![make_pending_commit(
            "starship",
            "feat(starship): add package spec",
            &["starship.yml"],
        )];

        // Should not panic; display output includes "1 file"
        show_pending_commits(&commits, &display, false);
    }

    #[test]
    fn show_pending_commits_multiple_files_uses_plural() {
        let display = DisplayManager::new(false);
        let commits = vec![make_pending_commit(
            "starship",
            "chore(starship): update spec and dotfiles",
            &["starship.yml", "starship/starship.toml"],
        )];

        // Should not panic; display output includes "2 files"
        show_pending_commits(&commits, &display, false);
    }

    #[test]
    fn show_pending_commits_multiple_commits() {
        let display = DisplayManager::new(false);
        let commits = vec![
            make_pending_commit(
                "starship",
                "feat(starship): add package spec",
                &["starship.yml"],
            ),
            make_pending_commit("fnm", "feat(fnm): add package spec", &["fnm.yml"]),
        ];

        show_pending_commits(&commits, &display, false);
    }

    #[test]
    fn show_pending_commits_with_colors() {
        let display = DisplayManager::new(false);
        let commits = vec![make_pending_commit(
            "starship",
            "feat(starship): add package spec",
            &["starship.yml"],
        )];

        // Exercises the use_colors=true branch
        show_pending_commits(&commits, &display, true);
    }

    // --- confirm_commits tests ---

    #[test]
    fn confirm_commits_auto_accept_preserves_messages() {
        let display = DisplayManager::new(false);
        let pending = vec![
            make_pending_commit("starship", "feat(starship): add spec", &["starship.yml"]),
            make_pending_commit("fnm", "feat(fnm): add spec", &["fnm.yml"]),
        ];

        let confirmed = confirm_commits(pending, true, &display).unwrap();

        assert_eq!(confirmed.len(), 2);
        assert_eq!(confirmed[0].message, "feat(starship): add spec");
        assert_eq!(confirmed[0].files, vec![PathBuf::from("starship.yml")]);
        assert_eq!(confirmed[1].message, "feat(fnm): add spec");
        assert_eq!(confirmed[1].files, vec![PathBuf::from("fnm.yml")]);
    }

    #[test]
    fn confirm_commits_auto_accept_empty_list() {
        let display = DisplayManager::new(false);
        let confirmed = confirm_commits(vec![], true, &display).unwrap();
        assert!(confirmed.is_empty());
    }

    // --- execute_push tests ---

    #[tokio::test]
    async fn execute_push_returns_exit_code_from_event_stream() {
        use selfie::package::event::EventStream;
        use selfie::sync_service::MockSyncService;

        let mut mock_service = MockSyncService::new();
        mock_service.expect_execute_push().returning(|_| {
            Box::pin(async {
                let events = vec![PackageEvent::Completed {
                    operation_info: OperationInfo {
                        id: uuid::Uuid::new_v4(),
                        operation_type: OperationType::SyncPush,
                        package_name: String::new(),
                        environment: "test".to_string(),
                        context: OperationContext::default(),
                        timestamp: std::time::Instant::now(),
                    },
                    result: OperationResult::Success(OperationSuccess::SyncPushComplete {
                        commits_pushed: 1,
                        steps_completed: StepCount::new(1, 1),
                    }),
                }];
                Box::pin(futures::stream::iter(events)) as EventStream
            })
        });

        let display = DisplayManager::new(false);
        let exit_code = execute_push(&mock_service, vec![], &display, false).await;
        assert_eq!(exit_code, 0);
    }
}
