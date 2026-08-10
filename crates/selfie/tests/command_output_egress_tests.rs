//! Leak regression for the general command-failure path.
//!
//! selfie runs user-defined commands and cannot know which of them print a
//! credential, so a failure value that every adapter receives must not carry a
//! command's whole output. `CommandFailure::ExecutionFailed` therefore has no
//! `stdout` field, and these tests fail if one comes back.
//!
//! **This deliberately does not scan every event for the secret**, the way the
//! dotfile leak tests do. That pattern belongs to the dotfile resolve path, where
//! resolved content never enters an event at all. On the
//! package path a check command's output entering the event stream *is* the
//! design: `CheckResultCompleted` carries it so that `selfie package check` can
//! display it, and `PackageEvent::Info` streams an install command's output live.
//! Scanning every event here would assert something this path never promised and
//! could never pass. The narrower claim — that the *failure value* stays clean —
//! is the one worth locking, because that value reaches adapters that never asked
//! for command output.

use futures::StreamExt;
use tempfile::TempDir;
use test_common::FakeCommandRunner;
use tokio_util::sync::CancellationToken;

use selfie::{
    config::SelfieConfigBuilder,
    fs::RealFileSystem,
    package::{
        event::{OperationResult, PackageEvent},
        git_adapter::GixGitStatusProvider,
        repository::YamlPackageRepository,
        service::{PackageService, PackageServiceImpl},
    },
};

const SECRET: &str = "s3cr3t-v4lue-DO-NOT-LEAK";
const CHECK_CMD: &str = "check-creds";

// A package whose check command fails, printing a credential to stdout and a
// genuine diagnostic to stderr.
fn failing_check_service(temp: &TempDir) -> impl PackageService {
    let package_dir = temp.path().to_path_buf();
    std::fs::write(
        package_dir.join("creds.yml"),
        format!("name: creds\nenvironments:\n  test:\n    check: \"{CHECK_CMD}\"\n    install: \"install-creds\"\n"),
    )
    .unwrap();

    let config = SelfieConfigBuilder::default()
        .environment("test")
        .package_directory(&package_dir)
        .build();

    let runner = FakeCommandRunner::new().failing_with_stdout(
        CHECK_CMD,
        format!("TOKEN={SECRET}").as_bytes(),
        b"error: vault sealed",
    );

    PackageServiceImpl::new(
        YamlPackageRepository::new(RealFileSystem, package_dir),
        runner,
        GixGitStatusProvider,
        config,
        CancellationToken::new(),
    )
}

#[tokio::test]
async fn a_failing_check_keeps_its_stdout_out_of_the_completed_event() {
    let temp = TempDir::new().unwrap();
    let service = failing_check_service(&temp);

    let events: Vec<PackageEvent> = service.check("creds").await.collect().await;

    let completed = events
        .iter()
        .find(|e| matches!(e, PackageEvent::Completed { .. }))
        .expect("the check must report a completion");

    // Positive control: the run genuinely produced the secret and carried it on
    // the channel that is allowed to. Without this the assertion below would pass
    // just as happily if the check had never run.
    assert!(
        format!("{events:?}").contains(SECRET),
        "the secret should still reach `CheckResultCompleted`, the acknowledged \
         channel — if it does not, this test is no longer observing anything"
    );

    test_common::assert_secret_free(
        &format!("{completed:?}"),
        SECRET,
        "the failure value every adapter receives",
    );
}

#[tokio::test]
async fn a_failing_check_still_reports_why_it_failed() {
    // The counterweight to the test above: dropping stdout must not cost the
    // diagnostic. stderr is what keeps a failure debuggable, so it has to survive.
    let temp = TempDir::new().unwrap();
    let service = failing_check_service(&temp);

    let events: Vec<PackageEvent> = service.check("creds").await.collect().await;

    let failure = events
        .iter()
        .find_map(|e| match e {
            PackageEvent::Completed {
                result: OperationResult::Failure(f),
                ..
            } => Some(f),
            _ => None,
        })
        .expect("a failing check must complete as a failure");

    let rendered = format!("{failure:?}");
    assert!(
        rendered.contains("vault sealed"),
        "stderr must survive so the failure stays diagnosable: {rendered}"
    );
    assert!(
        rendered.contains(CHECK_CMD),
        "the failing command must be named: {rendered}"
    );
}
