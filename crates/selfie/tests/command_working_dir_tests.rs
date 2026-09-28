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
        event::{OperationResult, PackageEvent},
        git_adapter::GixGitStatusProvider,
        repository::YamlPackageRepository,
        service::{InstallOptions, PackageService, PackageServiceImpl},
    },
};
use tempfile::TempDir;
use test_common::{FakeCommandRunner, collect_events};
use tokio_util::sync::CancellationToken;

const INSTALL_CMD: &str = "install-pkg";

// A service reading specs from `spec_dir`, with `package_directory` configured as
// `package_dir`. The two are the same directory except in a test that needs the
// package directory to go missing after its spec was read.
fn service<CR: CommandRunner + Clone + std::fmt::Debug + 'static>(
    spec_dir: &Path,
    package_dir: &Path,
    runner: CR,
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
        CancellationToken::new(),
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
