// How a run scores when it could not answer for a package it covers: the
// listing and the status are Found, a selfie error fails them, and a package
// the spec says needs no check is an answer.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use selfie::{
    commands::{CommandError, CommandOutput, CommandRunner, ContentOutput, OutputChunk},
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
    service_with_token(dir, runner, CancellationToken::new())
}

// `service`, running up to four recommends at once and canceled through
// `token`.
fn service_with_token<CR: CommandRunner + Clone + std::fmt::Debug + 'static>(
    dir: &Path,
    runner: CR,
    token: CancellationToken,
) -> impl PackageService {
    let config = SelfieConfigBuilder::default()
        .environment("test")
        .package_directory(dir)
        .max_concurrency_unchecked(4)
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
        token,
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
    collect_events(
        service(temp.path(), runner)
            .install("pkg", selfie::package::service::InstallOptions::default())
            .await,
    )
    .await
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

    let events = collect_events(
        service(temp.path(), runner)
            .install("pkg", selfie::package::service::InstallOptions::default())
            .await,
    )
    .await;

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

    let events = collect_events(
        service(temp.path(), runner)
            .install("pkg", selfie::package::service::InstallOptions::default())
            .await,
    )
    .await;

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

    let failure = events
        .iter()
        .find_map(|event| match event {
            PackageEvent::Completed {
                result: OperationResult::Failure(failure),
                ..
            } => Some(failure),
            _ => None,
        })
        .unwrap_or_else(|| panic!("the install must fail: {events:?}"));
    assert!(
        failure.to_string().contains("check-pkg"),
        "the failure must name the check: {failure}"
    );
}

// Returns each step's message and each completion, in order: the shape of an
// install run's events.
fn shape(events: &[PackageEvent]) -> Vec<String> {
    events
        .iter()
        .filter_map(|event| match event {
            PackageEvent::Progress { message, .. } => Some(format!("step: {message}")),
            PackageEvent::Completed {
                result: OperationResult::Success(success),
                ..
            } => Some(format!("completed: {success}")),
            PackageEvent::Completed {
                result: OperationResult::Failure(failure),
                ..
            } => Some(format!("failed: {failure}")),
            _ => None,
        })
        .collect()
}

// A dependency installable only by already being installed is checked before
// anything installs, and still takes its own turn: with nothing shared, a run
// keeps every step it has always had.
#[tokio::test]
async fn a_check_only_dependency_without_recommends_is_unchanged() {
    let temp = TempDir::new().unwrap();
    write_spec(temp.path(), "pkg", "    dependencies:\n      - lib\n");
    std::fs::write(
        temp.path().join("lib.yml"),
        "name: lib\nenvironments:\n  test:\n    install: \"# TODO\"\n    check: \"check-lib\"\n",
    )
    .unwrap();
    let runner = FakeCommandRunner::new()
        .succeeding("true", b"")
        .succeeding("check-lib", b"");

    let events = collect_events(
        service(temp.path(), runner.clone())
            .install("pkg", selfie::package::service::InstallOptions::default())
            .await,
    )
    .await;

    assert_eq!(
        shape(&events),
        [
            "step: Resolving package dependencies",
            "step: Checking whether lib is already installed",
            "step: Fetching package: lib",
            "step: Checking package environment",
            "step: Fetching package: pkg",
            "step: Checking package environment",
            "step: Checking whether pkg is already installed",
            "step: Checking if package has `install` command",
            "step: Running the install command for pkg",
            "step: Skipping installation verification (no check command)",
            "step: Package installation completed",
            "completed: Package 'pkg' installation completed successfully in environment 'test'",
        ]
    );
    let checks = runner
        .calls()
        .iter()
        .filter(|(command, _)| command == "check-lib")
        .count();
    assert_eq!(checks, 1);
}

// A runner that answers as `FakeCommandRunner` does, but holds `slow` pending
// for many polls before answering, so another graph runs while it waits. It
// counts how often `slow` started.
#[derive(Debug, Clone)]
struct Holding {
    inner: FakeCommandRunner,
    slow: &'static str,
    started: Arc<AtomicUsize>,
}

