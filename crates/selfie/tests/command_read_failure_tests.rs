// What the package operations do when a command's output could not be read.
//
// `run_buffered` must not discard a mid-read IO error and return what it had
// buffered as though it were the whole output — nothing downstream can tell the
// two apart, and every non-streaming path goes through it. (selfie-ql8m)
//
// These assert the consumers' side of the fix — that a command whose output
// selfie could not read does not become a *verdict*. The dotfile path's own
// consequence (a truncated credential reaching a file) is asserted in
// `dotfile_service_tests.rs`, where a write actually happens.

use futures::StreamExt;
use tempfile::TempDir;
use test_common::FakeCommandRunner;
use tokio_util::sync::CancellationToken;

use selfie::{
    config::SelfieConfigBuilder,
    fs::RealFileSystem,
    package::{
        event::{CheckResult, OperationResult, OperationSuccess, PackageEvent},
        git_adapter::GixGitStatusProvider,
        repository::YamlPackageRepository,
        service::{InstallOptions, PackageService, PackageServiceImpl},
    },
};

const CHECK_CMD: &str = "check-pkg";
const INSTALL_CMD: &str = "install-pkg";

// A one-package service whose commands answer from `runner`.
fn service(temp: &TempDir, runner: FakeCommandRunner) -> impl PackageService {
    let package_dir = temp.path().to_path_buf();
    std::fs::write(
        package_dir.join("pkg.yml"),
        format!(
            "name: pkg\nenvironments:\n  test:\n    check: \"{CHECK_CMD}\"\n    install: \"{INSTALL_CMD}\"\n"
        ),
    )
    .unwrap();

    let config = SelfieConfigBuilder::default()
        .environment("test")
        .package_directory(&package_dir)
        .build();

    PackageServiceImpl::new(
        YamlPackageRepository::new(RealFileSystem, package_dir),
        runner,
        GixGitStatusProvider,
        config,
        CancellationToken::new(),
    )
}

async fn collect(stream: selfie::package::event::EventStream) -> Vec<PackageEvent> {
    stream.collect().await
}

fn completed(events: &[PackageEvent]) -> &OperationResult {
    events
        .iter()
        .find_map(|e| match e {
            PackageEvent::Completed { result, .. } => Some(result),
            _ => None,
        })
        .expect("the operation should complete")
}

// The verdict a check reported, as carried by its own event.
//
// `create_operation_result` turns `CheckResult::Error` into an
// `OperationResult::Failure`, so the `Completed` event cannot distinguish "the
// check said not-installed" from "selfie could not read what the check said" —
// both are failures there. This event is where the distinction survives, so it
// is what the tests below assert on.
fn check_verdict(events: &[PackageEvent]) -> &CheckResult {
    events
        .iter()
        .find_map(|e| match e {
            PackageEvent::CheckResultCompleted { check_result, .. } => Some(&check_result.result),
            _ => None,
        })
        .expect("the check should report a result")
}

#[tokio::test]
async fn a_check_whose_output_could_not_be_read_is_an_error_not_a_verdict() {
    // The dangerous shape is exit 0 with a failed stdout read: `is_success()`
    // says installed, and the bytes backing that claim are a prefix of whatever
    // the command was writing. A truncated stdout could equally flip the verdict
    // the other way. Neither may be reported as a verdict.
    //
    // The cost of this decision, stated because it is real: a check that today
    // returns a *correct* verdict from `is_success()` alone now becomes an error,
    // which `log_proceeding_with_installation` turns into a warning and then runs
    // the install command anyway. That is the deliberate trade — a wrong
    // not-installed verdict is worse than a redundant install.
    let temp = TempDir::new().unwrap();
    let runner = FakeCommandRunner::new().stdout_read_failing(CHECK_CMD);

    let events = collect(service(&temp, runner).check("pkg").await).await;

    let verdict = check_verdict(&events);
    assert!(
        matches!(verdict, CheckResult::Error(_)),
        "an unreadable check became the verdict {verdict:?}"
    );
    assert!(
        matches!(completed(&events), OperationResult::Failure(_)),
        "an unreadable check completed as a success"
    );
}

#[tokio::test]
async fn a_check_that_can_be_read_still_produces_a_verdict() {
    // Control. Without it the test above passes against a check that errors
    // unconditionally, which would assert nothing about the read.
    let temp = TempDir::new().unwrap();
    let runner = FakeCommandRunner::new().succeeding(CHECK_CMD, b"installed");

    let events = collect(service(&temp, runner).check("pkg").await).await;

    let verdict = check_verdict(&events);
    assert!(
        matches!(verdict, CheckResult::Success { .. }),
        "expected a verdict, got {verdict:?}"
    );
    assert!(matches!(
        completed(&events),
        OperationResult::Success(OperationSuccess::PackageChecked { .. })
    ));
}

#[tokio::test]
async fn an_executable_path_is_not_reported_when_its_lookup_could_not_be_read() {
    // `find_executable_path` used `stdout_str().trim()` as a path. A truncated
    // read yields a path that either fails to resolve or, worse, resolves to a
    // different binary than the command actually named.
    let temp = TempDir::new().unwrap();
    let which = if cfg!(target_os = "windows") {
        "where pkg"
    } else {
        "which pkg"
    };
    let runner = FakeCommandRunner::new()
        .succeeding(CHECK_CMD, b"already here")
        .stdout_read_failing(which);

    let events = collect(
        service(&temp, runner)
            .install("pkg", InstallOptions::default())
            .await,
    )
    .await;

    match completed(&events) {
        OperationResult::Success(OperationSuccess::PackageInstalled {
            executable_path, ..
        }) => {
            assert_eq!(
                *executable_path, None,
                "a path was reported from output selfie could not read"
            );
        }
        other => panic!("expected an install to complete, got {other:?}"),
    }
}

#[tokio::test]
async fn an_executable_path_is_reported_when_its_lookup_can_be_read() {
    // Control for the test above: it would pass against an install that never
    // reports a path at all.
    let temp = TempDir::new().unwrap();
    let which = if cfg!(target_os = "windows") {
        "where pkg"
    } else {
        "which pkg"
    };
    let runner = FakeCommandRunner::new()
        .succeeding(CHECK_CMD, b"already here")
        .succeeding(which, b"/usr/local/bin/pkg\n");

    let events = collect(
        service(&temp, runner)
            .install("pkg", InstallOptions::default())
            .await,
    )
    .await;

    match completed(&events) {
        OperationResult::Success(OperationSuccess::PackageInstalled {
            executable_path, ..
        }) => {
            assert_eq!(executable_path.as_deref(), Some("/usr/local/bin/pkg"));
        }
        other => panic!("expected an install to complete, got {other:?}"),
    }
}
