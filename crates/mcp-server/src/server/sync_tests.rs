// The sync tools' results as an assistant receives them, with a mock sync
// service behind the tool, and the live one where the token is the question.

use std::{collections::HashMap, path::PathBuf, sync::Arc, time::Duration};

use rmcp::{
    handler::server::wrapper::Parameters,
    model::{CallToolRequestParams, CallToolResult},
};
use selfie::{
    git::GitSyncError,
    package::event::{
        EventStream, OperationContext, OperationFailure, OperationInfo, OperationResult,
        OperationType, PackageEvent,
    },
    sync_service::{
        MockSyncService, PendingCommit, PrepareResult, SyncError,
        port::{GitFailure, GitStep},
    },
};
use tokio_util::sync::CancellationToken;

use super::{
    SelfieServer, SyncPushParam,
    cancellation_tests::{connected, sh_runner},
    tests::{server_with, server_wrapping, tool_json},
};
use crate::services::{ConcreteDotfileService, ConcreteService, LiveServices, Scoped, ServiceSet};

// The live package and dotfile services, with `sync` in place of the live
// sync service for every request.
struct MockedSync {
    live: LiveServices,
    sync: Arc<MockSyncService>,
}

impl ServiceSet for MockedSync {
    type Package = ConcreteService;
    type Dotfile = ConcreteDotfileService;
    type Sync = MockSyncService;

    fn for_request(&self, ct: CancellationToken) -> Scoped<Self> {
        let live = self.live.for_request(ct);
        Scoped {
            package: live.package,
            dotfile: live.dotfile,
            sync: Arc::clone(&self.sync),
        }
    }

    fn shell_fallback(&self) -> Option<&selfie::commands::ShellFallback> {
        self.live.shell_fallback()
    }

    fn config(&self) -> &selfie::config::SelfieConfig {
        self.live.config()
    }
}

// A server over an empty package directory whose sync service is `sync`. The
// tempdir lives as long as the server is used.
fn server_mocking(sync: MockSyncService) -> (tempfile::TempDir, SelfieServer<MockedSync>) {
    let temp = tempfile::TempDir::new().unwrap();
    let sync = Arc::new(sync);
    let server = server_wrapping(
        temp.path(),
        None,
        Some(&temp.path().join("state")),
        sh_runner,
        |live| MockedSync { live, sync },
    );
    (temp, server)
}

fn completed(operation_type: OperationType, result: OperationResult) -> EventStream {
    let event = PackageEvent::Completed {
        operation_info: OperationInfo {
            id: uuid::Uuid::new_v4(),
            operation_type,
            package_name: String::new(),
            environment: "test".to_string(),
            context: OperationContext::default(),
            timestamp: std::time::Instant::now(),
        },
        result,
    };
    Box::pin(futures::stream::iter(vec![event]))
}

fn git_failed(operation_type: OperationType, step: GitStep, error: GitSyncError) -> EventStream {
    completed(
        operation_type,
        OperationResult::Failure(OperationFailure::Git(GitFailure::at(step, error))),
    )
}

fn push(messages: &[(&str, &str)]) -> Parameters<SyncPushParam> {
    Parameters(SyncPushParam {
        batch: false,
        message: None,
        messages: messages
            .iter()
            .map(|(name, message)| ((*name).to_string(), (*message).to_string()))
            .collect::<HashMap<_, _>>(),
        include_ungrouped: false,
    })
}

fn pending(name: &str) -> PendingCommit {
    PendingCommit {
        name: name.to_string(),
        message: format!("chore({name}): update"),
        files: vec![PathBuf::from(format!("packages/{name}.yml"))],
    }
}

fn is_error(result: &CallToolResult) -> bool {
    result.is_error == Some(true)
}

// A push whose preparation fails comes back as an error result in its own
// shape, which never passes through the event stream, and pushes nothing.
#[tokio::test]
async fn sync_push_reports_a_failed_preparation_with_its_git_label() {
    let mut sync = MockSyncService::new();
    sync.expect_prepare_push()
        .times(1)
        .returning(|_| Box::pin(async { Err(SyncError::GitError(GitSyncError::PushRejected)) }));
    sync.expect_execute_push().never();
    let (_temp, server) = server_mocking(sync);

    let result = server.selfie_sync_push(push(&[])).await.unwrap();

    assert!(is_error(&result), "{result:?}");
    assert_eq!(
        tool_json(&result),
        serde_json::json!({
            "status": "error",
            "message": SyncError::GitError(GitSyncError::PushRejected).to_string(),
            "kind": "push_rejected",
        })
    );
}

// A clean tree with nothing ahead pushes nothing and says so, with the
// preparation's warnings.
#[tokio::test]
async fn sync_push_with_nothing_to_push_runs_no_push() {
    let mut sync = MockSyncService::new();
    sync.expect_prepare_push().times(1).returning(|_| {
        Box::pin(async {
            Ok(PrepareResult {
                pending_commits: Vec::new(),
                ahead: 0,
                warnings: vec!["skipped notes.txt".to_string()],
            })
        })
    });
    sync.expect_execute_push().never();
    let (_temp, server) = server_mocking(sync);

    let result = server.selfie_sync_push(push(&[])).await.unwrap();

    assert!(!is_error(&result), "{result:?}");
    let json = tool_json(&result);
    assert_eq!(json["status"], "nothing_to_push", "{json}");
    assert_eq!(json["warnings"], serde_json::json!(["skipped notes.txt"]));
}