impl Holding {
    async fn hold(&self, command: &str) {
        if command == self.slow {
            self.started.fetch_add(1, Ordering::SeqCst);
            for _ in 0..200 {
                tokio::task::yield_now().await;
            }
        }
    }
}

impl CommandRunner for Holding {
    async fn is_command_available(&self, command: &str) -> bool {
        self.inner.is_command_available(command).await
    }

    async fn execute(
        &self,
        command: &str,
        working_dir: &Path,
        token: &CancellationToken,
    ) -> Result<CommandOutput, CommandError> {
        self.hold(command).await;
        self.inner.execute(command, working_dir, token).await
    }

    async fn execute_in_dir(
        &self,
        command: &str,
        working_dir: &Path,
        timeout: Duration,
        token: &CancellationToken,
    ) -> Result<CommandOutput, CommandError> {
        self.hold(command).await;
        self.inner
            .execute_in_dir(command, working_dir, timeout, token)
            .await
    }

    async fn execute_streaming(
        &self,
        command: &str,
        working_dir: &Path,
        timeout: Duration,
        output_sender: tokio::sync::mpsc::Sender<OutputChunk>,
        token: &CancellationToken,
    ) -> Result<CommandOutput, CommandError> {
        self.hold(command).await;
        self.inner
            .execute_streaming(command, working_dir, timeout, output_sender, token)
            .await
    }

    async fn execute_for_content(
        &self,
        command: &str,
        working_dir: &Path,
        timeout: Duration,
        token: &CancellationToken,
    ) -> Result<ContentOutput, CommandError> {
        self.hold(command).await;
        self.inner
            .execute_for_content(command, working_dir, timeout, token)
            .await
    }
}

// Counts the events `matches` accepts.
fn count(events: &[PackageEvent], matches: impl Fn(&PackageEvent) -> bool) -> usize {
    events.iter().filter(|event| matches(event)).count()
}

fn recommend_succeeded(events: &[PackageEvent], name: &str) -> usize {
    count(
        events,
        |event| matches!(event, PackageEvent::RecommendSucceeded { recommend_name, .. } if recommend_name == name),
    )
}

fn fetched(events: &[PackageEvent], name: &str) -> usize {
    let step = format!("Fetching package: {name}");
    count(
        events,
        |event| matches!(event, PackageEvent::Progress { message, .. } if *message == step),
    )
}

async fn install_root(
    dir: &Path,
    runner: impl CommandRunner + Clone + std::fmt::Debug + 'static,
) -> Vec<PackageEvent> {
    collect_events(
        service(dir, runner)
            .install("pkg", selfie::package::service::InstallOptions::default())
            .await,
    )
    .await
}

