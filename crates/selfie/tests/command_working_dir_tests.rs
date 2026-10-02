// Where the commands a package file configures run: in the package directory,
// which is the child's working directory, whatever characters its name holds.

use std::path::{Path, PathBuf};
use std::time::Duration;

use selfie::{
    commands::{CommandRunner, ShellCommandRunner},
    config::SelfieConfigBuilder,
    fs::RealFileSystem,
    package::{
        SpecOrigin,
        event::{AuditResult, CheckResult, EnvironmentStatus, OperationResult, PackageEvent},
        git_adapter::GixGitStatusProvider,
        repository::YamlPackageRepository,
        service::{InstallOptions, PackageService, PackageServiceImpl},
    },
};
use tempfile::TempDir;
use test_common::{FakeCommandRunner, collect_events};
use tokio_util::sync::CancellationToken;

const INSTALL_CMD: &str = "install-pkg";
const CHECK_CMD: &str = "check-pkg";
const DEP_CHECK_CMD: &str = "check-dep";
const REC_CHECK_CMD: &str = "check-rec";
const AUDIT_CMD: &str = "audit-pkg";

// A service reading specs from `spec_dir`, with `package_directory` configured as
// `package_dir`. The two are the same directory except in a test that needs the
// package directory to go missing after its spec was read.
fn service<CR: CommandRunner + Clone + std::fmt::Debug + 'static>(
    spec_dir: &Path,
    package_dir: &Path,
    runner: CR,
) -> impl PackageService {
    service_with_token(spec_dir, package_dir, runner, CancellationToken::new())
}

