//! Handler for `selfie sync push`.

use crate::formatters::plural;
use crate::received;
use crate::terminal_text::{DisplayPath, Name, lit, shown};
use tokio_util::sync::CancellationToken;

use selfie::{
    package::event::{OperationResult, OperationSuccess, PackageEvent},
    sync_service::{ConfirmedCommit, PrepareResult, PushOptions, SyncError, SyncService},
};

use crate::display_manager::Channel;
use crate::{
    commands::{
        common::create_sync_service,
        validation_display::{ValidationGroup, display_validation_groups},
    },
    config::CliConfig,
    display_manager::{Blank, DisplayManager, PromptFailure, Suggestion},
    event_processor::{EventProcessor, Exit},
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
) -> Exit {
    let service = create_sync_service(config, cancellation_token);
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
            let label = plural(count, lit!("package"), lit!("packages"));
            display.print_error(shown!(
                "{} changed {} failed validation — fix before pushing",
                count,
                label
            ));

            let groups: Vec<ValidationGroup> = failures
                .iter()
                .map(|f| ValidationGroup {
                    label: DisplayPath::new(&f.path).into(),
                    rows: f.issues.iter().map(crate::received::Issue::from).collect(),
                })
                .collect();

            display_validation_groups(groups, use_colors, display, Channel::Stderr);
            return Exit::Failed;
        }
        // Its own arm so the suggestion lands in the suggestion channel, as it
        // does for the same refusal on the apply path. The `Display` impl joins
        // both halves for callers with nowhere separate to put one.
        Err(SyncError::Privilege(refusal)) => {
            display.print_error(received::sentence(refusal.message()));
            display.print_suggestion(received::sentence(refusal.suggestion()));
            return Exit::Failed;
        }
        Err(e) => {
            display.print_error(received::sentence(e));
            return Exit::Failed;
        }
    };

    for warning in &warnings {
        display.print_warning(received::sentence(warning));
    }

    if pending_commits.is_empty() && ahead == 0 {
        display.print_info(lit!("Nothing to push — working tree is clean"));
        return Exit::Clean;
    }

    // No new commits but unpushed local commits exist — push them directly
    if pending_commits.is_empty() && ahead > 0 {
        let label = plural(ahead, lit!("commit"), lit!("commits"));
        display.print_info(shown!(
            "{} existing {} not yet pushed — pushing now",
            ahead,
            label
        ));
        return execute_push(&service, vec![], display).await;
    }

    // Refused before the preview, which is printed only to be confirmed: a run
    // that cannot ask would leave the list on stdout as if it were an answer.
    if !args.yes && !display.can_prompt() {
        return display.refuse_prompt(
            &PromptFailure::NoTerminal,
            lit!("Editing the commit messages"),
            lit!("Pass --yes to commit with the generated messages."),
        );
    }

    // Phase 1.5: Preview and confirm commit messages
    show_pending_commits(&pending_commits, display, use_colors);

    let confirmed_commits = match confirm_commits(pending_commits, args.yes, display) {
        Ok(commits) => commits,
        Err(failure) => {
            return display.refuse_prompt(
                &failure,
                lit!("Editing the commit messages"),
                lit!("Pass --yes to commit with the messages shown above."),
            );
        }
    };

    // Phase 2: Execute commits and push
    execute_push(&service, confirmed_commits, display).await
}

/// Display the list of pending commits as a preview before confirmation.
fn show_pending_commits(
    commits: &[selfie::sync_service::PendingCommit],
    display: &DisplayManager,
    use_colors: bool,
) {
    let count = commits.len();
    let label = plural(count, lit!("commit"), lit!("commits"));
    display.println(lit!(""));
    display.print_info(shown!("{} {} to push:", count, label));
    display.println(lit!(""));
    for (i, commit) in commits.iter().enumerate() {
        let num = i + 1;
        let file_count = commit.files.len();
        let file_label = if file_count == 1 {
            lit!("file")
        } else {
            lit!("files")
        };
        // The message takes a name's escape, as the note under the prompt
        // does, so the two read alike and what Enter keeps is what was
        // previewed.
        let message =
            crate::status_style::styled_text(commit_message(&commit.message), use_colors, |s| {
                s.bold()
            });
        display.println_after(
            shown!("  {}. ", num),
            shown!("{} ({} {})", message, file_count, file_label),
        );
    }
    display.println(lit!(""));
}

