// How a run scores when it could not answer for a package it covers: the
// listing and the status are Found, a selfie error fails them, and a package
// the spec says needs no check is an answer.

use std::path::{Path, PathBuf};
use std::time::Duration;

use selfie::{
    commands::{CommandError, CommandRunner},
    config::SelfieConfigBuilder,
    fs::RealFileSystem,
    package::{
        SpecOrigin,
        event::{Coverage, OperationResult, OperationSuccess, Outcome, PackageEvent},
        git_adapter::GixGitStatusProvider,
        repository::YamlPackageRepository,
        service::{PackageService, PackageServiceImpl},
    },
};
use tempfile::TempDir;
use test_common::{FakeCommandRunner, collect_events};
use tokio_util::sync::CancellationToken;

fn service<CR: CommandRunner + Clone + std::fmt::Debug + 'static>(
    dir: &Path,
    runner: CR,
) -> impl PackageService {
    let config = SelfieConfigBuilder::default()
        .environment("test")
        .package_directory(dir)
        .build();
    PackageServiceImpl::new(
        YamlPackageRepository::new(
            RealFileSystem,
            dir.to_path_buf(),
            SpecOrigin::PackageDirectory,
        ),
        YamlPackageRepository::new(
            RealFileSystem,
            config.dotfiles_directory(),
            SpecOrigin::DotfilesDirectory,
        ),
        runner,
        GixGitStatusProvider,
        config,
        CancellationToken::new(),
    )
}

// Writes `<name>.yml` with `fields` added to its `test` environment, which
// installs with `true`.
fn write_spec(dir: &Path, name: &str, fields: &str) {
    std::fs::write(
        dir.join(format!("{name}.yml")),
        format!("name: {name}\nenvironments:\n  test:\n    install: \"true\"\n{fields}"),
    )
    .unwrap();
}

fn timed_out(command: &str) -> CommandError {
    CommandError::Timeout {
        command: command.to_string(),
        timeout: Duration::from_secs(1),
        working_directory: PathBuf::from("/"),
    }
}

fn completed(events: &[PackageEvent]) -> &OperationSuccess {
    events
        .iter()
        .find_map(|event| match event {
            PackageEvent::Completed {
                result: OperationResult::Success(success),
                ..
            } => Some(success),
            _ => None,
        })
        .unwrap_or_else(|| panic!("the run must complete successfully: {events:?}"))
}

// Returns each gap's package with its coverage, in order.
fn gaps(success: &OperationSuccess) -> Vec<(String, Coverage)> {
    success
        .gaps()
        .unwrap_or_default()
        .iter()
        .map(|gap| (gap.package_name().to_string(), gap.coverage()))
        .collect()
}

async fn list(dir: &Path, runner: FakeCommandRunner, all: bool) -> OperationSuccess {
    let events = collect_events(PackageService::list(&service(dir, runner), all).await).await;
    completed(&events).clone()
}

#[tokio::test]
async fn a_list_with_one_check_timed_out_is_found() {
    let temp = TempDir::new().unwrap();
    write_spec(temp.path(), "slow", "    check: \"check-slow\"\n");
    write_spec(temp.path(), "fast", "    check: \"check-fast\"\n");
    let runner = FakeCommandRunner::new()
        .erroring("check-slow", timed_out("check-slow"))
        .succeeding("check-fast", b"");

    let success = list(temp.path(), runner, false).await;

    assert_eq!(gaps(&success), [("slow".to_string(), Coverage::Unanswered)]);
    assert_eq!(success.outcome(), Outcome::Found);
    assert!(
        success
            .to_string()
            .contains("; could not answer for: slow ("),
        "{success}"
    );
}

// Installed, not installed and no check command are each an answer.
#[tokio::test]
async fn a_list_where_every_check_answered_is_clean() {
    let temp = TempDir::new().unwrap();
    write_spec(temp.path(), "present", "    check: \"check-present\"\n");
    write_spec(temp.path(), "absent", "    check: \"check-absent\"\n");
    write_spec(temp.path(), "unchecked", "");
    let runner = FakeCommandRunner::new()
        .succeeding("check-present", b"")
        .failing("check-absent", b"");

    let success = list(temp.path(), runner, false).await;

    assert!(gaps(&success).is_empty(), "{success:?}");
    assert_eq!(success.outcome(), Outcome::Clean);
}

