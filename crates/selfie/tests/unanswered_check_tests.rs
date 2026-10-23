// How a run scores when it could not answer for a package it covers: the
// listing and the status are Found, a selfie error fails them, and a package
// the spec says needs no check is an answer.

use std::path::{Path, PathBuf};
use std::time::Duration;

use selfie::{
    commands::CommandError,
    package::{
        event::{
            Coverage, OperationFailure, OperationResult, OperationSuccess, Outcome, PackageEvent,
        },
        service::PackageService,
    },
};
use tempfile::TempDir;
use test_common::{
    FakeCommandRunner, collect_events, create_service_test_service_with_runner as service,
    get_operation_result,
};

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
    match get_operation_result(events) {
        Some(OperationResult::Success(success)) => success,
        other => panic!("the run must complete successfully: {other:?}"),
    }
}

fn failed(events: &[PackageEvent]) -> &OperationFailure {
    match get_operation_result(events) {
        Some(OperationResult::Failure(failure)) => failure,
        other => panic!("the run must fail: {other:?}"),
    }
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

async fn list(temp: &TempDir, runner: FakeCommandRunner, all: bool) -> OperationSuccess {
    let events = collect_events(PackageService::list(&service(temp, runner), all).await).await;
    completed(&events).clone()
}

// Lists a valid spec, `fine`, beside `<name>.yml` written as `yaml`.
async fn list_with_spec(name: &str, yaml: &str) -> OperationSuccess {
    let temp = TempDir::new().unwrap();
    write_spec(temp.path(), "fine", "");
    std::fs::write(temp.path().join(format!("{name}.yml")), yaml).unwrap();
    list(&temp, FakeCommandRunner::new(), false).await
}

#[tokio::test]
async fn a_list_with_one_check_timed_out_is_found() {
    let temp = TempDir::new().unwrap();
    write_spec(temp.path(), "slow", "    check: \"check-slow\"\n");
    write_spec(temp.path(), "fast", "    check: \"check-fast\"\n");
    let runner = FakeCommandRunner::new()
        .erroring("check-slow", timed_out("check-slow"))
        .succeeding("check-fast", b"");

    let success = list(&temp, runner, false).await;

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

    let success = list(&temp, runner, false).await;

    assert!(gaps(&success).is_empty(), "{success:?}");
    assert_eq!(success.outcome(), Outcome::Clean);
}

// A spec that does not parse is a row the listing could not answer for, named
// by its file's stem.
#[tokio::test]
async fn a_list_with_an_unparsable_spec_is_found() {
    let success = list_with_spec("broken", "name: [unterminated\n").await;

    assert_eq!(
        gaps(&success),
        [("broken".to_string(), Coverage::Unanswered)]
    );
    assert_eq!(success.outcome(), Outcome::Found);
}

// The summary names each spec with a few words for why, not each error's
// sentence and path, which its rows already carry.
#[tokio::test]
async fn a_list_summary_names_each_broken_spec_in_brief() {
    let temp = TempDir::new().unwrap();
    for name in ["first", "second"] {
        std::fs::write(
            temp.path().join(format!("{name}.yml")),
            "name: [unterminated\n",
        )
        .unwrap();
    }

    let summary = list(&temp, FakeCommandRunner::new(), false)
        .await
        .to_string();

    assert!(summary.contains("first (does not parse)"), "{summary}");
    assert!(summary.contains("second (does not parse)"), "{summary}");
    let dir = temp.path().to_string_lossy();
    assert!(!summary.contains(dir.as_ref()), "{summary}");
}

// A spec selfie refuses is a row the listing could not answer for.
#[tokio::test]
async fn a_list_with_a_refused_spec_is_found() {
    let success = list_with_spec("noenv", "name: noenv\n").await;

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

    let success = list(&temp, FakeCommandRunner::new(), true).await;

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

async fn status_of_pkg(temp: &TempDir, runner: FakeCommandRunner) -> OperationSuccess {
    let events = collect_events(service(temp, runner).status("pkg").await).await;
    completed(&events).clone()
}

async fn status_of_fixture(runner: FakeCommandRunner) -> OperationSuccess {
    let temp = TempDir::new().unwrap();
    write_status_fixture(temp.path());
    status_of_pkg(&temp, runner).await
}

// The status of `pkg`, whose check passes and which depends on `name`, whose
// spec is written as `yaml`, or not at all for `None`.
async fn status_with_dependency(name: &str, yaml: Option<&str>) -> OperationSuccess {
    let temp = TempDir::new().unwrap();
    write_spec(
        temp.path(),
        "pkg",
        &format!("    check: \"check-pkg\"\n    dependencies:\n      - {name}\n"),
    );
    if let Some(yaml) = yaml {
        std::fs::write(temp.path().join(format!("{name}.yml")), yaml).unwrap();
    }
    status_of_pkg(&temp, FakeCommandRunner::new().succeeding("check-pkg", b"")).await
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

// A package listed twice as a dependency, or as a dependency of itself, is
// one gap, named where it was first seen.
#[tokio::test]
async fn a_status_names_each_unanswered_package_once() {
    let temp = TempDir::new().unwrap();
    write_spec(
        temp.path(),
        "pkg",
        "    check: \"check-pkg\"\n    dependencies:\n      - dep\n      - pkg\n      - dep\n",
    );
    write_spec(temp.path(), "dep", "    check: \"check-dep\"\n");
    let runner = FakeCommandRunner::new()
        .erroring("check-pkg", timed_out("check-pkg"))
        .erroring("check-dep", timed_out("check-dep"));

    let success = status_of_pkg(&temp, runner).await;

    assert_eq!(
        gaps(&success),
        [
            ("pkg".to_string(), Coverage::Unanswered),
            ("dep".to_string(), Coverage::Unanswered),
        ]
    );
}

// A dependency with no spec is one the status could not answer for.
#[tokio::test]
async fn a_status_whose_dependency_has_no_spec_is_found() {
    let success = status_with_dependency("missing", None).await;

    assert_eq!(
        gaps(&success),
        [("missing".to_string(), Coverage::Unanswered)]
    );
    assert_eq!(success.outcome(), Outcome::Found);
}

#[tokio::test]
async fn a_status_whose_dependency_is_unparsable_is_found() {
    let success = status_with_dependency("broken", Some("name: [unterminated\n")).await;

    assert_eq!(
        gaps(&success),
        [("broken".to_string(), Coverage::Unanswered)]
    );
    assert_eq!(success.outcome(), Outcome::Found);
}

#[tokio::test]
async fn a_status_whose_dependency_is_refused_is_found() {
    let success = status_with_dependency("noenv", Some("name: noenv\n")).await;

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
    let success = status_with_dependency(
        "elsewhere",
        Some("name: elsewhere\nenvironments:\n  other:\n    install: \"true\"\n"),
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

    let success = status_of_pkg(&temp, FakeCommandRunner::new()).await;

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
        service(&temp, FakeCommandRunner::new())
            .status("nosuch")
            .await,
    )
    .await;

    failed(&events);
}

#[tokio::test]
async fn a_status_of_a_refused_package_fails() {
    let temp = TempDir::new().unwrap();
    std::fs::write(temp.path().join("noenv.yml"), "name: noenv\n").unwrap();

    let events = collect_events(
        service(&temp, FakeCommandRunner::new())
            .status("noenv")
            .await,
    )
    .await;

    failed(&events);
}

// Installs `pkg` from the specs in `temp`.
async fn install(temp: &TempDir, runner: FakeCommandRunner) -> Vec<PackageEvent> {
    collect_events(
        service(temp, runner)
            .install("pkg", selfie::package::service::InstallOptions::default())
            .await,
    )
    .await
}

// Installs `pkg`, which depends on `dep` and recommends `rec`; each installs
// with `true`. Only `slow` has a check, and the runner fails it with `error`
// every time, so every package installs and only `slow`'s turn is in question.
// The runner gives one answer per command, so a check that failed before an
// install could not pass after it; the other packages have none.
async fn install_pkg_with_a_failing_check(slow: &str, error: CommandError) -> Vec<PackageEvent> {
    let temp = TempDir::new().unwrap();
    for (name, extra) in [
        (
            "pkg",
            "    dependencies:\n      - dep\n    recommends:\n      - rec\n",
        ),
        ("dep", ""),
        ("rec", ""),
    ] {
        let check = if name == slow {
            format!("    check: \"check-{name}\"\n")
        } else {
            String::new()
        };
        write_spec(temp.path(), name, &format!("{check}{extra}"));
    }
    let check = format!("check-{slow}");
    let runner = FakeCommandRunner::new()
        .succeeding("true", b"")
        .erroring(&check, error);
    install(&temp, runner).await
}

async fn install_pkg_with_a_slow_check(slow: &str) -> Vec<PackageEvent> {
    let check = format!("check-{slow}");
    install_pkg_with_a_failing_check(slow, timed_out(&check)).await
}

// Counts the warnings whose message starts with `prefix`.
fn warnings_starting(events: &[PackageEvent], prefix: &str) -> usize {
    events
        .iter()
        .filter(|event| {
            matches!(
                event,
                PackageEvent::Warning { message, .. } if message.starts_with(prefix)
            )
        })
        .count()
}

#[tokio::test]
async fn a_post_install_check_that_times_out_is_found() {
    let events = install_pkg_with_a_slow_check("pkg").await;

    let success = completed(&events);
    assert_eq!(gaps(success), [("pkg".to_string(), Coverage::Unanswered)]);
    assert_eq!(success.outcome(), Outcome::Found);
    let summary = success.to_string();
    assert!(
        summary
            .contains("Package 'pkg' installed in environment 'test'; could not answer for: pkg ("),
        "{summary}"
    );
    assert!(!summary.contains("successfully"), "{summary}");
}

// Only the root's result reaches the caller, so a dependency's unconfirmed
// install has to be carried into it.
#[tokio::test]
async fn a_dependency_whose_post_install_check_times_out_makes_the_run_found() {
    let events = install_pkg_with_a_slow_check("dep").await;

    let success = completed(&events);
    assert_eq!(gaps(success), [("dep".to_string(), Coverage::Unanswered)]);
    assert_eq!(success.outcome(), Outcome::Found);
}

// A root found already installed keeps saying so, and where it is, beside the
// dependency no check confirmed.
#[tokio::test]
async fn an_already_installed_root_names_its_unconfirmed_dependency() {
    let temp = TempDir::new().unwrap();
    write_spec(
        temp.path(),
        "pkg",
        "    check: \"check-pkg\"\n    dependencies:\n      - dep\n",
    );
    write_spec(temp.path(), "dep", "    check: \"check-dep\"\n");
    let runner = FakeCommandRunner::new()
        .succeeding("true", b"")
        .succeeding("check-pkg", b"")
        .succeeding("which pkg", b"/opt/bin/pkg\n")
        .erroring("check-dep", timed_out("check-dep"));

    let events = install(&temp, runner).await;

    let success = completed(&events);
    assert_eq!(gaps(success), [("dep".to_string(), Coverage::Unanswered)]);
    assert_eq!(success.outcome(), Outcome::Found);
    let summary = success.to_string();
    assert!(
        summary.starts_with(
            "Package 'pkg' was already installed in environment 'test', at /opt/bin/pkg; \
             could not answer for: dep ("
        ),
        "{summary}"
    );
    assert!(!summary.contains("successfully"), "{summary}");
}

// A recommend leaves the run clean whatever happens to it, so one its check
// could not confirm is a warning naming it.
#[tokio::test]
async fn a_recommend_whose_post_install_check_times_out_leaves_the_run_clean() {
    let events = install_pkg_with_a_slow_check("rec").await;

    let success = completed(&events);
    assert!(gaps(success).is_empty(), "{success:?}");
    assert_eq!(success.outcome(), Outcome::Clean);
    assert_eq!(
        warnings_starting(
            &events,
            "Recommended package 'rec' installed, but its check gave no answer"
        ),
        1,
        "{events:?}"
    );
}

// A recommend's own dependency is named as installed for the recommend, not
// as a recommended package itself.
#[tokio::test]
async fn a_recommends_dependency_is_named_for_the_recommend() {
    let temp = TempDir::new().unwrap();
    write_spec(temp.path(), "pkg", "    recommends:\n      - rec\n");
    write_spec(temp.path(), "rec", "    dependencies:\n      - lib\n");
    write_spec(temp.path(), "lib", "    check: \"check-lib\"\n");
    let runner = FakeCommandRunner::new()
        .succeeding("true", b"")
        .erroring("check-lib", timed_out("check-lib"));

    let events = install(&temp, runner).await;

    assert_eq!(completed(&events).outcome(), Outcome::Clean);
    assert_eq!(
        warnings_starting(
            &events,
            "'lib', a dependency of recommended package 'rec', installed, but its check gave no answer"
        ),
        1,
        "{events:?}"
    );
}

// A check that never ran fails the install, whatever path its failure took:
// one the runner reports as the check's result counts as well as one it
// returns as an error.
#[tokio::test]
async fn a_post_install_check_that_could_not_start_fails_the_install() {
    let events =
        install_pkg_with_a_failing_check("pkg", CommandError::StdoutSpawn("check-pkg".to_string()))
            .await;

    let failure = failed(&events);
    assert!(
        failure.to_string().contains("check-pkg"),
        "the failure must name the check: {failure}"
    );
}