/// The message a commit gets from its prompt's `answer`: the answer, or the
/// suggested message when nothing in the answer shows.
fn chosen_message(answer: String, suggested: String) -> String {
    if Name::is_blank(&answer) {
        suggested
    } else {
        answer
    }
}

/// `message` on one line, as a listed or reported commit shows it: a name's
/// escape, without a trailing line break.
fn commit_message(message: &str) -> Name {
    Name::new(message.trim_end_matches(['\r', '\n']))
}

/// Prompt the user to confirm or edit each commit message.
///
/// # Errors
///
/// [`PromptFailure`] when a prompt got no answer.
fn confirm_commits(
    pending: Vec<selfie::sync_service::PendingCommit>,
    auto_accept: bool,
    display: &DisplayManager,
) -> Result<Vec<ConfirmedCommit>, PromptFailure> {
    if auto_accept {
        return Ok(pending
            .into_iter()
            .map(|c| ConfirmedCommit {
                files: c.files,
                message: c.message,
            })
            .collect());
    }

    let total = pending.len();
    let mut confirmed = Vec::new();
    for (i, commit) in pending.into_iter().enumerate() {
        // A blank answer keeps the suggested message's own bytes, as `--yes`
        // does.
        let answer = display.input(
            shown!("Commit message ({}/{})", i + 1, total),
            Suggestion::Keepable(commit.message.clone()),
            Blank::Allowed,
        )?;

        confirmed.push(ConfirmedCommit {
            files: commit.files,
            message: chosen_message(answer, commit.message),
        });
    }
    Ok(confirmed)
}

/// Execute the push via the service and process the resulting event stream.
async fn execute_push(
    service: &impl SyncService,
    commits: Vec<ConfirmedCommit>,
    display: &DisplayManager,
) -> Exit {
    let event_stream = service.execute_push(commits).await;
    let display_for_handler = display.clone();
    let processor = EventProcessor::new(display.clone());
    processor
        .process_events(event_stream, move |event| {
            handle_push_event(event, &display_for_handler)
        })
        .await
        .exit
}