// Two recommends that run at once and share a dependency install it once: the
// second waits for the first's turn instead of taking its own.
#[tokio::test]
async fn two_recommends_sharing_a_dependency_install_it_once() {
    let temp = TempDir::new().unwrap();
    write_spec(
        temp.path(),
        "pkg",
        "    recommends:\n      - rec1\n      - rec2\n",
    );
    write_spec(temp.path(), "rec1", "    dependencies:\n      - lib\n");
    write_spec(temp.path(), "rec2", "    dependencies:\n      - lib\n");
    std::fs::write(
        temp.path().join("lib.yml"),
        "name: lib\nenvironments:\n  test:\n    install: \"install-lib\"\n",
    )
    .unwrap();
    let inner = FakeCommandRunner::new()
        .succeeding("true", b"")
        .succeeding("install-lib", b"");
    let runner = Holding {
        inner: inner.clone(),
        slow: "install-lib",
        started: Arc::new(AtomicUsize::new(0)),
    };

    let events = install_root(temp.path(), runner.clone()).await;

    assert_eq!(completed(&events).outcome(), Outcome::Clean, "{events:?}");
    assert_eq!(runner.started.load(Ordering::SeqCst), 1, "{events:?}");
    assert_eq!(fetched(&events, "lib"), 1, "{events:?}");
    assert_eq!(recommend_succeeded(&events, "rec1"), 1, "{events:?}");
    assert_eq!(recommend_succeeded(&events, "rec2"), 1, "{events:?}");
    // The second recommend started while the first's turn for `lib` was still
    // pending, so the run did reach the shared cell from both graphs.
    let position = |matches: &dyn Fn(&PackageEvent) -> bool| {
        events.iter().position(matches).expect("the event is there")
    };
    let both_started = position(&|event| {
        matches!(event, PackageEvent::RecommendStarted { recommend_name, .. } if recommend_name == "rec2")
    })
    .max(position(&|event| {
        matches!(event, PackageEvent::RecommendStarted { recommend_name, .. } if recommend_name == "rec1")
    }));
    let lib_started = position(
        &|event| matches!(event, PackageEvent::Progress { message, .. } if message == "Running the install command for lib"),
    );
    let lib_installed = lib_started
        + events[lib_started..]
            .iter()
            .position(|event| {
                matches!(event, PackageEvent::Progress { message, .. } if message == "Package installation completed")
            })
            .expect("lib's turn completes");
    assert!(
        lib_started < both_started && both_started < lib_installed,
        "{events:?}"
    );
}

// `pkg` depends on `lib` and recommends `rec`, which depends on `lib` too.
// `lib`'s check fails before its install and times out after it, under the
// root; the recommend's graph reuses that turn.
#[tokio::test]
async fn a_dependency_shared_by_the_root_and_a_recommend_is_installed_once_under_the_root() {
    let temp = TempDir::new().unwrap();
    write_spec(
        temp.path(),
        "pkg",
        "    dependencies:\n      - lib\n    recommends:\n      - rec\n",
    );
    write_spec(temp.path(), "rec", "    dependencies:\n      - lib\n");
    std::fs::write(
        temp.path().join("lib.yml"),
        "name: lib\nenvironments:\n  test:\n    install: \"install-lib\"\n    check: \"check-lib\"\n",
    )
    .unwrap();
    let runner = FakeCommandRunner::new()
        .succeeding("true", b"")
        .succeeding("install-lib", b"")
        .erroring("check-lib", timed_out("check-lib"));

    let events = install_root(temp.path(), runner.clone()).await;

    let calls: Vec<String> = runner
        .calls()
        .into_iter()
        .map(|(command, _)| command)
        .collect();
    let ran = |command: &str| calls.iter().filter(|called| *called == command).count();
    assert_eq!(ran("install-lib"), 1, "{calls:?}");
    assert_eq!(ran("check-lib"), 2, "{calls:?}");
    let success = completed(&events);
    assert_eq!(gaps(success), [("lib".to_string(), Coverage::Unanswered)]);
    let about_lib = count(&events, |event| match event {
        PackageEvent::Warning { message, .. } => message.contains("'lib'"),
        PackageEvent::RecommendFailed { error, .. } => error.contains("lib"),
        _ => false,
    });
    assert_eq!(about_lib, 0, "{events:?}");
    assert_eq!(recommend_succeeded(&events, "rec"), 1, "{events:?}");
}

// A shared dependency whose install fails under the first recommend is not
// tried again under the second: the failure stands for both.
#[tokio::test]
async fn a_shared_dependency_that_fails_is_installed_once() {
    let temp = TempDir::new().unwrap();
    write_spec(
        temp.path(),
        "pkg",
        "    recommends:\n      - rec1\n      - rec2\n",
    );
    write_spec(temp.path(), "rec1", "    dependencies:\n      - lib\n");
    write_spec(temp.path(), "rec2", "    dependencies:\n      - lib\n");
    std::fs::write(
        temp.path().join("lib.yml"),
        "name: lib\nenvironments:\n  test:\n    install: \"install-lib\"\n",
    )
    .unwrap();
    let inner = FakeCommandRunner::new()
        .succeeding("true", b"")
        .failing("install-lib", b"");
    let runner = Holding {
        inner,
        slow: "install-lib",
        started: Arc::new(AtomicUsize::new(0)),
    };

    let events = install_root(temp.path(), runner.clone()).await;

    assert_eq!(runner.started.load(Ordering::SeqCst), 1, "{events:?}");
    let failed = count(&events, |event| {
        matches!(event, PackageEvent::RecommendFailed { .. })
    });
    assert_eq!(failed, 2, "{events:?}");
    assert_eq!(completed(&events).outcome(), Outcome::Clean);
}