// `service`, cancelled through `token`.
fn service_with_token<CR: CommandRunner + Clone + std::fmt::Debug + 'static>(
    spec_dir: &Path,
    package_dir: &Path,
    runner: CR,
    token: CancellationToken,
) -> impl PackageService {
    let config = SelfieConfigBuilder::default()
        .environment("test")
        .package_directory(package_dir)
        .build();

    PackageServiceImpl::new(
        YamlPackageRepository::new(
            RealFileSystem,
            spec_dir.to_path_buf(),
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

// Write `pkg.yml` into `dir`, with `fields` as its `test` environment.
fn write_spec(dir: &Path, fields: &str) {
    std::fs::write(
        dir.join("pkg.yml"),
        format!("name: pkg\nenvironments:\n  test:\n{fields}"),
    )
    .unwrap();
}

fn real_runner() -> ShellCommandRunner {
    ShellCommandRunner::new(ShellCommandRunner::default_shell(), Duration::from_secs(5))
}

fn completed(events: &[PackageEvent]) -> &OperationResult {
    test_common::get_operation_result(events).expect("the operation should complete")
}

// A package directory whose name has a quote and a space, holding a marker file
// a relative path finds only from inside it.
fn quoted_package_dir(temp: &TempDir) -> PathBuf {
    let dir = temp.path().join("it's a dir");
    std::fs::create_dir(&dir).unwrap();
    std::fs::write(dir.join("here.marker"), "").unwrap();
    dir
}

#[tokio::test]
async fn install_is_asked_to_run_in_the_package_directory() {
    let temp = TempDir::new().unwrap();
    write_spec(temp.path(), &format!("    install: \"{INSTALL_CMD}\"\n"));
    let runner = FakeCommandRunner::new().succeeding(INSTALL_CMD, b"");

    let events = collect_events(
        service(temp.path(), temp.path(), runner.clone())
            .install("pkg", InstallOptions::default())
            .await,
    )
    .await;

    assert!(
        matches!(completed(&events), OperationResult::Success(_)),
        "{events:?}"
    );
    // The install, then selfie's own lookup of the installed executable.
    let dir = temp.path().to_path_buf();
    assert_eq!(
        runner.calls(),
        vec![
            (INSTALL_CMD.to_string(), dir.clone()),
            ("which pkg".to_string(), dir),
        ]
    );
}

#[tokio::test]
async fn install_succeeds_in_a_package_directory_with_a_quote_and_a_space() {
    let temp = TempDir::new().unwrap();
    let package_dir = quoted_package_dir(&temp);
    write_spec(&package_dir, "    install: \"test -f ./here.marker\"\n");

    let events = collect_events(
        service(&package_dir, &package_dir, real_runner())
            .install("pkg", InstallOptions::default())
            .await,
    )
    .await;

    assert!(
        matches!(completed(&events), OperationResult::Success(_)),
        "{events:?}"
    );
}

#[tokio::test]
async fn install_in_a_missing_package_directory_fails_naming_it() {
    let temp = TempDir::new().unwrap();
    write_spec(temp.path(), "    install: \"true\"\n");
    let missing = temp.path().join("gone");

    let events = collect_events(
        service(temp.path(), &missing, real_runner())
            .install("pkg", InstallOptions::default())
            .await,
    )
    .await;

    let OperationResult::Failure(failure) = completed(&events) else {
        panic!("install in a missing directory must fail: {events:?}");
    };
    let rendered = failure.to_string();
    assert!(
        rendered.contains(&missing.display().to_string()),
        "the failure must name the directory: {rendered}"
    );
    assert!(
        !rendered.to_lowercase().contains("not found"),
        "the directory is not a missing command: {rendered}"
    );
}

// The commands every route through a check asked the runner for, with the
// directory each was asked to run in.
#[tokio::test]
async fn every_check_route_is_asked_to_run_in_the_package_directory() {
    let temp = TempDir::new().unwrap();
    write_spec(
        temp.path(),
        &format!(
            "    install: \"{INSTALL_CMD}\"\n    check: \"{CHECK_CMD}\"\n    dependencies:\n      - dep\n    recommends:\n      - rec\n"
        ),
    );
    for (name, check) in [("dep", DEP_CHECK_CMD), ("rec", REC_CHECK_CMD)] {
        std::fs::write(
            temp.path().join(format!("{name}.yml")),
            format!("name: {name}\nenvironments:\n  test:\n    install: \"true\"\n    check: \"{check}\"\n"),
        )
        .unwrap();
    }
    // The check fails, so install runs both its checks and its install command.
    let runner = FakeCommandRunner::new()
        .failing(CHECK_CMD, b"")
        .succeeding(DEP_CHECK_CMD, b"")
        .succeeding(REC_CHECK_CMD, b"")
        .succeeding(INSTALL_CMD, b"");
    let service = || service(temp.path(), temp.path(), runner.clone());

    collect_events(service().check("pkg").await).await;
    collect_events(service().list(false).await).await;
    collect_events(service().status("pkg").await).await;
    // The dependency and the recommend each report themselves present.
    collect_events(service().install("pkg", InstallOptions::default()).await).await;

    let calls = runner.calls();
    let checks = calls
        .iter()
        .filter(|(command, _)| command == CHECK_CMD)
        .count();
    // One each from check, list and status, and two from install.
    assert_eq!(checks, 5, "{calls:?}");
    for other in [DEP_CHECK_CMD, REC_CHECK_CMD] {
        assert!(
            calls.iter().any(|(command, _)| command == other),
            "{other} never ran: {calls:?}"
        );
    }
    for (command, dir) in &calls {
        assert_eq!(
            dir,
            temp.path(),
            "{command} ran outside the package directory"
        );
    }
}

#[tokio::test]
async fn check_finds_a_relative_path_in_a_package_directory_with_a_quote_and_a_space() {
    let temp = TempDir::new().unwrap();
    let package_dir = quoted_package_dir(&temp);
    write_spec(
        &package_dir,
        "    install: \"true\"\n    check: \"test -f ./here.marker\"\n",
    );

    let events = collect_events(
        service(&package_dir, &package_dir, real_runner())
            .check("pkg")
            .await,
    )
    .await;

    let verdict = events
        .iter()
        .find_map(|e| match e {
            PackageEvent::CheckResultCompleted { check_result, .. } => Some(&check_result.result),
            _ => None,
        })
        .expect("the check should report a result");
    assert!(
        matches!(verdict, CheckResult::Success { .. }),
        "{verdict:?}"
    );
}

#[tokio::test]
async fn status_reports_a_package_and_its_dependency_installed_from_a_relative_check() {
    let temp = TempDir::new().unwrap();
    let package_dir = quoted_package_dir(&temp);
    write_spec(
        &package_dir,
        "    install: \"true\"\n    check: \"test -f ./here.marker\"\n    dependencies:\n      - dep\n",
    );
    std::fs::write(
        package_dir.join("dep.yml"),
        "name: dep\nenvironments:\n  test:\n    install: \"true\"\n    check: \"test -f ./here.marker\"\n",
    )
    .unwrap();

    let events = collect_events(
        service(&package_dir, &package_dir, real_runner())
            .status("pkg")
            .await,
    )
    .await;

    let status = events
        .iter()
        .find_map(|e| match e {
            PackageEvent::EnvironmentStatusChecked {
                environment_status, ..
            } => Some(environment_status),
            _ => None,
        })
        .expect("status should report the environment");
    assert!(
        matches!(status.status, Some(EnvironmentStatus::Installed)),
        "{status:?}"
    );
    assert!(
        matches!(
            status.dependency_statuses.as_slice(),
            [dep] if matches!(dep.status, EnvironmentStatus::Installed)
        ),
        "{status:?}"
    );
}

#[tokio::test]
async fn audit_and_audit_all_are_asked_to_run_in_the_package_directory() {
    let temp = TempDir::new().unwrap();
    write_spec(
        temp.path(),
        &format!("    install: \"true\"\n    audit: \"{AUDIT_CMD}\"\n"),
    );
    let runner = FakeCommandRunner::new().succeeding(AUDIT_CMD, b"pkg\n");
    let service = || service(temp.path(), temp.path(), runner.clone());

    collect_events(service().audit("pkg").await).await;
    collect_events(service().audit_all().await).await;

    let calls = runner.calls();
    assert_eq!(
        calls,
        vec![
            (AUDIT_CMD.to_string(), temp.path().to_path_buf()),
            (AUDIT_CMD.to_string(), temp.path().to_path_buf()),
        ]
    );
}

#[tokio::test]
async fn audit_finds_a_relative_path_in_a_package_directory_with_a_quote_and_a_space() {
    let temp = TempDir::new().unwrap();
    let package_dir = quoted_package_dir(&temp);
    // Prints the package's own name as its source only when it finds the marker.
    write_spec(
        &package_dir,
        "    install: \"true\"\n    audit: \"test -f ./here.marker && echo pkg\"\n",
    );

    let events = collect_events(
        service(&package_dir, &package_dir, real_runner())
            .audit("pkg")
            .await,
    )
    .await;

    let result = events
        .iter()
        .find_map(|e| match e {
            PackageEvent::AuditResultCompleted { audit_result, .. } => Some(&audit_result.result),
            _ => None,
        })
        .expect("the audit should report a result");
    assert!(matches!(result, AuditResult::Clean { .. }), "{result:?}");
}

#[tokio::test]
async fn install_with_a_shell_that_cannot_start_names_the_shell_not_the_command() {
    let temp = TempDir::new().unwrap();
    write_spec(temp.path(), "    install: \"true\"\n");
    let shell = temp.path().join("no-such-shell");
    let runner = ShellCommandRunner::new(&shell.to_string_lossy(), Duration::from_secs(5));

    let events = collect_events(
        service(temp.path(), temp.path(), runner)
            .install("pkg", InstallOptions::default())
            .await,
    )
    .await;

    let OperationResult::Failure(failure) = completed(&events) else {
        panic!("install with no shell must fail: {events:?}");
    };
    let rendered = failure.to_string();
    assert!(
        rendered.contains(&shell.display().to_string()),
        "the failure must name the shell: {rendered}"
    );
    assert!(
        !rendered.to_lowercase().contains("not found"),
        "the install command was never looked for: {rendered}"
    );
}

#[tokio::test]
async fn install_stops_at_a_check_that_cannot_enter_the_package_directory() {
    let temp = TempDir::new().unwrap();
    write_spec(
        temp.path(),
        &format!("    install: \"{INSTALL_CMD}\"\n    check: \"{CHECK_CMD}\"\n"),
    );
    let gone = temp.path().join("gone");
    let unusable = selfie::commands::CommandError::WorkingDirectoryUnusable {
        command: CHECK_CMD.to_string(),
        working_directory: gone.clone(),
        cause: std::sync::Arc::new(std::io::Error::from(std::io::ErrorKind::NotFound)),
    };
    let runner = FakeCommandRunner::new()
        .erroring(CHECK_CMD, unusable)
        .succeeding(INSTALL_CMD, b"");

    let events = collect_events(
        service(temp.path(), temp.path(), runner.clone())
            .install("pkg", InstallOptions::default())
            .await,
    )
    .await;

    let OperationResult::Failure(failure) = completed(&events) else {
        panic!("install must fail: {events:?}");
    };
    assert!(
        failure.to_string().contains(&gone.display().to_string()),
        "the failure must name the directory: {failure}"
    );
    let commands: Vec<String> = runner.calls().into_iter().map(|(c, _)| c).collect();
    assert_eq!(
        commands,
        vec![CHECK_CMD.to_string()],
        "nothing runs after the check"
    );
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, PackageEvent::Warning { .. })),
        "no warning precedes the failure: {events:?}"
    );
}