// A spec that does not parse is a row the listing could not answer for, named
// by its file's stem.
#[tokio::test]
async fn a_list_with_an_unparsable_spec_is_found() {
    let temp = TempDir::new().unwrap();
    write_spec(temp.path(), "fine", "");
    std::fs::write(temp.path().join("broken.yml"), "name: [unterminated\n").unwrap();

    let success = list(temp.path(), FakeCommandRunner::new(), false).await;

    assert_eq!(
        gaps(&success),
        [("broken".to_string(), Coverage::Unanswered)]
    );
    assert_eq!(success.outcome(), Outcome::Found);
}

// A spec selfie refuses is a row the listing could not answer for.
#[tokio::test]
async fn a_list_with_a_refused_spec_is_found() {
    let temp = TempDir::new().unwrap();
    write_spec(temp.path(), "fine", "");
    std::fs::write(temp.path().join("noenv.yml"), "name: noenv\n").unwrap();

    let success = list(temp.path(), FakeCommandRunner::new(), false).await;

    assert_eq!(
        gaps(&success),
        [("noenv".to_string(), Coverage::Unanswered)]
    );
    assert_eq!(success.outcome(), Outcome::Found);
}

// Under `--all`, a row for another environment has no check here; its spec
// answered for it.
#[tokio::test]
async fn a_list_all_with_a_row_not_in_this_environment_is_clean() {
    let temp = TempDir::new().unwrap();
    std::fs::write(
        temp.path().join("elsewhere.yml"),
        "name: elsewhere\nenvironments:\n  other:\n    install: \"true\"\n",
    )
    .unwrap();

    let success = list(temp.path(), FakeCommandRunner::new(), true).await;

    assert!(gaps(&success).is_empty(), "{success:?}");
    assert_eq!(success.outcome(), Outcome::Clean);
}

// `pkg` depends on `dep` and recommends `rec`, and each has its own check.
fn write_status_fixture(dir: &Path) {
    write_spec(
        dir,
        "pkg",
        "    check: \"check-pkg\"\n    dependencies:\n      - dep\n    recommends:\n      - rec\n",
    );
    write_spec(dir, "dep", "    check: \"check-dep\"\n");
    write_spec(dir, "rec", "    check: \"check-rec\"\n");
}

async fn status_of_pkg(dir: &Path, runner: FakeCommandRunner) -> OperationSuccess {
    let events = collect_events(service(dir, runner).status("pkg").await).await;
    completed(&events).clone()
}

async fn status_of_fixture(runner: FakeCommandRunner) -> OperationSuccess {
    let temp = TempDir::new().unwrap();
    write_status_fixture(temp.path());
    status_of_pkg(temp.path(), runner).await
}

#[tokio::test]
async fn a_status_whose_check_timed_out_is_found() {
    let success = status_of_fixture(
        FakeCommandRunner::new()
            .erroring("check-pkg", timed_out("check-pkg"))
            .succeeding("check-dep", b"")
            .succeeding("check-rec", b""),
    )
    .await;

    assert_eq!(gaps(&success), [("pkg".to_string(), Coverage::Unanswered)]);
    assert_eq!(success.outcome(), Outcome::Found);
    let summary = success.to_string();
    assert!(summary.contains("could not answer for: pkg ("), "{summary}");
    assert!(!summary.contains("successfully"), "{summary}");
}

// The package's own check answered; its dependency's did not.
#[tokio::test]
async fn a_status_whose_dependency_check_timed_out_is_found() {
    let success = status_of_fixture(
        FakeCommandRunner::new()
            .succeeding("check-pkg", b"")
            .erroring("check-dep", timed_out("check-dep"))
            .succeeding("check-rec", b""),
    )
    .await;

    assert_eq!(gaps(&success), [("dep".to_string(), Coverage::Unanswered)]);
    assert_eq!(success.outcome(), Outcome::Found);
}

// A dependency with no spec is one the status could not answer for.
#[tokio::test]
async fn a_status_whose_dependency_has_no_spec_is_found() {
    let temp = TempDir::new().unwrap();
    write_spec(
        temp.path(),
        "pkg",
        "    check: \"check-pkg\"\n    dependencies:\n      - missing\n",
    );

    let success = status_of_pkg(
        temp.path(),
        FakeCommandRunner::new().succeeding("check-pkg", b""),
    )
    .await;

    assert_eq!(
        gaps(&success),
        [("missing".to_string(), Coverage::Unanswered)]
    );
    assert_eq!(success.outcome(), Outcome::Found);
}