// A dependency installable only by already being installed, shared by the
// root and a recommend, is checked once.
#[tokio::test]
async fn a_shared_check_only_dependency_is_checked_once() {
    let temp = TempDir::new().unwrap();
    write_spec(
        temp.path(),
        "pkg",
        "    dependencies:\n      - lib\n    recommends:\n      - rec\n",
    );
    write_spec(temp.path(), "rec", "    dependencies:\n      - lib\n");
    std::fs::write(
        temp.path().join("lib.yml"),
        "name: lib\nenvironments:\n  test:\n    install: \"# TODO\"\n    check: \"check-lib\"\n",
    )
    .unwrap();
    let runner = FakeCommandRunner::new()
        .succeeding("true", b"")
        .succeeding("check-lib", b"");

    let events = install_root(temp.path(), runner.clone()).await;

    assert_eq!(completed(&events).outcome(), Outcome::Clean, "{events:?}");
    let checks = runner
        .calls()
        .iter()
        .filter(|(command, _)| command == "check-lib")
        .count();
    assert_eq!(checks, 1);
    assert_eq!(recommend_succeeded(&events, "rec"), 1, "{events:?}");
}

// The error each recommend failed with, by name.
fn recommend_error(events: &[PackageEvent], name: &str) -> String {
    events
        .iter()
        .find_map(|event| match event {
            PackageEvent::RecommendFailed {
                recommend_name,
                error,
                ..
            } if recommend_name == name => Some(error.clone()),
            _ => None,
        })
        .unwrap_or_else(|| panic!("{name} must fail: {events:?}"))
}

// `rec1` reaches `lib` through `mid` and `rec2` reaches it directly, and the
// check says `lib` did not take. One turn fails for both, and each graph names
// its own package that needs `lib`.
#[tokio::test]
async fn a_shared_failure_names_each_recommends_own_dependent() {
    let temp = TempDir::new().unwrap();
    write_spec(
        temp.path(),
        "pkg",
        "    recommends:\n      - rec1\n      - rec2\n",
    );
    write_spec(temp.path(), "rec1", "    dependencies:\n      - mid\n");
    write_spec(temp.path(), "mid", "    dependencies:\n      - lib\n");
    write_spec(temp.path(), "rec2", "    dependencies:\n      - lib\n");
    write_spec(temp.path(), "lib", "    check: \"check-lib\"\n");
    let runner = FakeCommandRunner::new()
        .succeeding("true", b"")
        .failing("check-lib", b"");

    let events = install_root(temp.path(), runner).await;

    let rec1 = recommend_error(&events, "rec1");
    assert!(rec1.contains("'lib', which `mid` requires,"), "{rec1}");
    let rec2 = recommend_error(&events, "rec2");
    assert!(rec2.contains("'lib', which `rec2` requires,"), "{rec2}");
    assert!(!rec2.contains("mid"), "{rec2}");
}