// A check that cannot enter the package directory is that directory's failure,
// not an invalid check command.
#[tokio::test]
async fn check_names_a_package_directory_it_cannot_enter_not_the_command() {
    let temp = TempDir::new().unwrap();
    write_spec(
        temp.path(),
        &format!("    install: \"true\"\n    check: \"{CHECK_CMD}\"\n"),
    );
    let gone = temp.path().join("gone");
    let unusable = selfie::commands::CommandError::WorkingDirectoryUnusable {
        command: CHECK_CMD.to_string(),
        working_directory: gone.clone(),
        cause: std::sync::Arc::new(std::io::Error::from(std::io::ErrorKind::NotFound)),
    };
    let runner = FakeCommandRunner::new().erroring(CHECK_CMD, unusable);

    let events = collect_events(service(temp.path(), temp.path(), runner).check("pkg").await).await;

    let OperationResult::Failure(failure) = completed(&events) else {
        panic!("check must fail: {events:?}");
    };
    let rendered = failure.to_string();
    assert!(
        rendered.contains(&gone.display().to_string()),
        "the failure must name the directory: {rendered}"
    );
    assert!(
        !rendered.contains("Invalid command"),
        "the check command is not at fault: {rendered}"
    );
}

#[tokio::test]
async fn install_stops_at_a_pre_install_check_that_was_cancelled() {
    let temp = TempDir::new().unwrap();
    write_spec(
        temp.path(),
        &format!("    install: \"{INSTALL_CMD}\"\n    check: \"{CHECK_CMD}\"\n"),
    );
    let cancelled = selfie::commands::CommandError::Cancelled {
        command: CHECK_CMD.to_string(),
        working_directory: temp.path().to_path_buf(),
    };
    let runner = FakeCommandRunner::new()
        .erroring(CHECK_CMD, cancelled)
        .succeeding(INSTALL_CMD, b"");

    let events = collect_events(
        service(temp.path(), temp.path(), runner.clone())
            .install("pkg", InstallOptions::default())
            .await,
    )
    .await;

    assert!(
        matches!(completed(&events), OperationResult::Failure(_)),
        "{events:?}"
    );
    let commands: Vec<String> = runner.calls().into_iter().map(|(c, _)| c).collect();
    assert_eq!(
        commands,
        vec![CHECK_CMD.to_string()],
        "nothing runs after the check"
    );
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, PackageEvent::Warning { .. })),
        "no warning precedes the failure: {events:?}"
    );
}