#[tokio::test]
async fn a_status_whose_dependency_is_unparsable_is_found() {
    let temp = TempDir::new().unwrap();
    write_spec(
        temp.path(),
        "pkg",
        "    check: \"check-pkg\"\n    dependencies:\n      - broken\n",
    );
    std::fs::write(temp.path().join("broken.yml"), "name: [unterminated\n").unwrap();

    let success = status_of_pkg(
        temp.path(),
        FakeCommandRunner::new().succeeding("check-pkg", b""),
    )
    .await;

    assert_eq!(
        gaps(&success),
        [("broken".to_string(), Coverage::Unanswered)]
    );
    assert_eq!(success.outcome(), Outcome::Found);
}

#[tokio::test]
async fn a_status_whose_dependency_is_refused_is_found() {
    let temp = TempDir::new().unwrap();
    write_spec(
        temp.path(),
        "pkg",
        "    check: \"check-pkg\"\n    dependencies:\n      - noenv\n",
    );
    std::fs::write(temp.path().join("noenv.yml"), "name: noenv\n").unwrap();

    let success = status_of_pkg(
        temp.path(),
        FakeCommandRunner::new().succeeding("check-pkg", b""),
    )
    .await;

    assert_eq!(
        gaps(&success),
        [("noenv".to_string(), Coverage::Unanswered)]
    );
    assert_eq!(success.outcome(), Outcome::Found);
}

// A dependency with no entry for this environment answered too: its spec says
// it does not apply here.
#[tokio::test]
async fn a_status_whose_dependency_is_not_in_this_environment_is_clean() {
    let temp = TempDir::new().unwrap();
    write_spec(
        temp.path(),
        "pkg",
        "    check: \"check-pkg\"\n    dependencies:\n      - elsewhere\n",
    );
    std::fs::write(
        temp.path().join("elsewhere.yml"),
        "name: elsewhere\nenvironments:\n  other:\n    install: \"true\"\n",
    )
    .unwrap();

    let success = status_of_pkg(
        temp.path(),
        FakeCommandRunner::new().succeeding("check-pkg", b""),
    )
    .await;

    assert!(gaps(&success).is_empty(), "{success:?}");
    assert_eq!(success.outcome(), Outcome::Clean);
}

// A package that declares no check answered for itself.
#[tokio::test]
async fn a_status_of_a_package_with_no_check_is_clean() {
    let temp = TempDir::new().unwrap();
    write_spec(temp.path(), "pkg", "");

    let success = status_of_pkg(temp.path(), FakeCommandRunner::new()).await;

    assert!(gaps(&success).is_empty(), "{success:?}");
    assert_eq!(success.outcome(), Outcome::Clean);
}

// A recommend's unanswered check is shown in its row and leaves the run clean:
// a status never covers a recommend.
#[tokio::test]
async fn a_status_whose_recommend_check_timed_out_is_clean() {
    let success = status_of_fixture(
        FakeCommandRunner::new()
            .succeeding("check-pkg", b"")
            .succeeding("check-dep", b"")
            .erroring("check-rec", timed_out("check-rec")),
    )
    .await;

    assert!(gaps(&success).is_empty(), "{success:?}");
    assert_eq!(success.outcome(), Outcome::Clean);
}

#[tokio::test]
async fn a_status_whose_check_says_not_installed_is_clean() {
    let success = status_of_fixture(
        FakeCommandRunner::new()
            .failing("check-pkg", b"")
            .failing("check-dep", b"")
            .succeeding("check-rec", b""),
    )
    .await;

    assert!(gaps(&success).is_empty(), "{success:?}");
    assert_eq!(success.outcome(), Outcome::Clean);
}

// A package named on the command line that selfie cannot look up fails the
// status: the request itself failed.
#[tokio::test]
async fn a_status_of_a_missing_package_fails() {
    let temp = TempDir::new().unwrap();

    let events = collect_events(
        service(temp.path(), FakeCommandRunner::new())
            .status("nosuch")
            .await,
    )
    .await;

    assert!(
        events.iter().any(|event| matches!(
            event,
            PackageEvent::Completed {
                result: OperationResult::Failure(_),
                ..
            }
        )),
        "{events:?}"
    );
}

#[tokio::test]
async fn a_status_of_a_refused_package_fails() {
    let temp = TempDir::new().unwrap();
    std::fs::write(temp.path().join("noenv.yml"), "name: noenv\n").unwrap();

    let events = collect_events(
        service(temp.path(), FakeCommandRunner::new())
            .status("noenv")
            .await,
    )
    .await;

    assert!(
        events.iter().any(|event| matches!(
            event,
            PackageEvent::Completed {
                result: OperationResult::Failure(_),
                ..
            }
        )),
        "{events:?}"
    );
}