// `rec1` reaches `lib` after `slow`, which the runner holds, so `rec2` takes
// `lib`'s turn first. The warning for `lib`'s unanswered check still names
// `rec1`, the first in the root's declared order to reach it.
#[tokio::test]
async fn a_shared_gap_is_named_for_the_first_recommend_in_declared_order() {
    let temp = TempDir::new().unwrap();
    write_spec(
        temp.path(),
        "pkg",
        "    recommends:\n      - rec1\n      - rec2\n",
    );
    write_spec(
        temp.path(),
        "rec1",
        "    dependencies:\n      - slow\n      - lib\n",
    );
    write_spec(temp.path(), "rec2", "    dependencies:\n      - lib\n");
    std::fs::write(
        temp.path().join("slow.yml"),
        "name: slow\nenvironments:\n  test:\n    install: \"install-slow\"\n",
    )
    .unwrap();
    std::fs::write(
        temp.path().join("lib.yml"),
        "name: lib\nenvironments:\n  test:\n    install: \"install-lib\"\n    check: \"check-lib\"\n",
    )
    .unwrap();
    let inner = FakeCommandRunner::new()
        .succeeding("true", b"")
        .succeeding("install-slow", b"")
        .succeeding("install-lib", b"")
        .erroring("check-lib", timed_out("check-lib"));
    let runner = Holding {
        inner: inner.clone(),
        slow: "install-slow",
        started: Arc::new(AtomicUsize::new(0)),
    };

    let events = install_root(temp.path(), runner).await;

    // The runner records a held command only once it is released, so `lib`
    // installing first means `rec2` took its turn while `rec1` was held.
    let calls: Vec<String> = inner
        .calls()
        .into_iter()
        .map(|(command, _)| command)
        .collect();
    let at = |command: &str| calls.iter().position(|called| called == command).unwrap();
    assert!(at("install-lib") < at("install-slow"), "{calls:?}");
    assert_eq!(completed(&events).outcome(), Outcome::Clean);
    let about_lib = count(
        &events,
        |event| matches!(event, PackageEvent::Warning { message, .. } if message.contains("'lib'")),
    );
    assert_eq!(about_lib, 1, "{events:?}");
    assert_eq!(
        warnings_starting(
            &events,
            "'lib', a dependency of recommended package 'rec1', installed, but its check gave no answer"
        ),
        1,
        "{events:?}"
    );
}

// The step and total of `name`'s last progress event: the one just before it
// succeeded.
fn last_progress_of(events: &[PackageEvent], name: &str) -> (usize, usize) {
    let succeeded = events
        .iter()
        .position(|event| {
            matches!(event, PackageEvent::RecommendSucceeded { recommend_name, .. } if recommend_name == name)
        })
        .unwrap_or_else(|| panic!("{name} must succeed: {events:?}"));
    events[..succeeded]
        .iter()
        .rev()
        .find_map(|event| match event {
            PackageEvent::Progress {
                step, total_steps, ..
            } => Some((*step, *total_steps)),
            _ => None,
        })
        .unwrap()
}

// A recommend reusing a turn the root took advances past that turn's steps, so
// its progress ends at its total: seven steps for each of its two packages.
#[tokio::test]
async fn a_recommend_reusing_a_turn_ends_its_progress_at_its_total() {
    let temp = TempDir::new().unwrap();
    write_spec(
        temp.path(),
        "pkg",
        "    dependencies:\n      - lib\n    recommends:\n      - rec\n",
    );
    write_spec(temp.path(), "rec", "    dependencies:\n      - lib\n");
    write_spec(temp.path(), "lib", "    check: \"check-lib\"\n");
    let runner = FakeCommandRunner::new()
        .succeeding("true", b"")
        .erroring("check-lib", timed_out("check-lib"));

    let events = install_root(temp.path(), runner).await;

    assert_eq!(last_progress_of(&events, "rec"), (14, 14), "{events:?}");
}

// The same for a check-only dependency, whose check and turn the recommend
// both reuse: it was already installed, so it counts three steps, not seven.
#[tokio::test]
async fn a_recommend_reusing_a_check_only_turn_ends_its_progress_at_its_total() {
    let temp = TempDir::new().unwrap();
    write_spec(
        temp.path(),
        "pkg",
        "    dependencies:\n      - lib\n    recommends:\n      - rec\n",
    );
    write_spec(temp.path(), "rec", "    dependencies:\n      - lib\n");
    std::fs::write(
        temp.path().join("lib.yml"),
        "name: lib\nenvironments:\n  test:\n    install: \"# TODO\"\n    check: \"check-lib\"\n",
    )
    .unwrap();
    let runner = FakeCommandRunner::new()
        .succeeding("true", b"")
        .succeeding("check-lib", b"");

    let events = install_root(temp.path(), runner).await;

    assert_eq!(last_progress_of(&events, "rec"), (10, 10), "{events:?}");
}