// An install whose own command moves its package directory away has nothing
// left to verify in: the post-install check cannot start, and the install must
// fail rather than report a success it never verified.
#[tokio::test]
async fn an_install_that_moves_its_own_package_directory_away_fails() {
    let temp = TempDir::new().unwrap();
    let package_dir = temp.path().join("pkgs");
    std::fs::create_dir(&package_dir).unwrap();
    let quote = |path: &Path| {
        shlex::try_quote(path.to_str().unwrap())
            .unwrap()
            .into_owned()
    };
    let install = format!(
        "mv {} {}",
        quote(&package_dir),
        quote(&temp.path().join("pkgs.gone"))
    );
    write_spec(
        &package_dir,
        &format!("    install: \"{install}\"\n    check: \"test -f ./installed\"\n"),
    );

    let events = collect_events(
        service(&package_dir, &package_dir, real_runner())
            .install("pkg", InstallOptions::default())
            .await,
    )
    .await;

    assert!(
        temp.path().join("pkgs.gone").exists(),
        "the install command never ran: {events:?}"
    );
    let OperationResult::Failure(failure) = completed(&events) else {
        panic!("an install with nothing to verify in must fail: {events:?}");
    };
    assert!(
        failure
            .to_string()
            .contains(&package_dir.display().to_string()),
        "the failure must name the directory: {failure}"
    );
}