fn handle_push_event(event: &PackageEvent, display: &DisplayManager) -> bool {
    match event {
        PackageEvent::SyncCommitCreated {
            package_name,
            message,
            ..
        } => {
            // The line names the commit and escapes its message as the preview
            // did, so the two lines of one run read alike.
            display.print_success(shown!(
                "{}: {}",
                Name::new(package_name),
                commit_message(message)
            ));
            true
        }

        PackageEvent::Completed {
            result:
                OperationResult::Success(OperationSuccess::SyncPushComplete { commits_pushed, .. }),
            ..
        } => {
            display.println(lit!(""));
            if *commits_pushed == 0 {
                display.print_success(lit!("Pushed to remote"));
            } else {
                let label = plural(*commits_pushed, lit!("commit"), lit!("commits"));
                display.print_success(shown!("Pushed {} {} to remote", *commits_pushed, label));
            }
            true
        }

        PackageEvent::Completed {
            result: OperationResult::Success(OperationSuccess::SyncNothingToPush { .. }),
            ..
        } => {
            display.print_info(lit!("Nothing to push"));
            true
        }

        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use selfie::package::event::{OperationContext, OperationInfo, OperationType, StepCount};
    use selfie::sync_service::PendingCommit;
    use std::path::PathBuf;

    // A message the prompt could only draw escaped is shown escaped above an
    // empty prompt, and pressing Enter commits its own bytes, as --yes does.
    #[test]
    fn accepting_a_message_that_needs_escaping_commits_its_own_bytes() {
        use crate::display_manager::answer;
        let message = "feat(x\u{FE0F}\u{1b}]0;pwn\u{7}): add".to_string();
        let pending = vec![selfie::sync_service::PendingCommit {
            name: "x".to_string(),
            message: message.clone(),
            files: Vec::new(),
        }];
        let display = DisplayManager::new(false).answering(vec![answer(String::new())]);

        let confirmed = confirm_commits(pending, false, &display).unwrap();

        assert_eq!(confirmed[0].message, message);
        let printed = display.printed();
        assert!(
            printed
                .iter()
                .any(|(_, line)| line.contains("x\\u{fe0f}\\u{1b}]0;pwn\\u{7}")),
            "{printed:?}"
        );
    }

    // A message holding a blank other than a space is not pre-filled, since
    // the prompt would draw it as a space: the note above the empty prompt
    // shows it escaped, and Enter keeps its own bytes.
    #[test]
    fn a_commit_message_with_a_visible_blank_is_not_prefilled() {
        use crate::display_manager::answer;
        let message = "feat(x): add\u{A0}y".to_string();
        let pending = vec![selfie::sync_service::PendingCommit {
            name: "x".to_string(),
            message: message.clone(),
            files: Vec::new(),
        }];
        let display = DisplayManager::new(false).answering(vec![answer(String::new())]);

        let confirmed = confirm_commits(pending, false, &display).unwrap();

        assert_eq!(confirmed[0].message, message);
        assert!(
            display
                .printed()
                .iter()
                .any(|(_, line)| line == r"  Suggested: feat(x): add\u{a0}y (Enter keeps it)"),
            "{:?}",
            display.printed()
        );
    }

    // An answer the user typed is committed as typed.
    #[test]
    fn a_typed_message_is_committed_as_typed() {
        assert_eq!(
            chosen_message("fix: mine".into(), "chore: x".into()),
            "fix: mine"
        );
        assert_eq!(chosen_message("  ".into(), "chore: x".into()), "chore: x");
    }

    // An answer in which nothing shows, such as a Hangul filler or a zero-width
    // space, keeps the suggested message rather than committing an invisible one.
    #[test]
    fn an_invisible_answer_keeps_the_suggested_message() {
        for invisible in ["\u{3164}", "\u{200b}", " \u{ffa0} "] {
            assert_eq!(
                chosen_message(invisible.into(), "chore: x".into()),
                "chore: x",
                "{invisible:?}"
            );
        }
    }

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

        assert!(handle_push_event(&event, &display));
    }

    // The line for a commit made shows its message as the preview did: on one
    // line, an invisible character and a line break written as escapes.
    #[test]
    fn a_created_commit_shows_its_message_as_a_name() {
        let display = DisplayManager::new(false);
        let event = PackageEvent::SyncCommitCreated {
            operation_info: make_operation_info(),
            package_name: "starship".to_string(),
            message: "abc1234 feat(starship): add\u{200B}\nthe body\n".to_string(),
        };

        assert!(handle_push_event(&event, &display));

        assert_eq!(
            display.printed(),
            [(
                crate::display_manager::Channel::Stdout,
                r"starship: abc1234 feat(starship): add\u{200b}\nthe body".to_string()
            )]
        );
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

        assert!(handle_push_event(&event, &display));
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

        assert!(handle_push_event(&event, &display));
    }

    // The header is the shared handler's, shown only under `--verbose`.
    #[test]
    fn leaves_started_to_the_shared_handler() {
        let display = DisplayManager::new(false);
        let started = PackageEvent::Started {
            operation_info: make_operation_info(),
        };
        assert!(!handle_push_event(&started, &display));
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

    // A commit message is previewed as the note under its prompt shows it: on
    // one line, an invisible character or a line break in it written as an
    // escape.
    #[test]
    fn a_push_preview_shows_the_message_as_a_name() {
        let display = DisplayManager::new(false);
        let commits = vec![make_pending_commit(
            "starship",
            "feat(starship): add\u{200B}\nthe body",
            &["starship.yml"],
        )];

        show_pending_commits(&commits, &display, true);

        let line = display
            .written()
            .into_iter()
            .find(|line| line.contains("feat"))
            .expect("the commit was listed");
        assert_eq!(line, r"  1. feat(starship): add\u{200b}\nthe body (1 file)");
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

    // Ctrl+C at a commit message ends the push canceled, with nothing committed.
    #[test]
    fn ctrl_c_at_a_commit_message_is_canceled() {
        let display = DisplayManager::new(false).answering(vec![crate::display_manager::ctrl_c()]);
        let pending = vec![selfie::sync_service::PendingCommit {
            name: "fd".to_string(),
            files: vec![PathBuf::from("fd.yml")],
            message: "feat(fd): add spec".to_string(),
        }];

        let failure = confirm_commits(pending, false, &display).unwrap_err();

        assert_eq!(failure.exit(), crate::event_processor::Exit::Canceled);
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
        let exit = execute_push(&mock_service, vec![], &display).await;
        assert_eq!(exit, Exit::Clean);
    }
}