// A recommend waiting on another graph's turn when the run is canceled stops
// untried, not failed.
#[tokio::test]
async fn a_canceled_run_reports_a_waiting_recommend_as_untried() {
    let temp = TempDir::new().unwrap();
    write_spec(
        temp.path(),
        "pkg",
        "    recommends:\n      - rec1\n      - rec2\n",
    );
    write_spec(temp.path(), "rec1", "    dependencies:\n      - lib\n");
    write_spec(temp.path(), "rec2", "    dependencies:\n      - lib\n");
    std::fs::write(
        temp.path().join("lib.yml"),
        "name: lib\nenvironments:\n  test:\n    install: \"install-lib\"\n",
    )
    .unwrap();
    let token = CancellationToken::new();
    let runner = Canceling {
        inner: FakeCommandRunner::new()
            .succeeding("true", b"")
            .succeeding("install-lib", b""),
        token: token.clone(),
    };

    let events = collect_events(
        service_with_token(temp.path(), runner, token)
            .install("pkg", selfie::package::service::InstallOptions::default())
            .await,
    )
    .await;

    // Only the recommend whose own turn the cancel interrupted is a failure;
    // the one that was waiting is named untried.
    let failed = count(&events, |event| {
        matches!(event, PackageEvent::RecommendFailed { .. })
    });
    assert_eq!(failed, 1, "{events:?}");
    let untried = count(&events, |event| match event {
        PackageEvent::RecommendsUntried { names, .. } => names.iter().any(|name| name == "rec2"),
        _ => false,
    });
    assert_eq!(untried, 1, "{events:?}");
}

// A runner that answers as `FakeCommandRunner` does, except that `install-lib`
// yields so the other recommend reaches `lib`, then cancels the run and fails
// as a canceled command does.
#[derive(Debug, Clone)]
struct Canceling {
    inner: FakeCommandRunner,
    token: CancellationToken,
}

impl Canceling {
    async fn answer(
        &self,
        command: &str,
        working_dir: &Path,
        token: &CancellationToken,
    ) -> Result<CommandOutput, CommandError> {
        if command == "install-lib" {
            for _ in 0..200 {
                tokio::task::yield_now().await;
            }
            self.token.cancel();
            return Err(CommandError::Cancelled {
                command: command.to_string(),
                working_directory: working_dir.to_path_buf(),
            });
        }
        self.inner.execute(command, working_dir, token).await
    }
}

impl CommandRunner for Canceling {
    async fn is_command_available(&self, command: &str) -> bool {
        self.inner.is_command_available(command).await
    }

    async fn execute(
        &self,
        command: &str,
        working_dir: &Path,
        token: &CancellationToken,
    ) -> Result<CommandOutput, CommandError> {
        self.answer(command, working_dir, token).await
    }

    async fn execute_in_dir(
        &self,
        command: &str,
        working_dir: &Path,
        _timeout: Duration,
        token: &CancellationToken,
    ) -> Result<CommandOutput, CommandError> {
        self.answer(command, working_dir, token).await
    }

    async fn execute_streaming(
        &self,
        command: &str,
        working_dir: &Path,
        _timeout: Duration,
        _output_sender: tokio::sync::mpsc::Sender<OutputChunk>,
        token: &CancellationToken,
    ) -> Result<CommandOutput, CommandError> {
        self.answer(command, working_dir, token).await
    }

    async fn execute_for_content(
        &self,
        command: &str,
        working_dir: &Path,
        timeout: Duration,
        token: &CancellationToken,
    ) -> Result<ContentOutput, CommandError> {
        self.inner
            .execute_for_content(command, working_dir, timeout, token)
            .await
    }
}