#[tokio::test]
async fn a_post_install_check_that_runs_and_fails_is_still_a_warning() {
    let temp = TempDir::new().unwrap();
    write_spec(
        temp.path(),
        &format!("    install: \"{INSTALL_CMD}\"\n    check: \"{CHECK_CMD}\"\n"),
    );
    let runner = FakeCommandRunner::new()
        .failing(CHECK_CMD, b"")
        .succeeding(INSTALL_CMD, b"");

    let events = collect_events(
        service(temp.path(), temp.path(), runner)
            .install("pkg", InstallOptions::default())
            .await,
    )
    .await;

    assert!(
        matches!(completed(&events), OperationResult::Success(_)),
        "{events:?}"
    );
    assert!(
        events.iter().any(|e| matches!(
            e,
            PackageEvent::Warning { message, .. } if message.contains("verification failed")
        )),
        "the failed verification must be reported: {events:?}"
    );
}

// A runner that answers as `FakeCommandRunner` does, and refuses every command
// once `token` is cancelled, as the shell runner does.
#[derive(Debug, Clone)]
struct CancelAware {
    inner: FakeCommandRunner,
}

impl CancelAware {
    fn cancelled(command: &str, dir: &Path) -> selfie::commands::CommandError {
        selfie::commands::CommandError::Cancelled {
            command: command.to_string(),
            working_directory: dir.to_path_buf(),
        }
    }
}

impl CommandRunner for CancelAware {
    async fn is_command_available(&self, command: &str) -> bool {
        self.inner.is_command_available(command).await
    }

    async fn execute(
        &self,
        command: &str,
        working_dir: &Path,
        token: &CancellationToken,
    ) -> Result<selfie::commands::CommandOutput, selfie::commands::CommandError> {
        if token.is_cancelled() {
            return Err(Self::cancelled(command, working_dir));
        }
        self.inner.execute(command, working_dir, token).await
    }

    async fn execute_in_dir(
        &self,
        command: &str,
        working_dir: &Path,
        timeout: Duration,
        token: &CancellationToken,
    ) -> Result<selfie::commands::CommandOutput, selfie::commands::CommandError> {
        if token.is_cancelled() {
            return Err(Self::cancelled(command, working_dir));
        }
        self.inner
            .execute_in_dir(command, working_dir, timeout, token)
            .await
    }

    async fn execute_streaming(
        &self,
        command: &str,
        working_dir: &Path,
        timeout: Duration,
        output_sender: tokio::sync::mpsc::Sender<selfie::commands::OutputChunk>,
        token: &CancellationToken,
    ) -> Result<selfie::commands::CommandOutput, selfie::commands::CommandError> {
        if token.is_cancelled() {
            return Err(Self::cancelled(command, working_dir));
        }
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
    ) -> Result<selfie::commands::ContentOutput, selfie::commands::CommandError> {
        self.inner
            .execute_for_content(command, working_dir, timeout, token)
            .await
    }
}

// A Ctrl+C landing while the install command runs cancels the post-install
// check. The install ends as a cancellation: no warning about a check that never
// ran, and nothing runs after it.
#[tokio::test]
async fn a_cancel_before_the_post_install_check_ends_the_install_cancelled() {
    let temp = TempDir::new().unwrap();
    write_spec(
        temp.path(),
        &format!("    install: \"{INSTALL_CMD}\"\n    check: \"{CHECK_CMD}\"\n"),
    );
    let token = CancellationToken::new();
    let runner = CancelAware {
        inner: FakeCommandRunner::new()
            .failing(CHECK_CMD, b"")
            .succeeding(INSTALL_CMD, b"")
            .cancelling(INSTALL_CMD, &token),
    };

    let events = collect_events(
        service_with_token(temp.path(), temp.path(), runner.clone(), token)
            .install("pkg", InstallOptions::default())
            .await,
    )
    .await;

    assert!(
        events
            .iter()
            .any(|e| matches!(e, PackageEvent::Canceled { .. })),
        "a cancelled install must say so: {events:?}"
    );
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, PackageEvent::Completed { .. })),
        "a cancelled install must not also report a result: {events:?}"
    );
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, PackageEvent::Warning { .. })),
        "no warning about a check that never ran: {events:?}"
    );
    let commands: Vec<String> = runner.inner.calls().into_iter().map(|(c, _)| c).collect();
    assert_eq!(
        commands,
        vec![CHECK_CMD.to_string(), INSTALL_CMD.to_string()],
        "nothing runs after the cancel"
    );
    // The runner refuses every command after the cancel, so the install stopping
    // shows in its progress: it must end at the refused check, not carry on to
    // its completion step.
    assert!(
        !events.iter().any(|e| matches!(
            e,
            PackageEvent::Progress { message, .. } if message.contains("installation completed")
        )),
        "the install carried on past the cancelled check: {events:?}"
    );
}
