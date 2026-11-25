// Leak regression for the general command-failure path.
//
// selfie runs user-defined commands and cannot know which of them print a
// credential. A check's failure value, which every adapter receives, carries its
// stderr and never its stdout, and the check tests below fail if that changes.
// An install's failure carries both pipes, bounded, because the terminal showed
// that output as it ran and an MCP client was shown none of it; the install
// tests lock the bound.

// This deliberately does not scan every event for the secret, the way the
// dotfile leak tests do. On the package path a check command's output entering
// the event stream is the design: `CheckResultCompleted` carries it, cut to the
// same bound as a failure, so that `selfie package check` can display it, and
// `PackageEvent::Info` streams an install command's output live. The narrower
// claim, about the failure value, is the one worth locking, because that value
// reaches adapters that never asked for command output.

use futures::StreamExt;
use selfie::package::SpecOrigin;
use tempfile::TempDir;
use test_common::FakeCommandRunner;
use tokio_util::sync::CancellationToken;

use selfie::{
    config::SelfieConfigBuilder,
    fs::RealFileSystem,
    package::{
        event::{
            CheckVerdict, CommandFailure, OperationFailure, OperationResult, OperationSuccess,
            PackageEvent,
        },
        repository::YamlPackageRepository,
        service::{InstallOptions, PackageService, PackageServiceImpl},
    },
};

const SECRET: &str = "s3cr3t-v4lue-DO-NOT-LEAK";
const CHECK_CMD: &str = "check-creds";

// A package whose check command fails, printing a credential to stdout and a
// genuine diagnostic to stderr.
fn failing_check_service(temp: &TempDir) -> impl PackageService {
    std::fs::write(
        temp.path().join("creds.yml"),
        format!("name: creds\nenvironments:\n  test:\n    check: \"{CHECK_CMD}\"\n    install: \"install-creds\"\n"),
    )
    .unwrap();
    let runner = FakeCommandRunner::new().failing_with_stdout(
        CHECK_CMD,
        format!("TOKEN={SECRET}").as_bytes(),
        b"error: vault sealed",
    );
    service_over(temp, runner)
}

const INSTALL_CMD: &str = "install-build";

// Builds a package with no check whose install command fails with `stdout`
// and `stderr`.
fn failing_install_service(
    temp: &TempDir,
    stdout: &[u8],
    stderr: &[u8],
) -> impl PackageService + use<> {
    std::fs::write(
        temp.path().join("build.yml"),
        format!("name: build\nenvironments:\n  test:\n    install: \"{INSTALL_CMD}\"\n"),
    )
    .unwrap();
    let runner = FakeCommandRunner::new().failing_with_stdout(INSTALL_CMD, stdout, stderr);
    service_over(temp, runner)
}

// Builds a service that reads packages from `temp` and runs commands through
// `runner`.
fn service_over(temp: &TempDir, runner: FakeCommandRunner) -> impl PackageService + use<> {
    let package_dir = temp.path().to_path_buf();
    let config = SelfieConfigBuilder::default()
        .environment("test")
        .package_directory(&package_dir)
        .build();

    PackageServiceImpl::new(
        YamlPackageRepository::new(RealFileSystem, package_dir, SpecOrigin::PackageDirectory),
        YamlPackageRepository::new(
            RealFileSystem,
            config.dotfiles_directory(),
            SpecOrigin::DotfilesDirectory,
        ),
        runner,
        test_common::isolated_git_adapter(),
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

    let verdict = events
        .iter()
        .find_map(|e| match e {
            PackageEvent::Completed {
                result:
                    OperationResult::Success(OperationSuccess::PackageChecked {
                        verdict: verdict @ CheckVerdict::NotInstalled { .. },
                        ..
                    }),
                ..
            } => Some(verdict),
            _ => None,
        })
        .expect("a failing check must complete as not installed");

    let rendered = format!("{verdict:?}");
    assert!(
        rendered.contains("vault sealed"),
        "stderr must survive so the failure stays diagnosable: {rendered}"
    );
    assert!(
        rendered.contains(CHECK_CMD),
        "the failing command must be named: {rendered}"
    );
}

#[tokio::test]
async fn a_failed_install_carries_both_pipes_bounded() {
    // Each pipe is past the bound, with a marker in the middle that the bound
    // elides and a last line that it always keeps.
    let pipe = |last| test_common::secret_between_filler(SECRET, last);
    let temp = TempDir::new().unwrap();
    let service = failing_install_service(&temp, &pipe("stdout-last"), &pipe("stderr-last"));

    let events: Vec<PackageEvent> = service
        .install("build", InstallOptions::default())
        .await
        .collect()
        .await;

    let (stdout, stderr) = events
        .iter()
        .find_map(|e| match e {
            PackageEvent::Completed {
                result:
                    OperationResult::Failure(OperationFailure::CommandError(
                        CommandFailure::ExecutionFailed { stdout, stderr, .. },
                    )),
                ..
            } => Some((stdout.as_str(), stderr.as_str())),
            _ => None,
        })
        .expect("a failing install must complete as a failed command");

    for (pipe, tail, last) in [
        ("stdout", stdout, "stdout-last"),
        ("stderr", stderr, "stderr-last"),
    ] {
        assert!(tail.trim_end().ends_with(last), "{pipe}: {tail}");
        assert!(tail.contains("bytes elided"), "{pipe} was not bounded");
        test_common::assert_secret_free(tail, SECRET, pipe);
    }
}

// Builds a package whose install command succeeds with `stdout` and `stderr`
// and whose check says it is not installed, before the install and after it.
fn installed_but_not_found_service(
    temp: &TempDir,
    stdout: &[u8],
    stderr: &[u8],
) -> impl PackageService + use<> {
    std::fs::write(
        temp.path().join("build.yml"),
        format!(
            "name: build\nenvironments:\n  test:\n    install: \"{INSTALL_CMD}\"\n    check: \"{CHECK_CMD}\"\n"
        ),
    )
    .unwrap();
    let runner = FakeCommandRunner::new()
        .succeeding_noisy(INSTALL_CMD, stdout, stderr)
        .failing(CHECK_CMD, b"");
    service_over(temp, runner)
}

#[tokio::test]
async fn an_install_its_check_rejects_carries_both_pipes_bounded() {
    let pipe = |last| test_common::secret_between_filler(SECRET, last);
    let temp = TempDir::new().unwrap();
    let service =
        installed_but_not_found_service(&temp, &pipe("stdout-last"), &pipe("stderr-last"));

    let events: Vec<PackageEvent> = service
        .install("build", InstallOptions::default())
        .await
        .collect()
        .await;

    let (stdout, stderr) = events
        .iter()
        .find_map(|e| match e {
            PackageEvent::Completed {
                result:
                    OperationResult::Failure(OperationFailure::NotInstalledAfterInstall {
                        stdout,
                        stderr,
                        ..
                    }),
                ..
            } => Some((stdout.as_str(), stderr.as_str())),
            _ => None,
        })
        .expect("an install its check rejects must complete as not installed");

    for (pipe, tail, last) in [
        ("stdout", stdout, "stdout-last"),
        ("stderr", stderr, "stderr-last"),
    ] {
        assert!(tail.trim_end().ends_with(last), "{pipe}: {tail}");
        assert!(tail.contains("bytes elided"), "{pipe} was not bounded");
        test_common::assert_secret_free(tail, SECRET, pipe);
    }
}