// Commits already ahead of the remote are pushed even when nothing new is
// committed.
#[tokio::test]
async fn sync_push_pushes_commits_already_ahead() {
    let mut sync = MockSyncService::new();
    sync.expect_prepare_push().times(1).returning(|_| {
        Box::pin(async {
            Ok(PrepareResult {
                pending_commits: Vec::new(),
                ahead: 2,
                warnings: Vec::new(),
            })
        })
    });
    sync.expect_execute_push()
        .times(1)
        .withf(Vec::is_empty)
        .returning(|_| {
            Box::pin(async {
                git_failed(
                    OperationType::SyncPush,
                    GitStep::Push,
                    GitSyncError::PushRejected,
                )
            })
        });
    let (_temp, server) = server_mocking(sync);

    let json = tool_json(&server.selfie_sync_push(push(&[])).await.unwrap());

    assert_ne!(json["status"], "nothing_to_push", "{json}");
    assert_eq!(json["result"]["step"], "push", "{json}");
}

// A message given for one package replaces its generated one and leaves the
// other's alone. A failed push is an error result carrying the step and kind,
// with the preparation's warnings added.
#[tokio::test]
async fn sync_push_applies_the_given_messages_and_reports_a_failed_push() {
    let mut sync = MockSyncService::new();
    sync.expect_prepare_push().times(1).returning(|_| {
        Box::pin(async {
            Ok(PrepareResult {
                pending_commits: vec![pending("bat"), pending("fish")],
                ahead: 0,
                warnings: vec!["skipped notes.txt".to_string()],
            })
        })
    });
    sync.expect_execute_push()
        .times(1)
        .withf(|commits| {
            let messages: Vec<&str> = commits.iter().map(|c| c.message.as_str()).collect();
            messages == ["feat(bat): theme", "chore(fish): update"]
        })
        .returning(|_| {
            Box::pin(async {
                git_failed(
                    OperationType::SyncPush,
                    GitStep::Push,
                    GitSyncError::PushRejected,
                )
            })
        });
    let (_temp, server) = server_mocking(sync);

    let result = server
        .selfie_sync_push(push(&[("bat", "feat(bat): theme")]))
        .await
        .unwrap();

    assert!(is_error(&result), "{result:?}");
    let json = tool_json(&result);
    assert_eq!(json["result"]["status"], "failure", "{json}");
    assert_eq!(json["result"]["kind"], "push_rejected", "{json}");
    assert_eq!(json["warnings"], serde_json::json!(["skipped notes.txt"]));
}

// A pull git refused is an error result naming the step and kind.
#[tokio::test]
async fn sync_pull_reports_the_step_and_kind_git_refused() {
    let mut sync = MockSyncService::new();
    sync.expect_pull().times(1).returning(|| {
        Box::pin(async {
            git_failed(
                OperationType::SyncPull,
                GitStep::FastForward,
                GitSyncError::Diverged,
            )
        })
    });
    let (_temp, server) = server_mocking(sync);

    let result = server.selfie_sync_pull().await.unwrap();

    assert!(is_error(&result), "{result:?}");
    let json = tool_json(&result);
    assert_eq!(
        (&json["result"]["step"], &json["result"]["kind"]),
        (&"fast_forward".into(), &"diverged".into()),
        "{json}"
    );
}

// Over the protocol, a call reaches the service the server was given: the
// step and kind come from the mock alone.
#[tokio::test]
async fn a_sync_call_over_the_protocol_reaches_the_given_service() {
    let mut sync = MockSyncService::new();
    sync.expect_pull().times(1).returning(|| {
        Box::pin(async {
            git_failed(
                OperationType::SyncPull,
                GitStep::CheckStatus,
                GitSyncError::StagedChanges,
            )
        })
    });
    let (_temp, server) = server_mocking(sync);
    let limit = Duration::from_secs(10);

    let (client, serving) = connected(&server).await;
    let result = tokio::time::timeout(
        limit,
        client.call_tool(CallToolRequestParams::new("selfie_sync_pull")),
    )
    .await
    .expect("the call answered")
    .unwrap();
    let json = tool_json(&result);
    assert_eq!(
        (&json["result"]["step"], &json["result"]["kind"]),
        (&"check_status".into(), &"staged_changes".into()),
        "{json}"
    );

    // A panic in the serving task, such as a failed mock expectation, is
    // reported here rather than lost.
    client.cancel().await.unwrap();
    drop(server);
    tokio::time::timeout(limit, serving)
        .await
        .expect("the server stopped")
        .expect("the server did not panic");
}

// Sync status runs its drift check on the request's dotfile service, so a
// canceled request is reported as canceled rather than completing.
#[tokio::test]
async fn sync_status_stops_with_its_request() {
    let temp = tempfile::TempDir::new().unwrap();
    let packages = temp.path().join("packages");
    std::fs::create_dir(&packages).unwrap();
    let init = test_common::isolated_git()
        .args(["init", "--quiet"])
        .current_dir(&packages)
        .status()
        .unwrap();
    assert!(init.success());
    let server = server_with(&packages, None, Some(&temp.path().join("state")));

    // The control: the same repository, uncanceled, gives a status.
    let json = tool_json(
        &server
            .for_request(CancellationToken::new())
            .selfie_sync_status()
            .await
            .unwrap(),
    );
    assert_ne!(json["result"]["status"], "canceled", "{json}");
    assert!(
        json["data"]
            .as_array()
            .is_some_and(|rows| rows.iter().any(|row| row["type"] == "sync_repo_status")),
        "{json}"
    );

    let canceled = CancellationToken::new();
    canceled.cancel();
    let json = tool_json(
        &server
            .for_request(canceled)
            .selfie_sync_status()
            .await
            .unwrap(),
    );
    assert_eq!(json["result"]["status"], "canceled", "{json}");
}
