//! Integration tests for the dotfile service layer
//!
//! These tests verify dotfile deployment operations using real filesystem
//! and repository implementations with temporary directories.
//!
//! ## Directory layout
//!
//! Source paths resolve relative to the YAML file's parent directory, so:
//!
//! - Package dotfiles: YAML lives in `packages/`, source files in `packages/<name>/`
//! - Standalone dotfiles: YAML lives in `dotfiles/`, source files in `dotfiles/<name>/`
//!
//! This is why most tests create source files under `dirs.package_dir` — the
//! package YAML files are there, so that's the resolution base.

use std::path::PathBuf;

use futures::StreamExt;
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;

use test_common::FakeCommandRunner;

use selfie::{
    config::SelfieConfigBuilder,
    dotfile_service::{
        port::{ApplyOptions, DotfileService},
        service::DotfileServiceImpl,
    },
    fs::RealFileSystem,
    package::{
        event::{OperationResult, OperationSuccess, PackageEvent},
        repository::YamlPackageRepository,
    },
};

/// Collect all events from an event stream
async fn collect_events(stream: selfie::package::event::EventStream) -> Vec<PackageEvent> {
    stream.collect::<Vec<_>>().await
}

/// Extract the operation result from collected events
fn get_operation_result(events: &[PackageEvent]) -> Option<&OperationResult> {
    events.iter().find_map(|e| match e {
        PackageEvent::Completed { result, .. } => Some(result),
        _ => None,
    })
}

/// Helper to create a package YAML file with a dotfiles section
fn create_package_with_dotfiles(
    package_dir: &std::path::Path,
    name: &str,
    dotfiles: &[(&str, &str)],
) -> PathBuf {
    let mut dotfiles_yaml = String::from("dotfiles:\n");
    for (source, target) in dotfiles {
        dotfiles_yaml.push_str(&format!(
            "  - source: \"{source}\"\n    target: \"{target}\"\n"
        ));
    }

    let yaml = format!(
        r#"name: {name}
environments:
  test:
    install: "echo installed"
{dotfiles_yaml}"#
    );

    let file_path = package_dir.join(format!("{name}.yml"));
    std::fs::write(&file_path, yaml).unwrap();
    file_path
}

/// Create standard test directories under a temp dir
struct TestDirs {
    _temp: TempDir,
    package_dir: PathBuf,
    dotfiles_dir: PathBuf,
    target_dir: PathBuf,
    state_dir: PathBuf,
}

impl TestDirs {
    fn new() -> Self {
        let temp = TempDir::new().unwrap();
        let package_dir = temp.path().join("packages");
        let dotfiles_dir = temp.path().join("dotfiles");
        let target_dir = temp.path().join("target");
        let state_dir = temp.path().join("state");
        std::fs::create_dir_all(&package_dir).unwrap();
        std::fs::create_dir_all(&dotfiles_dir).unwrap();
        std::fs::create_dir_all(&target_dir).unwrap();
        std::fs::create_dir_all(&state_dir).unwrap();
        Self {
            _temp: temp,
            package_dir,
            dotfiles_dir,
            target_dir,
            state_dir,
        }
    }

    /// Create a service backed only by the packages directory.
    fn service(
        &self,
    ) -> DotfileServiceImpl<YamlPackageRepository<RealFileSystem>, RealFileSystem, FakeCommandRunner>
    {
        self.service_with_runner(FakeCommandRunner::new())
    }

    /// A packages-only service whose provider commands answer from `runner`.
    fn service_with_runner(
        &self,
        runner: FakeCommandRunner,
    ) -> DotfileServiceImpl<YamlPackageRepository<RealFileSystem>, RealFileSystem, FakeCommandRunner>
    {
        self.service_with_runner_and_token(runner, CancellationToken::new())
    }

    /// As [`service_with_runner`](Self::service_with_runner), but the caller
    /// supplies the cancellation token and may use any runner type.
    ///
    /// Generic over the runner because the cancellation tests need one that
    /// observes the token it is handed, which `FakeCommandRunner` deliberately
    /// ignores.
    fn service_with_runner_and_token<CR>(
        &self,
        runner: CR,
        token: CancellationToken,
    ) -> DotfileServiceImpl<YamlPackageRepository<RealFileSystem>, RealFileSystem, CR>
    where
        CR: selfie::commands::CommandRunner + Clone + std::fmt::Debug + Send + Sync + 'static,
    {
        let fs = RealFileSystem;
        let config = SelfieConfigBuilder::default()
            .environment("test")
            .package_directory(&self.package_dir)
            .dotfiles_directory(self.dotfiles_dir.clone())
            .state_directory(self.state_dir.clone())
            .build();
        let repo = YamlPackageRepository::new(fs, config.package_directory().clone());
        DotfileServiceImpl::new(repo, fs, runner, config, token)
    }

    /// Create a service backed by both `packages/` and `dotfiles/` directories.
    fn service_with_dotfiles(
        &self,
    ) -> DotfileServiceImpl<YamlPackageRepository<RealFileSystem>, RealFileSystem, FakeCommandRunner>
    {
        let fs = RealFileSystem;
        let config = SelfieConfigBuilder::default()
            .environment("test")
            .package_directory(&self.package_dir)
            .dotfiles_directory(self.dotfiles_dir.clone())
            .state_directory(self.state_dir.clone())
            .build();
        let package_repo = YamlPackageRepository::new(fs, config.package_directory().clone());
        let dotfiles_repo = YamlPackageRepository::new(fs, self.dotfiles_dir.clone());
        DotfileServiceImpl::new(
            package_repo,
            fs,
            FakeCommandRunner::new(),
            config,
            CancellationToken::new(),
        )
        .with_dotfiles_repository(dotfiles_repo)
    }
}

#[tokio::test]
async fn test_apply_all_deploys_new_dotfile() {
    let dirs = TestDirs::new();

    // Create a dotfile source file
    let source_dir = dirs.package_dir.join("myapp");
    std::fs::create_dir_all(&source_dir).unwrap();
    std::fs::write(source_dir.join("config.toml"), "key = \"value\"").unwrap();

    let target_file = dirs.target_dir.join("config.toml");
    create_package_with_dotfiles(
        &dirs.package_dir,
        "myapp",
        &[("myapp/config.toml", target_file.to_str().unwrap())],
    );

    let service = dirs.service();
    let stream = service.apply_all(ApplyOptions::default()).await;
    let events = collect_events(stream).await;

    let has_deploying = events
        .iter()
        .any(|e| matches!(e, PackageEvent::DotfileDeploying { .. }));
    let has_deployed = events
        .iter()
        .any(|e| matches!(e, PackageEvent::DotfileDeployed { .. }));
    assert!(has_deploying, "Should emit DotfileDeploying event");
    assert!(has_deployed, "Should emit DotfileDeployed event");

    assert!(target_file.exists(), "Target file should be created");
    let content = std::fs::read_to_string(&target_file).unwrap();
    assert_eq!(content, "key = \"value\"");

    let result = get_operation_result(&events).expect("Should have a Completed event");
    match result {
        OperationResult::Success(OperationSuccess::DotfilesApplied {
            deployed_count,
            skipped_count,
            conflict_count,
            ..
        }) => {
            assert_eq!(*deployed_count, 1);
            assert_eq!(*skipped_count, 0);
            assert_eq!(*conflict_count, 0);
        }
        other => panic!("Expected DotfilesApplied success, got: {other:?}"),
    }
}

#[tokio::test]
async fn test_apply_all_skips_when_up_to_date() {
    let dirs = TestDirs::new();

    let source_dir = dirs.package_dir.join("myapp");
    std::fs::create_dir_all(&source_dir).unwrap();
    std::fs::write(source_dir.join("config.toml"), "key = \"value\"").unwrap();

    let target_file = dirs.target_dir.join("config.toml");
    create_package_with_dotfiles(
        &dirs.package_dir,
        "myapp",
        &[("myapp/config.toml", target_file.to_str().unwrap())],
    );

    let service = dirs.service();

    // First apply
    let stream = service.apply_all(ApplyOptions::default()).await;
    let _ = collect_events(stream).await;

    // Second apply - should skip
    let stream = service.apply_all(ApplyOptions::default()).await;
    let events = collect_events(stream).await;

    let has_skipped = events
        .iter()
        .any(|e| matches!(e, PackageEvent::DotfileSkipped { .. }));
    assert!(
        has_skipped,
        "Should emit DotfileSkipped event on second apply"
    );

    let result = get_operation_result(&events).expect("Should have a Completed event");
    match result {
        OperationResult::Success(OperationSuccess::DotfilesApplied {
            deployed_count,
            skipped_count,
            ..
        }) => {
            assert_eq!(*deployed_count, 0);
            assert_eq!(*skipped_count, 1);
        }
        other => panic!("Expected DotfilesApplied success, got: {other:?}"),
    }
}

#[tokio::test]
async fn test_apply_dry_run_does_not_write() {
    let dirs = TestDirs::new();

    let source_dir = dirs.package_dir.join("myapp");
    std::fs::create_dir_all(&source_dir).unwrap();
    std::fs::write(source_dir.join("config.toml"), "key = \"value\"").unwrap();

    let target_file = dirs.target_dir.join("config.toml");
    create_package_with_dotfiles(
        &dirs.package_dir,
        "myapp",
        &[("myapp/config.toml", target_file.to_str().unwrap())],
    );

    let service = dirs.service();
    let options = ApplyOptions {
        dry_run: true,
        ..Default::default()
    };
    let stream = service.apply_all(options).await;
    let events = collect_events(stream).await;

    let has_skipped_dry_run = events
        .iter()
        .any(|e| matches!(e, PackageEvent::DotfileSkipped { reason, .. } if reason == "dry run"));
    assert!(
        has_skipped_dry_run,
        "Should emit DotfileSkipped with 'dry run' reason"
    );

    let has_deploying = events
        .iter()
        .any(|e| matches!(e, PackageEvent::DotfileDeploying { .. }));
    assert!(
        !has_deploying,
        "Should NOT emit DotfileDeploying in dry run"
    );

    assert!(
        !target_file.exists(),
        "Target file should NOT exist in dry run"
    );

    // Verify completion counts: deployed should be 0, skipped should include the dry-run skip
    let result = get_operation_result(&events).expect("Should have a Completed event");
    match result {
        OperationResult::Success(OperationSuccess::DotfilesApplied {
            deployed_count,
            skipped_count,
            ..
        }) => {
            assert_eq!(*deployed_count, 0, "deployed_count should be 0 in dry run");
            assert_eq!(
                *skipped_count, 1,
                "skipped_count should include the dry-run skip"
            );
        }
        other => panic!("Expected DotfilesApplied success, got: {other:?}"),
    }
}

#[tokio::test]
async fn test_apply_specific_package() {
    let dirs = TestDirs::new();

    let source_dir_a = dirs.package_dir.join("app-a");
    let source_dir_b = dirs.package_dir.join("app-b");
    std::fs::create_dir_all(&source_dir_a).unwrap();
    std::fs::create_dir_all(&source_dir_b).unwrap();
    std::fs::write(source_dir_a.join("a.conf"), "config-a").unwrap();
    std::fs::write(source_dir_b.join("b.conf"), "config-b").unwrap();

    let target_a = dirs.target_dir.join("a.conf");
    let target_b = dirs.target_dir.join("b.conf");

    create_package_with_dotfiles(
        &dirs.package_dir,
        "app-a",
        &[("app-a/a.conf", target_a.to_str().unwrap())],
    );
    create_package_with_dotfiles(
        &dirs.package_dir,
        "app-b",
        &[("app-b/b.conf", target_b.to_str().unwrap())],
    );

    let service = dirs.service();
    let stream = service.apply("app-a", ApplyOptions::default()).await;
    let events = collect_events(stream).await;

    let result = get_operation_result(&events).expect("Should have a Completed event");
    match result {
        OperationResult::Success(OperationSuccess::DotfilesApplied { deployed_count, .. }) => {
            assert_eq!(*deployed_count, 1);
        }
        other => panic!("Expected DotfilesApplied success, got: {other:?}"),
    }

    assert!(target_a.exists(), "app-a dotfile should be deployed");
    assert!(!target_b.exists(), "app-b dotfile should NOT be deployed");
}

#[tokio::test]
async fn test_apply_conflict_detected() {
    let dirs = TestDirs::new();

    let source_dir = dirs.package_dir.join("myapp");
    std::fs::create_dir_all(&source_dir).unwrap();
    std::fs::write(source_dir.join("config.toml"), "key = \"new-value\"").unwrap();

    let target_file = dirs.target_dir.join("config.toml");
    create_package_with_dotfiles(
        &dirs.package_dir,
        "myapp",
        &[("myapp/config.toml", target_file.to_str().unwrap())],
    );

    let service = dirs.service();

    // First deploy
    let stream = service.apply_all(ApplyOptions::default()).await;
    let _ = collect_events(stream).await;

    // Modify target and source
    std::fs::write(&target_file, "key = \"user-modified\"").unwrap();
    std::fs::write(source_dir.join("config.toml"), "key = \"updated-source\"").unwrap();

    // Apply again without auto_accept
    let stream = service.apply_all(ApplyOptions::default()).await;
    let events = collect_events(stream).await;

    let has_conflict = events
        .iter()
        .any(|e| matches!(e, PackageEvent::DotfileConflict { .. }));
    assert!(has_conflict, "Should emit DotfileConflict event");

    let content = std::fs::read_to_string(&target_file).unwrap();
    assert_eq!(content, "key = \"user-modified\"");
}

#[tokio::test]
async fn test_apply_conflict_auto_accept() {
    let dirs = TestDirs::new();

    let source_dir = dirs.package_dir.join("myapp");
    std::fs::create_dir_all(&source_dir).unwrap();
    std::fs::write(source_dir.join("config.toml"), "key = \"original\"").unwrap();

    let target_file = dirs.target_dir.join("config.toml");
    create_package_with_dotfiles(
        &dirs.package_dir,
        "myapp",
        &[("myapp/config.toml", target_file.to_str().unwrap())],
    );

    let service = dirs.service();

    // First deploy
    let stream = service.apply_all(ApplyOptions::default()).await;
    let _ = collect_events(stream).await;

    // Modify target and source
    std::fs::write(&target_file, "key = \"user-modified\"").unwrap();
    std::fs::write(source_dir.join("config.toml"), "key = \"updated-source\"").unwrap();

    // Apply with auto_accept
    let options = ApplyOptions {
        auto_accept: true,
        ..Default::default()
    };
    let stream = service.apply_all(options).await;
    let events = collect_events(stream).await;

    let has_deployed = events
        .iter()
        .any(|e| matches!(e, PackageEvent::DotfileDeployed { .. }));
    assert!(has_deployed, "Should deploy with auto_accept");

    let content = std::fs::read_to_string(&target_file).unwrap();
    assert_eq!(content, "key = \"updated-source\"");
}

#[tokio::test]
async fn test_apply_conflict_resolver_accept() {
    use selfie::dotfile_service::port::{ConflictDetail, ConflictResolution, ConflictResolver};
    use std::sync::Arc;

    /// A test resolver that always accepts conflicts.
    struct AlwaysAccept;
    impl ConflictResolver for AlwaysAccept {
        fn resolve(&self, _target: &str, _detail: ConflictDetail<'_>) -> ConflictResolution {
            ConflictResolution::Accept
        }
    }

    let dirs = TestDirs::new();

    let source_dir = dirs.package_dir.join("myapp");
    std::fs::create_dir_all(&source_dir).unwrap();
    std::fs::write(source_dir.join("config.toml"), "key = \"original\"").unwrap();

    let target_file = dirs.target_dir.join("config.toml");
    create_package_with_dotfiles(
        &dirs.package_dir,
        "myapp",
        &[("myapp/config.toml", target_file.to_str().unwrap())],
    );

    let service = dirs.service();

    // First deploy
    let stream = service.apply_all(ApplyOptions::default()).await;
    let _ = collect_events(stream).await;

    // Modify target and source to create a conflict
    std::fs::write(&target_file, "key = \"user-modified\"").unwrap();
    std::fs::write(source_dir.join("config.toml"), "key = \"updated-source\"").unwrap();

    // Apply with a resolver that accepts
    let options = ApplyOptions {
        conflict_resolver: Some(Arc::new(AlwaysAccept)),
        ..Default::default()
    };
    let stream = service.apply_all(options).await;
    let events = collect_events(stream).await;

    // Should deploy (not emit a conflict event)
    let has_conflict = events
        .iter()
        .any(|e| matches!(e, PackageEvent::DotfileConflict { .. }));
    assert!(
        !has_conflict,
        "Should NOT emit DotfileConflict when resolver accepts"
    );

    let has_deployed = events
        .iter()
        .any(|e| matches!(e, PackageEvent::DotfileDeployed { .. }));
    assert!(has_deployed, "Should deploy when resolver accepts");

    let content = std::fs::read_to_string(&target_file).unwrap();
    assert_eq!(content, "key = \"updated-source\"");
}

#[tokio::test]
async fn test_apply_conflict_resolver_skip() {
    use selfie::dotfile_service::port::{ConflictDetail, ConflictResolution, ConflictResolver};
    use std::sync::Arc;

    /// A test resolver that always skips conflicts.
    struct AlwaysSkip;
    impl ConflictResolver for AlwaysSkip {
        fn resolve(&self, _target: &str, _detail: ConflictDetail<'_>) -> ConflictResolution {
            ConflictResolution::Skip
        }
    }

    let dirs = TestDirs::new();

    let source_dir = dirs.package_dir.join("myapp");
    std::fs::create_dir_all(&source_dir).unwrap();
    std::fs::write(source_dir.join("config.toml"), "key = \"original\"").unwrap();

    let target_file = dirs.target_dir.join("config.toml");
    create_package_with_dotfiles(
        &dirs.package_dir,
        "myapp",
        &[("myapp/config.toml", target_file.to_str().unwrap())],
    );

    let service = dirs.service();

    // First deploy
    let stream = service.apply_all(ApplyOptions::default()).await;
    let _ = collect_events(stream).await;

    // Modify target and source to create a conflict
    std::fs::write(&target_file, "key = \"user-modified\"").unwrap();
    std::fs::write(source_dir.join("config.toml"), "key = \"updated-source\"").unwrap();

    // Apply with a resolver that skips
    let options = ApplyOptions {
        conflict_resolver: Some(Arc::new(AlwaysSkip)),
        ..Default::default()
    };
    let stream = service.apply_all(options).await;
    let events = collect_events(stream).await;

    // Should NOT deploy, should emit conflict event (resolver returned Skip)
    let has_deployed = events
        .iter()
        .any(|e| matches!(e, PackageEvent::DotfileDeployed { .. }));
    assert!(!has_deployed, "Should NOT deploy when resolver skips");

    // Target should still have the user's content
    let content = std::fs::read_to_string(&target_file).unwrap();
    assert_eq!(content, "key = \"user-modified\"");
}

#[tokio::test]
async fn test_check_drift_detects_target_change() {
    let dirs = TestDirs::new();

    let source_dir = dirs.package_dir.join("myapp");
    std::fs::create_dir_all(&source_dir).unwrap();
    std::fs::write(source_dir.join("config.toml"), "key = \"value\"").unwrap();

    let target_file = dirs.target_dir.join("config.toml");
    create_package_with_dotfiles(
        &dirs.package_dir,
        "myapp",
        &[("myapp/config.toml", target_file.to_str().unwrap())],
    );

    let service = dirs.service();

    // First deploy
    let stream = service.apply_all(ApplyOptions::default()).await;
    let _ = collect_events(stream).await;

    // Modify target externally
    std::fs::write(&target_file, "key = \"modified\"").unwrap();

    // Check drift
    let stream = service.check_drift().await;
    let events = collect_events(stream).await;

    let has_drift = events
        .iter()
        .any(|e| matches!(e, PackageEvent::DotfileDriftDetected { .. }));
    assert!(has_drift, "Should detect drift after target modification");

    let result = get_operation_result(&events).expect("Should have a Completed event");
    match result {
        OperationResult::Success(OperationSuccess::DotfileDriftChecked {
            drift_count,
            total_count,
            ..
        }) => {
            assert_eq!(*drift_count, 1);
            assert_eq!(*total_count, 1);
        }
        other => panic!("Expected DotfileDriftChecked success, got: {other:?}"),
    }
}

#[tokio::test]
async fn test_check_drift_no_drift_when_up_to_date() {
    let dirs = TestDirs::new();

    let source_dir = dirs.package_dir.join("myapp");
    std::fs::create_dir_all(&source_dir).unwrap();
    std::fs::write(source_dir.join("config.toml"), "key = \"value\"").unwrap();

    let target_file = dirs.target_dir.join("config.toml");
    create_package_with_dotfiles(
        &dirs.package_dir,
        "myapp",
        &[("myapp/config.toml", target_file.to_str().unwrap())],
    );

    let service = dirs.service();

    // Deploy
    let stream = service.apply_all(ApplyOptions::default()).await;
    let _ = collect_events(stream).await;

    // Check drift - should be clean
    let stream = service.check_drift().await;
    let events = collect_events(stream).await;

    let has_drift = events
        .iter()
        .any(|e| matches!(e, PackageEvent::DotfileDriftDetected { .. }));
    assert!(!has_drift, "Should not detect drift when up to date");

    let result = get_operation_result(&events).expect("Should have a Completed event");
    match result {
        OperationResult::Success(OperationSuccess::DotfileDriftChecked { drift_count, .. }) => {
            assert_eq!(*drift_count, 0);
        }
        other => panic!("Expected DotfileDriftChecked success, got: {other:?}"),
    }
}

#[tokio::test]
async fn test_check_drift_missing_source_emits_warning() {
    let dirs = TestDirs::new();

    let source_dir = dirs.package_dir.join("myapp");
    std::fs::create_dir_all(&source_dir).unwrap();
    std::fs::write(source_dir.join("config.toml"), "key = \"value\"").unwrap();

    let target_file = dirs.target_dir.join("config.toml");
    create_package_with_dotfiles(
        &dirs.package_dir,
        "myapp",
        &[("myapp/config.toml", target_file.to_str().unwrap())],
    );

    let service = dirs.service();

    // First deploy
    let stream = service.apply_all(ApplyOptions::default()).await;
    let _ = collect_events(stream).await;

    // Delete the source file
    std::fs::remove_file(source_dir.join("config.toml")).unwrap();

    // Check drift — should emit a warning about missing source, not panic
    let stream = service.check_drift().await;
    let events = collect_events(stream).await;

    let has_warning = events
        .iter()
        .any(|e| matches!(e, PackageEvent::Warning { .. }));
    assert!(
        has_warning,
        "Should emit a warning when source file is missing during drift check"
    );

    // Should still complete successfully
    let result = get_operation_result(&events).expect("Should have a Completed event");
    assert!(
        matches!(
            result,
            OperationResult::Success(OperationSuccess::DotfileDriftChecked { .. })
        ),
        "Should still complete with DotfileDriftChecked even with missing source"
    );
}

#[tokio::test]
async fn test_apply_all_no_dotfiles_packages() {
    let dirs = TestDirs::new();

    let yaml = r#"name: no-config-pkg
environments:
  test:
    install: "echo installed"
"#;
    std::fs::write(dirs.package_dir.join("no-config-pkg.yml"), yaml).unwrap();

    let service = dirs.service();

    let stream = service.apply_all(ApplyOptions::default()).await;
    let events = collect_events(stream).await;

    let result = get_operation_result(&events).expect("Should have a Completed event");
    match result {
        OperationResult::Success(OperationSuccess::DotfilesApplied {
            deployed_count,
            skipped_count,
            conflict_count,
            ..
        }) => {
            assert_eq!(*deployed_count, 0);
            assert_eq!(*skipped_count, 0);
            assert_eq!(*conflict_count, 0);
        }
        other => panic!("Expected DotfilesApplied success, got: {other:?}"),
    }
}

#[tokio::test]
async fn test_apply_rejects_path_traversal() {
    let dirs = TestDirs::new();

    let target_file = dirs.target_dir.join("secret.txt");

    // Source uses "../" to escape the dotfiles directory — should be caught by
    // validate_source_path's normalize_path logic before any file I/O.
    create_package_with_dotfiles(
        &dirs.package_dir,
        "evil-pkg",
        &[("../../etc/passwd", target_file.to_str().unwrap())],
    );

    let service = dirs.service();
    let stream = service.apply_all(ApplyOptions::default()).await;
    let events = collect_events(stream).await;

    // Should get a warning specifically about path traversal
    let has_traversal_warning = events.iter().any(|e| {
        matches!(e, PackageEvent::Warning { message, .. } if message.contains("escapes YAML base directory"))
    });
    assert!(
        has_traversal_warning,
        "Should emit a warning about path escaping YAML base directory"
    );

    let result = get_operation_result(&events).expect("Should have a Completed event");
    match result {
        OperationResult::Success(OperationSuccess::DotfilesApplied {
            deployed_count,
            skipped_count,
            ..
        }) => {
            assert_eq!(*skipped_count, 1);
            assert_eq!(*deployed_count, 0);
        }
        other => panic!("Expected DotfilesApplied success, got: {other:?}"),
    }
}

#[tokio::test]
async fn test_apply_missing_source_warns_and_skips() {
    let dirs = TestDirs::new();

    let target_file = dirs.target_dir.join("config.toml");
    // Source file "nonexistent/config.toml" does not exist alongside the YAML
    create_package_with_dotfiles(
        &dirs.package_dir,
        "missing-src",
        &[("nonexistent/config.toml", target_file.to_str().unwrap())],
    );

    let service = dirs.service();
    let stream = service.apply_all(ApplyOptions::default()).await;
    let events = collect_events(stream).await;

    let has_warning = events
        .iter()
        .any(|e| matches!(e, PackageEvent::Warning { .. }));
    assert!(has_warning, "Should emit a warning about missing source");

    let result = get_operation_result(&events).expect("Should have a Completed event");
    match result {
        OperationResult::Success(OperationSuccess::DotfilesApplied {
            deployed_count,
            skipped_count,
            ..
        }) => {
            assert_eq!(*skipped_count, 1);
            assert_eq!(*deployed_count, 0);
        }
        other => panic!("Expected DotfilesApplied success, got: {other:?}"),
    }
}

#[tokio::test]
async fn test_apply_source_only_change_redeploys() {
    let dirs = TestDirs::new();

    let source_dir = dirs.package_dir.join("myapp");
    std::fs::create_dir_all(&source_dir).unwrap();
    std::fs::write(source_dir.join("config.toml"), "key = \"original\"").unwrap();

    let target_file = dirs.target_dir.join("config.toml");
    create_package_with_dotfiles(
        &dirs.package_dir,
        "myapp",
        &[("myapp/config.toml", target_file.to_str().unwrap())],
    );

    let service = dirs.service();

    // First deploy
    let stream = service.apply_all(ApplyOptions::default()).await;
    let _ = collect_events(stream).await;

    // Change ONLY the source file (target still matches last deploy)
    std::fs::write(source_dir.join("config.toml"), "key = \"updated\"").unwrap();

    // Apply again — should redeploy (RepoChanged drift), not conflict
    let stream = service.apply_all(ApplyOptions::default()).await;
    let events = collect_events(stream).await;

    let result = get_operation_result(&events).expect("Should have a Completed event");
    match result {
        OperationResult::Success(OperationSuccess::DotfilesApplied {
            deployed_count,
            conflict_count,
            ..
        }) => {
            assert_eq!(*deployed_count, 1);
            assert_eq!(*conflict_count, 0);
        }
        other => panic!("Expected DotfilesApplied success, got: {other:?}"),
    }

    let content = std::fs::read_to_string(&target_file).unwrap();
    assert_eq!(content, "key = \"updated\"");
}

#[tokio::test]
async fn test_apply_nonexistent_package_name() {
    let dirs = TestDirs::new();

    // Create a real package, but we'll apply a non-existent one
    let source_dir = dirs.package_dir.join("myapp");
    std::fs::create_dir_all(&source_dir).unwrap();
    std::fs::write(source_dir.join("config.toml"), "key = \"value\"").unwrap();

    let target_file = dirs.target_dir.join("config.toml");
    create_package_with_dotfiles(
        &dirs.package_dir,
        "myapp",
        &[("myapp/config.toml", target_file.to_str().unwrap())],
    );

    let service = dirs.service();
    let stream = service.apply("no-such-pkg", ApplyOptions::default()).await;
    let events = collect_events(stream).await;

    let result = get_operation_result(&events).expect("Should have a Completed event");
    match result {
        OperationResult::Success(OperationSuccess::DotfilesApplied {
            deployed_count,
            skipped_count,
            conflict_count,
            ..
        }) => {
            assert_eq!(*deployed_count, 0);
            assert_eq!(*skipped_count, 0);
            assert_eq!(*conflict_count, 0);
        }
        other => panic!("Expected DotfilesApplied success, got: {other:?}"),
    }

    assert!(
        !target_file.exists(),
        "Target file should NOT be deployed for non-matching package"
    );
}

#[tokio::test]
async fn test_deploy_state_persists_across_service_instances() {
    let dirs = TestDirs::new();

    let source_dir = dirs.package_dir.join("myapp");
    std::fs::create_dir_all(&source_dir).unwrap();
    std::fs::write(source_dir.join("config.toml"), "key = \"value\"").unwrap();

    let target_file = dirs.target_dir.join("config.toml");
    create_package_with_dotfiles(
        &dirs.package_dir,
        "myapp",
        &[("myapp/config.toml", target_file.to_str().unwrap())],
    );

    // Deploy with first service instance
    let service1 = dirs.service();
    let stream = service1.apply_all(ApplyOptions::default()).await;
    let _ = collect_events(stream).await;

    // Create a FRESH service instance and apply again
    let service2 = dirs.service();
    let stream = service2.apply_all(ApplyOptions::default()).await;
    let events = collect_events(stream).await;

    // Should skip (up to date), proving state was read from disk
    let result = get_operation_result(&events).expect("Should have a Completed event");
    match result {
        OperationResult::Success(OperationSuccess::DotfilesApplied { skipped_count, .. }) => {
            assert_eq!(*skipped_count, 1);
        }
        other => panic!("Expected DotfilesApplied success, got: {other:?}"),
    }
}

/// A state file written before `DeployEntry::target` was removed still loads.
///
/// Every state file on every machine still carries a `target:` key. If it stopped
/// deserializing, `load_deploy_state` would swallow the error and return an empty
/// state, so every entry everywhere would become `NotTracked` at once — a mass
/// re-deploy and a conflict prompt per file, from a field nobody reads.
#[tokio::test]
async fn a_state_file_carrying_the_removed_target_field_still_loads() {
    let dirs = TestDirs::new();

    let source_dir = dirs.package_dir.join("myapp");
    std::fs::create_dir_all(&source_dir).unwrap();
    std::fs::write(source_dir.join("config.toml"), "key = \"value\"").unwrap();

    let target_file = dirs.target_dir.join("config.toml");
    std::fs::write(&target_file, "key = \"value\"").unwrap();
    create_package_with_dotfiles(
        &dirs.package_dir,
        "myapp",
        &[("myapp/config.toml", target_file.to_str().unwrap())],
    );

    let checksum = selfie::dotfile_service::deploy::compute_checksum(b"key = \"value\"");
    std::fs::write(
        dirs.state_dir.join("deploy-state.yml"),
        format!(
            "deployed:\n  myapp/config.toml:\n    target: {}\n    source_checksum: {checksum}\n    \
             deployed_checksum: {checksum}\n    deployed_at: \"2026-01-01T00:00:00+00:00\"\n",
            target_file.display()
        ),
    )
    .unwrap();

    let events = collect_events(dirs.service().check_drift().await).await;

    let result = get_operation_result(&events).expect("Should have a Completed event");
    match result {
        OperationResult::Success(OperationSuccess::DotfileDriftChecked {
            drift_count,
            total_count,
            ..
        }) => {
            assert_eq!(*total_count, 1);
            assert_eq!(
                *drift_count, 0,
                "the old entry was not read, so a tracked file was reported as drifted: {events:?}"
            );
        }
        other => panic!("Expected DotfileDriftChecked success, got: {other:?}"),
    }
}

#[tokio::test]
async fn test_dry_run_does_not_persist_state() {
    let dirs = TestDirs::new();

    let source_dir = dirs.package_dir.join("myapp");
    std::fs::create_dir_all(&source_dir).unwrap();
    std::fs::write(source_dir.join("config.toml"), "key = \"value\"").unwrap();

    let target_file = dirs.target_dir.join("config.toml");
    create_package_with_dotfiles(
        &dirs.package_dir,
        "myapp",
        &[("myapp/config.toml", target_file.to_str().unwrap())],
    );

    // Dry-run apply
    let service1 = dirs.service();
    let options = ApplyOptions {
        dry_run: true,
        ..Default::default()
    };
    let stream = service1.apply_all(options).await;
    let _ = collect_events(stream).await;

    // Create fresh service and do a real apply
    let service2 = dirs.service();
    let stream = service2.apply_all(ApplyOptions::default()).await;
    let events = collect_events(stream).await;

    // Should deploy (not skip), proving dry run didn't write state
    let result = get_operation_result(&events).expect("Should have a Completed event");
    match result {
        OperationResult::Success(OperationSuccess::DotfilesApplied { deployed_count, .. }) => {
            assert_eq!(*deployed_count, 1);
        }
        other => panic!("Expected DotfilesApplied success, got: {other:?}"),
    }
}

#[tokio::test]
async fn test_apply_multiple_dotfiles_in_one_package() {
    let dirs = TestDirs::new();

    let source_dir = dirs.package_dir.join("myapp");
    std::fs::create_dir_all(&source_dir).unwrap();
    std::fs::write(source_dir.join("config.toml"), "key = \"value1\"").unwrap();
    std::fs::write(source_dir.join("settings.yml"), "setting: true").unwrap();

    let target_file1 = dirs.target_dir.join("config.toml");
    let target_file2 = dirs.target_dir.join("settings.yml");
    create_package_with_dotfiles(
        &dirs.package_dir,
        "myapp",
        &[
            ("myapp/config.toml", target_file1.to_str().unwrap()),
            ("myapp/settings.yml", target_file2.to_str().unwrap()),
        ],
    );

    let service = dirs.service();
    let stream = service.apply_all(ApplyOptions::default()).await;
    let events = collect_events(stream).await;

    let result = get_operation_result(&events).expect("Should have a Completed event");
    match result {
        OperationResult::Success(OperationSuccess::DotfilesApplied { deployed_count, .. }) => {
            assert_eq!(*deployed_count, 2);
        }
        other => panic!("Expected DotfilesApplied success, got: {other:?}"),
    }

    assert!(target_file1.exists(), "First dotfile should be deployed");
    assert!(target_file2.exists(), "Second dotfile should be deployed");
}

#[tokio::test]
async fn test_apply_target_parent_dir_is_file() {
    let dirs = TestDirs::new();

    let source_dir = dirs.package_dir.join("myapp");
    std::fs::create_dir_all(&source_dir).unwrap();
    std::fs::write(source_dir.join("config.toml"), "key = \"value\"").unwrap();

    // Create a regular file where the parent directory should be, making it
    // impossible to create the target path (a file can't also be a directory)
    let blocker = dirs.target_dir.join("not-a-dir");
    std::fs::write(&blocker, "I am a file, not a directory").unwrap();

    let target_file = blocker.join("config.toml");
    create_package_with_dotfiles(
        &dirs.package_dir,
        "myapp",
        &[("myapp/config.toml", target_file.to_str().unwrap())],
    );

    let service = dirs.service();
    let stream = service.apply_all(ApplyOptions::default()).await;
    let events = collect_events(stream).await;

    let has_warning = events
        .iter()
        .any(|e| matches!(e, PackageEvent::Warning { .. }));
    assert!(
        has_warning,
        "Should emit a warning about write failure when parent path is a file"
    );

    let result = get_operation_result(&events).expect("Should have a Completed event");
    match result {
        OperationResult::Success(OperationSuccess::DotfilesApplied {
            deployed_count,
            skipped_count,
            ..
        }) => {
            assert_eq!(*skipped_count, 1);
            assert_eq!(*deployed_count, 0);
        }
        other => panic!("Expected DotfilesApplied success, got: {other:?}"),
    }
}

#[tokio::test]
async fn test_apply_corrupt_state_file_recovers() {
    let dirs = TestDirs::new();

    let source_dir = dirs.package_dir.join("myapp");
    std::fs::create_dir_all(&source_dir).unwrap();
    std::fs::write(source_dir.join("config.toml"), "key = \"value\"").unwrap();

    let target_file = dirs.target_dir.join("config.toml");
    create_package_with_dotfiles(
        &dirs.package_dir,
        "myapp",
        &[("myapp/config.toml", target_file.to_str().unwrap())],
    );

    // Write garbage to the state file
    let state_file = dirs.state_dir.join("deploy-state.yml");
    std::fs::write(&state_file, "{{{{not valid yaml!!! garbage $$$").unwrap();

    let service = dirs.service();
    let stream = service.apply_all(ApplyOptions::default()).await;
    let events = collect_events(stream).await;

    let result = get_operation_result(&events).expect("Should have a Completed event");
    match result {
        OperationResult::Success(OperationSuccess::DotfilesApplied { deployed_count, .. }) => {
            assert_eq!(*deployed_count, 1);
        }
        other => panic!("Expected DotfilesApplied success, got: {other:?}"),
    }

    assert!(
        target_file.exists(),
        "Dotfile should be deployed despite corrupt state"
    );
}

#[tokio::test]
async fn test_check_drift_with_no_prior_deploys() {
    let dirs = TestDirs::new();

    let source_dir = dirs.package_dir.join("myapp");
    std::fs::create_dir_all(&source_dir).unwrap();
    std::fs::write(source_dir.join("config.toml"), "key = \"value\"").unwrap();

    let target_file = dirs.target_dir.join("config.toml");
    create_package_with_dotfiles(
        &dirs.package_dir,
        "myapp",
        &[("myapp/config.toml", target_file.to_str().unwrap())],
    );

    // Create target manually (not via selfie) so there's no deploy state
    std::fs::write(&target_file, "key = \"value\"").unwrap();

    let service = dirs.service();
    let stream = service.check_drift().await;
    let events = collect_events(stream).await;

    let has_drift = events
        .iter()
        .any(|e| matches!(e, PackageEvent::DotfileDriftDetected { .. }));
    assert!(
        has_drift,
        "Should detect drift when target exists but wasn't tracked"
    );

    let result = get_operation_result(&events).expect("Should have a Completed event");
    match result {
        OperationResult::Success(OperationSuccess::DotfileDriftChecked { drift_count, .. }) => {
            assert_eq!(*drift_count, 1);
        }
        other => panic!("Expected DotfileDriftChecked success, got: {other:?}"),
    }
}

// ─── Dual-repository tests (packages/ + dotfiles/) ─────────────────────────

#[tokio::test]
async fn test_apply_deploys_from_both_packages_and_dotfiles_dirs() {
    let dirs = TestDirs::new();

    // Package dotfile: YAML + source in packages/
    let pkg_source_dir = dirs.package_dir.join("starship");
    std::fs::create_dir_all(&pkg_source_dir).unwrap();
    std::fs::write(pkg_source_dir.join("starship.toml"), "format = \"bold\"").unwrap();

    let pkg_target = dirs.target_dir.join("starship.toml");
    create_package_with_dotfiles(
        &dirs.package_dir,
        "starship",
        &[("starship/starship.toml", pkg_target.to_str().unwrap())],
    );

    // Standalone dotfile: YAML + source in dotfiles/
    let dot_source_dir = dirs.dotfiles_dir.join("dprint");
    std::fs::create_dir_all(&dot_source_dir).unwrap();
    std::fs::write(dot_source_dir.join("dprint.jsonc"), "{\"lineWidth\": 80}").unwrap();

    let dot_target = dirs.target_dir.join("dprint.jsonc");
    create_package_with_dotfiles(
        &dirs.dotfiles_dir,
        "dprint",
        &[("dprint/dprint.jsonc", dot_target.to_str().unwrap())],
    );

    // Use the dual-repo service
    let service = dirs.service_with_dotfiles();
    let stream = service.apply_all(ApplyOptions::default()).await;
    let events = collect_events(stream).await;

    let result = get_operation_result(&events).expect("Should have a Completed event");
    match result {
        OperationResult::Success(OperationSuccess::DotfilesApplied { deployed_count, .. }) => {
            assert_eq!(*deployed_count, 2, "Should deploy from both repos");
        }
        other => panic!("Expected DotfilesApplied success, got: {other:?}"),
    }

    assert!(pkg_target.exists(), "Package dotfile should be deployed");
    assert!(dot_target.exists(), "Standalone dotfile should be deployed");
    assert_eq!(
        std::fs::read_to_string(&pkg_target).unwrap(),
        "format = \"bold\""
    );
    assert_eq!(
        std::fs::read_to_string(&dot_target).unwrap(),
        "{\"lineWidth\": 80}"
    );
}

#[tokio::test]
async fn test_apply_specific_name_finds_standalone_dotfile() {
    let dirs = TestDirs::new();

    // Only a standalone dotfile in dotfiles/, nothing in packages/
    let dot_source_dir = dirs.dotfiles_dir.join("dprint");
    std::fs::create_dir_all(&dot_source_dir).unwrap();
    std::fs::write(dot_source_dir.join("dprint.jsonc"), "{\"lineWidth\": 80}").unwrap();

    let dot_target = dirs.target_dir.join("dprint.jsonc");
    create_package_with_dotfiles(
        &dirs.dotfiles_dir,
        "dprint",
        &[("dprint/dprint.jsonc", dot_target.to_str().unwrap())],
    );

    let service = dirs.service_with_dotfiles();
    let stream = service.apply("dprint", ApplyOptions::default()).await;
    let events = collect_events(stream).await;

    let result = get_operation_result(&events).expect("Should have a Completed event");
    match result {
        OperationResult::Success(OperationSuccess::DotfilesApplied { deployed_count, .. }) => {
            assert_eq!(*deployed_count, 1);
        }
        other => panic!("Expected DotfilesApplied success, got: {other:?}"),
    }

    assert!(dot_target.exists(), "Standalone dotfile should be deployed");
}

#[tokio::test]
async fn test_check_drift_covers_standalone_dotfiles() {
    let dirs = TestDirs::new();

    // Standalone dotfile in dotfiles/
    let dot_source_dir = dirs.dotfiles_dir.join("dprint");
    std::fs::create_dir_all(&dot_source_dir).unwrap();
    std::fs::write(dot_source_dir.join("dprint.jsonc"), "{\"lineWidth\": 80}").unwrap();

    let dot_target = dirs.target_dir.join("dprint.jsonc");
    create_package_with_dotfiles(
        &dirs.dotfiles_dir,
        "dprint",
        &[("dprint/dprint.jsonc", dot_target.to_str().unwrap())],
    );

    let service = dirs.service_with_dotfiles();

    // Deploy first
    let stream = service.apply_all(ApplyOptions::default()).await;
    let _ = collect_events(stream).await;

    // Modify the target externally
    std::fs::write(&dot_target, "{\"lineWidth\": 120}").unwrap();

    // Drift check should detect the standalone dotfile change
    let stream = service.check_drift().await;
    let events = collect_events(stream).await;

    let has_drift = events
        .iter()
        .any(|e| matches!(e, PackageEvent::DotfileDriftDetected { .. }));
    assert!(
        has_drift,
        "Should detect drift in standalone dotfile after target modification"
    );

    let result = get_operation_result(&events).expect("Should have a Completed event");
    match result {
        OperationResult::Success(OperationSuccess::DotfileDriftChecked { drift_count, .. }) => {
            assert_eq!(*drift_count, 1);
        }
        other => panic!("Expected DotfileDriftChecked success, got: {other:?}"),
    }
}

// ───────────────────────────────── Track tests ─────────────────────────────────

#[tokio::test]
async fn test_track_standalone_creates_spec_and_copies_file() {
    let dirs = TestDirs::new();

    // Create a "target" file to track (simulating ~/.config/starship.toml)
    let target_file = dirs.target_dir.join("starship.toml");
    std::fs::write(&target_file, "format = \"$all\"").unwrap();

    let service = dirs.service_with_dotfiles();
    let stream = service
        .track_standalone("starship", target_file.to_str().unwrap())
        .await;
    let events = collect_events(stream).await;

    let result = get_operation_result(&events).expect("Should have a Completed event");
    match result {
        OperationResult::Success(OperationSuccess::DotfileTracked { name, .. }) => {
            assert_eq!(name, "starship");
        }
        other => panic!("Expected DotfileTracked success, got: {other:?}"),
    }

    // Source file should be copied into dotfiles_dir/starship/starship.toml
    let copied = dirs.dotfiles_dir.join("starship").join("starship.toml");
    assert!(
        copied.exists(),
        "Source file should be copied to dotfiles dir"
    );
    assert_eq!(
        std::fs::read_to_string(&copied).unwrap(),
        "format = \"$all\""
    );

    // YAML spec should be created at dotfiles_dir/starship.yml
    let spec = dirs.dotfiles_dir.join("starship.yml");
    assert!(spec.exists(), "YAML spec should be created");
    let spec_content = std::fs::read_to_string(&spec).unwrap();
    assert!(
        spec_content.contains("starship/starship.toml"),
        "Spec should reference the source file with subdirectory"
    );
}

#[tokio::test]
async fn test_track_standalone_fails_when_target_missing() {
    let dirs = TestDirs::new();
    let service = dirs.service_with_dotfiles();

    let stream = service
        .track_standalone("missing", "/nonexistent/file.toml")
        .await;
    let events = collect_events(stream).await;

    let result = get_operation_result(&events).expect("Should have a Completed event");
    assert!(
        matches!(result, OperationResult::Failure(_)),
        "Should fail when target file doesn't exist"
    );
}

#[tokio::test]
async fn test_track_for_package_adds_dotfile_to_existing_package() {
    let dirs = TestDirs::new();

    // Create an existing package without dotfiles
    let yaml = r#"name: alacritty
environments:
  test:
    install: "echo installed"
"#;
    std::fs::write(dirs.package_dir.join("alacritty.yml"), yaml).unwrap();

    // Create a "target" file to track
    let target_file = dirs.target_dir.join("alacritty.toml");
    std::fs::write(&target_file, "[font]\nsize = 12").unwrap();

    let service = dirs.service();
    let stream = service
        .track_for_package("alacritty", target_file.to_str().unwrap())
        .await;
    let events = collect_events(stream).await;

    let result = get_operation_result(&events).expect("Should have a Completed event");
    match result {
        OperationResult::Success(OperationSuccess::DotfileTracked { name, .. }) => {
            assert_eq!(name, "alacritty");
        }
        other => panic!("Expected DotfileTracked success, got: {other:?}"),
    }

    // Source file should be copied into a subdirectory named after the package
    let copied = dirs.package_dir.join("alacritty").join("alacritty.toml");
    assert!(
        copied.exists(),
        "Source file should be copied into package subdirectory"
    );
    assert_eq!(
        std::fs::read_to_string(&copied).unwrap(),
        "[font]\nsize = 12"
    );

    // Package YAML should now contain a dotfiles section with relative source path
    let updated_yaml = std::fs::read_to_string(dirs.package_dir.join("alacritty.yml")).unwrap();
    assert!(
        updated_yaml.contains("dotfiles"),
        "Updated YAML should contain dotfiles section"
    );
    assert!(
        updated_yaml.contains("alacritty/alacritty.toml"),
        "Updated YAML should reference the tracked file with subdirectory"
    );
}

#[tokio::test]
async fn test_track_for_package_fails_when_package_not_found() {
    let dirs = TestDirs::new();

    let target_file = dirs.target_dir.join("some.conf");
    std::fs::write(&target_file, "content").unwrap();

    let service = dirs.service();
    let stream = service
        .track_for_package("nonexistent", target_file.to_str().unwrap())
        .await;
    let events = collect_events(stream).await;

    let result = get_operation_result(&events).expect("Should have a Completed event");
    assert!(
        matches!(result, OperationResult::Failure(_)),
        "Should fail when package doesn't exist"
    );
}

// ─── Secret-bearing dotfiles ────────────────────────────────────────────────
//
// Content that comes from a command, or from a template with `vars`, is resolved
// at apply time, compared in memory, and never recorded. See ADR-0003.

mod secret_bearing {
    use super::*;
    use selfie::dotfile_service::port::{ConflictDetail, ConflictResolution, ConflictResolver};
    use std::sync::{Arc, Mutex};

    /// A value distinctive enough that finding it anywhere is unambiguous.
    const SECRET: &str = "s3cr3t-v4lue-DO-NOT-LEAK";

    /// Write a package whose single dotfile is a whole-file provider entry.
    fn provider_package(package_dir: &std::path::Path, target: &str, command: &str) {
        let yaml = format!(
            "name: creds\nenvironments:\n  test:\n    install: \"echo i\"\ndotfiles:\n  \
             - command: \"{command}\"\n    target: \"{target}\"\n"
        );
        std::fs::write(package_dir.join("creds.yml"), yaml).unwrap();
    }

    /// Write a package whose single dotfile is a template, plus the template.
    fn template_package(
        package_dir: &std::path::Path,
        target: &str,
        template_body: &str,
        vars: &[(&str, &str)],
    ) {
        std::fs::create_dir_all(package_dir.join("creds")).unwrap();
        std::fs::write(package_dir.join("creds/credentials.tpl"), template_body).unwrap();

        let mut yaml = format!(
            "name: creds\nenvironments:\n  test:\n    install: \"echo i\"\ndotfiles:\n  \
             - source: \"creds/credentials.tpl\"\n    target: \"{target}\"\n    vars:\n"
        );
        for (name, command) in vars {
            yaml.push_str(&format!("      {name}: \"{command}\"\n"));
        }
        std::fs::write(package_dir.join("creds.yml"), yaml).unwrap();
    }

    /// Assert that no event reproduces `secret`, in any rendering.
    ///
    /// Scans every event and every field rather than one variant's diff: a leak
    /// added to a warning, or to a newly introduced field, has to fail this too.
    /// *Which* renderings count is `test_common::secrets`' decision, not this
    /// call site's — see that module for what they are and what they miss.
    #[track_caller]
    fn assert_no_event_mentions(events: &[PackageEvent], secret: impl AsRef<[u8]>) {
        let secret = secret.as_ref();
        for event in events {
            test_common::assert_secret_free(&format!("{event:?}"), secret, "an event");
        }
    }

    /// A resolver that accepts every conflict.
    ///
    /// Secret-bearing entries ignore `auto_accept`, so a test needing the
    /// overwrite path has to go through a resolver — the same route a human at a
    /// terminal takes.
    struct AlwaysAcceptSecret;

    impl ConflictResolver for AlwaysAcceptSecret {
        fn resolve(&self, _target: &str, _detail: ConflictDetail<'_>) -> ConflictResolution {
            ConflictResolution::Accept
        }
    }

    fn accepting() -> ApplyOptions {
        ApplyOptions {
            conflict_resolver: Some(Arc::new(AlwaysAcceptSecret)),
            ..Default::default()
        }
    }

    fn state_file(dirs: &TestDirs) -> PathBuf {
        dirs.state_dir.join("deploy-state.yml")
    }

    #[tokio::test]
    async fn provider_content_is_deployed_to_an_absent_target() {
        let dirs = TestDirs::new();
        let target = dirs.target_dir.join("credentials");
        provider_package(&dirs.package_dir, target.to_str().unwrap(), "op read x");

        let runner = FakeCommandRunner::new().succeeding("op read x", SECRET.as_bytes());
        let service = dirs.service_with_runner(runner);

        let events = collect_events(service.apply_all(ApplyOptions::default()).await).await;

        assert_eq!(std::fs::read_to_string(&target).unwrap(), SECRET);
        assert!(
            events
                .iter()
                .any(|e| matches!(e, PackageEvent::DotfileDeployed { .. })),
        );
    }

    // ---- A provider or binding whose output could not be read (selfie-ql8m) ----
    //
    // The harm is not "resolve returns an error" — it is a truncated credential
    // reaching a file. A short read used to be indistinguishable from a short
    // command: the `MAX_CONTENT_BYTES` cap is a maximum, so a prefix sails under
    // it, and for a var binding a prefix is also still non-empty. So these assert
    // the filesystem, and each has a control proving the same fixture *does*
    // write when the read succeeds.

    #[tokio::test]
    async fn a_provider_whose_output_could_not_be_read_writes_no_target() {
        let dirs = TestDirs::new();
        let target = dirs.target_dir.join("credentials");
        provider_package(&dirs.package_dir, target.to_str().unwrap(), "op read x");

        let runner = FakeCommandRunner::new().stdout_read_failing("op read x");
        let service = dirs.service_with_runner(runner);

        let _ = collect_events(service.apply_all(ApplyOptions::default()).await).await;

        assert!(
            !target.exists(),
            "a truncated credential was written to {}",
            target.display()
        );
    }

    #[tokio::test]
    async fn a_provider_whose_output_could_not_be_read_leaves_a_pre_existing_target_intact() {
        let dirs = TestDirs::new();
        use std::os::unix::fs::PermissionsExt as _;

        let target = dirs.target_dir.join("credentials");
        std::fs::write(&target, "previous-credential-abc123").unwrap();
        let mode_before = std::fs::metadata(&target).unwrap().permissions().mode();
        provider_package(&dirs.package_dir, target.to_str().unwrap(), "op read x");

        let runner = FakeCommandRunner::new().stdout_read_failing("op read x");
        let service = dirs.service_with_runner(runner);

        // `accepting()`, not `ApplyOptions::default()`. Without a resolver a
        // secret-bearing entry whose target differs is reported as a conflict and
        // skipped, so the target would survive for a reason that has nothing to
        // do with the read — and mutating `run_capture` to return partial content
        // left this test passing. It has to take the overwrite path for the
        // assertion below to mean anything.
        let _ = collect_events(service.apply_all(accepting()).await).await;

        assert_eq!(
            std::fs::read(&target).unwrap(),
            b"previous-credential-abc123",
            "a working credential was replaced with a truncated one"
        );
        assert_eq!(
            std::fs::metadata(&target).unwrap().permissions().mode(),
            mode_before,
            "the target's mode changed despite nothing being deployed"
        );
    }

    #[tokio::test]
    async fn the_same_provider_fixture_does_write_when_the_read_succeeds() {
        // Control for both tests above. Without it they pass if apply never
        // reached the entry at all — a broken fixture would look like a fix.
        let dirs = TestDirs::new();
        let target = dirs.target_dir.join("credentials");
        std::fs::write(&target, "previous-credential-abc123").unwrap();
        provider_package(&dirs.package_dir, target.to_str().unwrap(), "op read x");

        let runner = FakeCommandRunner::new().succeeding("op read x", SECRET.as_bytes());
        let service = dirs.service_with_runner(runner);

        let _ = collect_events(service.apply_all(accepting()).await).await;

        assert_eq!(std::fs::read_to_string(&target).unwrap(), SECRET);
    }

    #[tokio::test]
    async fn a_binding_whose_output_could_not_be_read_writes_no_target() {
        // The second write path. A truncated binding is spliced into a rendered
        // file, and neither the emptiness check nor the size cap can see it.
        let dirs = TestDirs::new();
        let target = dirs.target_dir.join("credentials");
        template_package(
            &dirs.package_dir,
            target.to_str().unwrap(),
            "key: {{ api_key }}\n",
            &[("api_key", "op read x")],
        );

        let runner = FakeCommandRunner::new().stdout_read_failing("op read x");
        let service = dirs.service_with_runner(runner);

        let _ = collect_events(service.apply_all(ApplyOptions::default()).await).await;

        assert!(
            !target.exists(),
            "a file holding a truncated credential was written to {}",
            target.display()
        );
    }

    #[tokio::test]
    async fn a_binding_whose_output_could_not_be_read_leaves_a_pre_existing_target_intact() {
        let dirs = TestDirs::new();
        let target = dirs.target_dir.join("credentials");
        std::fs::write(&target, "key: previous-credential-abc123\n").unwrap();
        template_package(
            &dirs.package_dir,
            target.to_str().unwrap(),
            "key: {{ api_key }}\n",
            &[("api_key", "op read x")],
        );

        let runner = FakeCommandRunner::new().stdout_read_failing("op read x");
        let service = dirs.service_with_runner(runner);

        // `accepting()` for the same reason as the provider case above: without a
        // resolver this would be skipped as a conflict and prove nothing.
        let _ = collect_events(service.apply_all(accepting()).await).await;

        assert_eq!(
            std::fs::read_to_string(&target).unwrap(),
            "key: previous-credential-abc123\n",
            "a working credential was replaced with a truncated one"
        );
    }

    #[tokio::test]
    async fn the_same_binding_fixture_does_write_when_the_read_succeeds() {
        // Control for the two binding tests above. Seeded and `accepting()` so it
        // exercises the same overwrite path the pre-existing-target test does —
        // a control taking a different path would not control anything.
        let dirs = TestDirs::new();
        let target = dirs.target_dir.join("credentials");
        std::fs::write(&target, "key: previous-credential-abc123\n").unwrap();
        template_package(
            &dirs.package_dir,
            target.to_str().unwrap(),
            "key: {{ api_key }}\n",
            &[("api_key", "op read x")],
        );

        let runner = FakeCommandRunner::new().succeeding("op read x", SECRET.as_bytes());
        let service = dirs.service_with_runner(runner);

        let _ = collect_events(service.apply_all(accepting()).await).await;

        assert_eq!(
            std::fs::read_to_string(&target).unwrap(),
            format!("key: {SECRET}\n")
        );
    }

    #[tokio::test]
    async fn provider_commands_run_in_the_package_directory() {
        let dirs = TestDirs::new();
        let target = dirs.target_dir.join("credentials");
        provider_package(&dirs.package_dir, target.to_str().unwrap(), "op read x");

        let runner = FakeCommandRunner::new().succeeding("op read x", b"v");
        let service = dirs.service_with_runner(runner.clone());

        let _ = collect_events(service.apply_all(ApplyOptions::default()).await).await;

        assert_eq!(
            runner.calls(),
            vec![("op read x".to_string(), dirs.package_dir.clone())],
        );
    }

    #[tokio::test]
    async fn an_in_sync_target_is_skipped() {
        let dirs = TestDirs::new();
        let target = dirs.target_dir.join("credentials");
        std::fs::write(&target, SECRET).unwrap();
        provider_package(&dirs.package_dir, target.to_str().unwrap(), "op read x");

        let runner = FakeCommandRunner::new().succeeding("op read x", SECRET.as_bytes());
        let service = dirs.service_with_runner(runner);

        let events = collect_events(service.apply_all(ApplyOptions::default()).await).await;

        // `starts_with`, not equality: a target seeded by `std::fs::write` is
        // 0644, so this also takes the permissions-tightening branch, whose
        // reason extends the same prefix. Both are in-sync skips, which is what
        // this test is about; the two modes are covered separately below.
        let skipped = events.iter().any(|e| {
            matches!(
                e,
                PackageEvent::DotfileSkipped { reason, .. }
                    if reason.starts_with("already in sync")
            )
        });
        assert!(skipped, "expected an in-sync skip, got: {events:?}");
    }

    #[tokio::test]
    async fn a_differing_target_is_a_conflict_and_is_not_overwritten() {
        let dirs = TestDirs::new();
        let target = dirs.target_dir.join("credentials");
        std::fs::write(&target, "hand-edited").unwrap();
        provider_package(&dirs.package_dir, target.to_str().unwrap(), "op read x");

        let runner = FakeCommandRunner::new().succeeding("op read x", SECRET.as_bytes());
        let service = dirs.service_with_runner(runner);

        let events = collect_events(service.apply_all(ApplyOptions::default()).await).await;

        assert!(
            events
                .iter()
                .any(|e| matches!(e, PackageEvent::DotfileConflict { .. })),
        );
        assert_eq!(
            std::fs::read_to_string(&target).unwrap(),
            "hand-edited",
            "a conflict must leave the target alone"
        );
    }

    #[tokio::test]
    async fn a_secret_entry_records_no_deploy_state() {
        let dirs = TestDirs::new();
        let target = dirs.target_dir.join("credentials");
        provider_package(&dirs.package_dir, target.to_str().unwrap(), "op read x");

        let runner = FakeCommandRunner::new().succeeding("op read x", SECRET.as_bytes());
        let service = dirs.service_with_runner(runner);

        let _ = collect_events(service.apply_all(ApplyOptions::default()).await).await;

        // A stored checksum of a credential is a confirmation oracle: ADR-0003.
        let state = std::fs::read_to_string(state_file(&dirs)).unwrap_or_default();
        assert!(
            !state.contains("credentials"),
            "secret-bearing entries must record no deploy state, got: {state}"
        );
    }

    #[tokio::test]
    async fn a_plain_repo_file_entry_still_records_deploy_state() {
        // Proves the existing checksum path is untouched by the secret path.
        let dirs = TestDirs::new();
        let source_dir = dirs.package_dir.join("myapp");
        std::fs::create_dir_all(&source_dir).unwrap();
        std::fs::write(source_dir.join("config.toml"), "key = \"value\"").unwrap();
        let target = dirs.target_dir.join("config.toml");
        create_package_with_dotfiles(
            &dirs.package_dir,
            "myapp",
            &[("myapp/config.toml", target.to_str().unwrap())],
        );

        let service = dirs.service();
        let _ = collect_events(service.apply_all(ApplyOptions::default()).await).await;

        let state = std::fs::read_to_string(state_file(&dirs)).unwrap();
        assert!(state.contains("myapp/config.toml"), "got: {state}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_secret_target_is_written_owner_only_even_over_a_world_readable_file() {
        use std::os::unix::fs::PermissionsExt as _;

        let dirs = TestDirs::new();
        let target = dirs.target_dir.join("credentials");
        std::fs::write(&target, "old").unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o644)).unwrap();
        provider_package(&dirs.package_dir, target.to_str().unwrap(), "op read x");

        let runner = FakeCommandRunner::new().succeeding("op read x", SECRET.as_bytes());
        let service = dirs.service_with_runner(runner);

        let _ = collect_events(service.apply_all(accepting()).await).await;

        let mode = std::fs::metadata(&target).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "credential targets must be owner-only");
        assert_eq!(std::fs::read_to_string(&target).unwrap(), SECRET);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn an_in_sync_target_with_lax_permissions_is_tightened() {
        use std::os::unix::fs::PermissionsExt as _;

        // The adoption path ADR-0003 names as the reason this design is safe: a
        // machine with pre-existing config whose content already matches. Being
        // told "already in sync" while the file stays world-readable would leave
        // the user believing it is managed to the standard the docs promise
        // unconditionally.
        let dirs = TestDirs::new();
        let target = dirs.target_dir.join("credentials");
        std::fs::write(&target, SECRET).unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o644)).unwrap();
        provider_package(&dirs.package_dir, target.to_str().unwrap(), "op read x");

        let runner = FakeCommandRunner::new().succeeding("op read x", SECRET.as_bytes());
        let service = dirs.service_with_runner(runner);

        let events = collect_events(service.apply_all(ApplyOptions::default()).await).await;

        let mode = std::fs::metadata(&target).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode, 0o600,
            "an in-sync target must still end up owner-only"
        );
        assert_eq!(
            std::fs::read_to_string(&target).unwrap(),
            SECRET,
            "content must be unchanged"
        );
        assert!(
            events.iter().any(|e| matches!(
                e,
                PackageEvent::DotfileSkipped { reason, .. } if reason.contains("permissions")
            )),
            "the tightening must be reported rather than done silently: {events:?}"
        );
        assert_no_event_mentions(&events, SECRET);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn an_in_sync_target_readable_only_by_its_group_is_still_tightened() {
        use std::os::unix::fs::PermissionsExt as _;

        // 0640 leaks to the group but not to others, so a check that only looks
        // at the "other" bits would pass it. On a shared machine the group is
        // exactly who you are hiding a credential from.
        let dirs = TestDirs::new();
        let target = dirs.target_dir.join("credentials");
        std::fs::write(&target, SECRET).unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o640)).unwrap();
        provider_package(&dirs.package_dir, target.to_str().unwrap(), "op read x");

        let runner = FakeCommandRunner::new().succeeding("op read x", SECRET.as_bytes());
        let service = dirs.service_with_runner(runner);

        let _ = collect_events(service.apply_all(ApplyOptions::default()).await).await;

        let mode = std::fs::metadata(&target).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "group-readable is not owner-only");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn an_in_sync_target_already_owner_only_is_left_completely_alone() {
        use std::os::unix::fs::MetadataExt as _;
        use std::os::unix::fs::PermissionsExt as _;

        // The counterpart: tightening must be conditional. Rewriting a correct
        // file on every apply would churn the inode and make "already in sync" a
        // lie.
        let dirs = TestDirs::new();
        let target = dirs.target_dir.join("credentials");
        std::fs::write(&target, SECRET).unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o600)).unwrap();
        let before = std::fs::metadata(&target).unwrap().ino();
        provider_package(&dirs.package_dir, target.to_str().unwrap(), "op read x");

        let runner = FakeCommandRunner::new().succeeding("op read x", SECRET.as_bytes());
        let service = dirs.service_with_runner(runner);

        let events = collect_events(service.apply_all(ApplyOptions::default()).await).await;

        assert_eq!(
            std::fs::metadata(&target).unwrap().ino(),
            before,
            "an already-correct target must not be rewritten"
        );
        assert!(
            events.iter().any(|e| matches!(
                e,
                PackageEvent::DotfileSkipped { reason, .. }
                    if reason == "already in sync"
            )),
            "expected a plain in-sync skip, got: {events:?}"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_symlinked_secret_target_is_replaced_not_written_through() {
        let dirs = TestDirs::new();
        let elsewhere = dirs.target_dir.join("elsewhere");
        std::fs::write(&elsewhere, "untouched").unwrap();
        let target = dirs.target_dir.join("credentials");
        std::os::unix::fs::symlink(&elsewhere, &target).unwrap();
        provider_package(&dirs.package_dir, target.to_str().unwrap(), "op read x");

        let runner = FakeCommandRunner::new().succeeding("op read x", SECRET.as_bytes());
        let service = dirs.service_with_runner(runner);

        let _ = collect_events(service.apply_all(accepting()).await).await;

        assert_eq!(
            std::fs::read_to_string(&elsewhere).unwrap(),
            "untouched",
            "the credential must not be written through the link"
        );
        assert!(
            !std::fs::symlink_metadata(&target)
                .unwrap()
                .file_type()
                .is_symlink(),
            "the link itself must be replaced"
        );
        assert_eq!(std::fs::read_to_string(&target).unwrap(), SECRET);
    }

    #[tokio::test]
    async fn non_utf8_provider_output_survives_byte_exact() {
        let dirs = TestDirs::new();
        let target = dirs.target_dir.join("id_ed25519");
        provider_package(&dirs.package_dir, target.to_str().unwrap(), "op read key");

        let bytes = [0x00u8, 0xff, 0xfe, 0x0a];
        let runner = FakeCommandRunner::new().succeeding("op read key", &bytes);
        let service = dirs.service_with_runner(runner);

        let _ = collect_events(service.apply_all(ApplyOptions::default()).await).await;

        assert_eq!(std::fs::read(&target).unwrap(), bytes);
    }

    #[tokio::test]
    async fn a_template_renders_its_bindings() {
        let dirs = TestDirs::new();
        let target = dirs.target_dir.join("credentials");
        template_package(
            &dirs.package_dir,
            target.to_str().unwrap(),
            "key: {{ api_key }}\ncorp: {{ corp }}\n",
            &[("api_key", "op read a"), ("corp", "teller get B")],
        );

        let runner = FakeCommandRunner::new()
            .succeeding("op read a", SECRET.as_bytes())
            .succeeding("teller get B", b"corp-token");
        let service = dirs.service_with_runner(runner);

        let _ = collect_events(service.apply_all(ApplyOptions::default()).await).await;

        assert_eq!(
            std::fs::read_to_string(&target).unwrap(),
            format!("key: {SECRET}\ncorp: corp-token\n")
        );
    }

    #[tokio::test]
    async fn two_secret_entries_do_not_share_values() {
        // Each entry's bindings must be built fresh. A binding map reused across
        // entries would splice one entry's secret into the other's file.
        let dirs = TestDirs::new();
        std::fs::create_dir_all(dirs.package_dir.join("creds")).unwrap();
        std::fs::write(dirs.package_dir.join("creds/a.tpl"), "value: {{ va }}\n").unwrap();
        // b.tpl references a name it does not declare — `va` belongs to the first
        // entry. Per-entry bindings leave it verbatim; a binding map reused across
        // entries would resolve it and splice the first entry's secret in here.
        // The two entries must use *different* names for this to be observable at
        // all: with a shared name the second binding simply overwrites the first.
        std::fs::write(
            dirs.package_dir.join("creds/b.tpl"),
            "value: {{ vb }}\nborrowed: {{ va }}\n",
        )
        .unwrap();

        let target_a = dirs.target_dir.join("a.conf");
        let target_b = dirs.target_dir.join("b.conf");
        let yaml = format!(
            "name: creds\nenvironments:\n  test:\n    install: \"echo i\"\ndotfiles:\n  \
             - source: \"creds/a.tpl\"\n    target: \"{}\"\n    vars:\n      va: \"read-a\"\n  \
             - source: \"creds/b.tpl\"\n    target: \"{}\"\n    vars:\n      vb: \"read-b\"\n",
            target_a.display(),
            target_b.display()
        );
        std::fs::write(dirs.package_dir.join("creds.yml"), yaml).unwrap();

        let runner = FakeCommandRunner::new()
            .succeeding("read-a", b"AAAAAAAA")
            .succeeding("read-b", b"BBBBBBBB");
        let service = dirs.service_with_runner(runner);

        let _ = collect_events(service.apply_all(ApplyOptions::default()).await).await;

        let a = std::fs::read_to_string(&target_a).unwrap();
        let b = std::fs::read_to_string(&target_b).unwrap();
        assert_eq!(a, "value: AAAAAAAA\n");
        assert_eq!(
            b, "value: BBBBBBBB\nborrowed: {{ va }}\n",
            "the first entry's binding must not be visible to the second"
        );
        assert!(!b.contains("AAAAAAAA"), "a's value bled into b: {b}");
    }

    #[tokio::test]
    async fn a_failing_provider_stops_the_apply_when_stop_on_error_is_set() {
        let dirs = TestDirs::new();
        let target = dirs.target_dir.join("credentials");
        provider_package(&dirs.package_dir, target.to_str().unwrap(), "op read x");

        let runner = FakeCommandRunner::new().failing("op read x", b"not logged in");
        let service = dirs.service_with_runner(runner);

        let events = collect_events(service.apply_all(ApplyOptions::default()).await).await;

        assert!(
            matches!(
                get_operation_result(&events),
                Some(OperationResult::Failure(_))
            ),
            "stop_on_error defaults to true, so a failed resolve aborts"
        );
        assert!(!target.exists());
    }

    #[tokio::test]
    async fn a_failing_provider_reports_stderr() {
        let dirs = TestDirs::new();
        let target = dirs.target_dir.join("credentials");
        provider_package(&dirs.package_dir, target.to_str().unwrap(), "op read x");

        let runner = FakeCommandRunner::new().failing("op read x", b"not logged in");
        let service = dirs.service_with_runner(runner);

        let events = collect_events(service.apply_all(ApplyOptions::default()).await).await;

        assert!(
            format!("{events:?}").contains("not logged in"),
            "a failure must stay diagnosable"
        );
    }

    #[tokio::test]
    async fn stderr_from_a_succeeding_provider_never_surfaces() {
        let dirs = TestDirs::new();
        let target = dirs.target_dir.join("credentials");
        provider_package(&dirs.package_dir, target.to_str().unwrap(), "op read x");

        // A provider run with a verbose flag can echo secret material to stderr.
        let runner = FakeCommandRunner::new().succeeding_noisy(
            "op read x",
            b"content",
            format!("debug: token={SECRET}").as_bytes(),
        );
        let service = dirs.service_with_runner(runner);

        let events = collect_events(service.apply_all(ApplyOptions::default()).await).await;

        assert_no_event_mentions(&events, SECRET);
    }

    #[tokio::test]
    async fn empty_provider_output_is_an_error_and_does_not_truncate_the_target() {
        let dirs = TestDirs::new();
        let target = dirs.target_dir.join("credentials");
        std::fs::write(&target, "existing credential").unwrap();
        provider_package(&dirs.package_dir, target.to_str().unwrap(), "op read x");

        let runner = FakeCommandRunner::new().succeeding("op read x", b"");
        let service = dirs.service_with_runner(runner);

        let events = collect_events(service.apply_all(ApplyOptions::default()).await).await;

        assert!(format!("{events:?}").contains("produced no output"));
        assert_eq!(
            std::fs::read_to_string(&target).unwrap(),
            "existing credential"
        );
    }

    #[tokio::test]
    async fn a_dry_run_executes_no_command_at_all() {
        let dirs = TestDirs::new();
        let target = dirs.target_dir.join("credentials");
        provider_package(&dirs.package_dir, target.to_str().unwrap(), "op read x");

        let runner = FakeCommandRunner::new().succeeding("op read x", SECRET.as_bytes());
        let service = dirs.service_with_runner(runner.clone());

        let options = ApplyOptions {
            dry_run: true,
            ..Default::default()
        };
        let events = collect_events(service.apply_all(options).await).await;

        // Resolving is what runs the user's commands, and a preview must not do
        // that — it reaches a secret store and can raise a biometric prompt.
        assert_eq!(
            runner.call_count(),
            0,
            "--dry-run must not execute a provider command: {:?}",
            runner.calls()
        );
        assert!(!target.exists(), "dry run must not write");
        assert!(
            events.iter().any(|e| matches!(
                e,
                PackageEvent::DotfileSkipped { reason, .. } if reason.contains("dry run")
            )),
            "the dry run should still report the entry, got: {events:?}"
        );
        assert_no_event_mentions(&events, SECRET);
    }

    #[tokio::test]
    async fn an_invalid_entry_is_refused_rather_than_guessed_at() {
        let dirs = TestDirs::new();
        let target = dirs.target_dir.join("credentials");
        let yaml = format!(
            "name: creds\nenvironments:\n  test:\n    install: \"echo i\"\ndotfiles:\n  \
             - source: \"a.tpl\"\n    command: \"op read x\"\n    target: \"{}\"\n",
            target.display()
        );
        std::fs::write(dirs.package_dir.join("creds.yml"), yaml).unwrap();

        let runner = FakeCommandRunner::new();
        let service = dirs.service_with_runner(runner.clone());

        let events = collect_events(service.apply_all(ApplyOptions::default()).await).await;

        assert!(
            format!("{events:?}").contains("exactly one of"),
            "got: {events:?}"
        );
        assert_eq!(runner.call_count(), 0, "an invalid entry must run nothing");
        assert!(!target.exists());
    }

    // ─── Enumeration must not resolve ───────────────────────────────────────

    #[tokio::test]
    async fn a_drift_check_executes_no_binding() {
        let dirs = TestDirs::new();
        let provider_target = dirs.target_dir.join("provider.conf");
        let template_target = dirs.target_dir.join("template.conf");

        std::fs::create_dir_all(dirs.package_dir.join("creds")).unwrap();
        std::fs::write(dirs.package_dir.join("creds/t.tpl"), "key: {{ v }}\n").unwrap();
        let yaml = format!(
            "name: creds\nenvironments:\n  test:\n    install: \"echo i\"\ndotfiles:\n  \
             - command: \"op read x\"\n    target: \"{}\"\n  \
             - source: \"creds/t.tpl\"\n    target: \"{}\"\n    vars:\n      v: \"op read y\"\n",
            provider_target.display(),
            template_target.display()
        );
        std::fs::write(dirs.package_dir.join("creds.yml"), yaml).unwrap();

        // Scripted so that a resolve attempt would succeed rather than error —
        // the assertion is that it never happens, not that it fails.
        let runner = FakeCommandRunner::new()
            .succeeding("op read x", SECRET.as_bytes())
            .succeeding("op read y", SECRET.as_bytes());
        let service = dirs.service_with_runner(runner.clone());

        let events = collect_events(service.check_drift().await).await;

        assert_eq!(
            runner.call_count(),
            0,
            "a read-only operation must not run a provider command: {:?}",
            runner.calls()
        );
        assert_no_event_mentions(&events, SECRET);
    }

    #[tokio::test]
    async fn a_drift_check_reports_secret_entries_without_counting_them_as_drift() {
        let dirs = TestDirs::new();
        let target = dirs.target_dir.join("credentials");
        provider_package(&dirs.package_dir, target.to_str().unwrap(), "op read x");

        let runner = FakeCommandRunner::new().succeeding("op read x", SECRET.as_bytes());
        let service = dirs.service_with_runner(runner);

        let events = collect_events(service.check_drift().await).await;

        assert!(
            events.iter().any(|e| matches!(
                e,
                PackageEvent::DotfileSkipped { reason, .. } if reason.contains("provider-sourced")
            )),
            "secret entries should be identified, got: {events:?}"
        );
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, PackageEvent::DotfileDriftDetected { .. })),
            "counting them as drift would make sync status permanently dirty"
        );

        // The summary must agree: zero drift, not "one unverifiable therefore one
        // drifted".
        match get_operation_result(&events) {
            Some(OperationResult::Success(OperationSuccess::DotfileDriftChecked {
                drift_count,
                ..
            })) => assert_eq!(*drift_count, 0),
            other => panic!("expected a drift summary, got: {other:?}"),
        }
    }

    // ─── Leak regression ────────────────────────────────────────────────────

    #[tokio::test]
    async fn no_event_carries_the_secret_across_a_full_apply() {
        // Covers deploy, in-sync skip, and conflict in one run, for both a
        // whole-file provider and a templated entry.
        for existing in [None, Some(SECRET), Some("hand-edited")] {
            let dirs = TestDirs::new();
            let provider_target = dirs.target_dir.join("provider.conf");
            let template_target = dirs.target_dir.join("template.conf");

            std::fs::create_dir_all(dirs.package_dir.join("creds")).unwrap();
            std::fs::write(dirs.package_dir.join("creds/t.tpl"), "key: {{ v }}\n").unwrap();
            let yaml = format!(
                "name: creds\nenvironments:\n  test:\n    install: \"echo i\"\ndotfiles:\n  \
                 - command: \"op read x\"\n    target: \"{}\"\n  \
                 - source: \"creds/t.tpl\"\n    target: \"{}\"\n    vars:\n      v: \"op read y\"\n",
                provider_target.display(),
                template_target.display()
            );
            std::fs::write(dirs.package_dir.join("creds.yml"), yaml).unwrap();

            if let Some(content) = existing {
                std::fs::write(&provider_target, content).unwrap();
                std::fs::write(&template_target, content).unwrap();
            }

            let runner = FakeCommandRunner::new()
                .succeeding("op read x", SECRET.as_bytes())
                .succeeding("op read y", SECRET.as_bytes());
            let service = dirs.service_with_runner(runner);

            let events = collect_events(service.apply_all(ApplyOptions::default()).await).await;

            assert_no_event_mentions(&events, SECRET);

            // Positive control: the run really did handle the secret, so this
            // cannot be passing because nothing happened.
            assert!(
                events.iter().any(|e| matches!(
                    e,
                    PackageEvent::DotfileDeployed { .. }
                        | PackageEvent::DotfileSkipped { .. }
                        | PackageEvent::DotfileConflict { .. }
                )),
                "no dotfile outcome was produced for existing={existing:?}"
            );
        }
    }

    #[tokio::test]
    async fn a_secret_conflict_event_reports_structure_only() {
        let dirs = TestDirs::new();
        let target = dirs.target_dir.join("credentials");
        std::fs::write(&target, "line one\nline two\nline three\n").unwrap();
        provider_package(&dirs.package_dir, target.to_str().unwrap(), "op read x");

        let runner = FakeCommandRunner::new().succeeding("op read x", SECRET.as_bytes());
        let service = dirs.service_with_runner(runner);

        let events = collect_events(service.apply_all(ApplyOptions::default()).await).await;

        let conflict = events
            .iter()
            .find_map(|e| match e {
                PackageEvent::DotfileConflict { diff, .. } => Some(diff),
                _ => None,
            })
            .expect("expected a conflict event");

        assert!(conflict.contains("lines"), "got: {conflict}");
        assert!(conflict.contains("content hidden"), "got: {conflict}");
        test_common::assert_secret_free(conflict, SECRET, "the conflict diff");
        assert!(
            conflict.contains("op read x"),
            "the command is a reference, not a credential, and should be shown: {conflict}"
        );
    }

    #[tokio::test]
    async fn auto_accept_does_not_overwrite_a_secret_target() {
        // `auto_accept` is caller-settable — the MCP server exposes it to an
        // assistant — so honoring it would let a non-interactive caller silently
        // overwrite a hand-edited credentials file. The spec requires provider
        // conflicts to be reported and skipped without an interactive resolver,
        // whatever auto_accept says.
        let dirs = TestDirs::new();
        let target = dirs.target_dir.join("credentials");
        std::fs::write(&target, "hand-edited credential").unwrap();
        provider_package(&dirs.package_dir, target.to_str().unwrap(), "op read x");

        let runner = FakeCommandRunner::new().succeeding("op read x", SECRET.as_bytes());
        let service = dirs.service_with_runner(runner);

        let options = ApplyOptions {
            auto_accept: true,
            ..Default::default()
        };
        let events = collect_events(service.apply_all(options).await).await;

        assert_eq!(
            std::fs::read_to_string(&target).unwrap(),
            "hand-edited credential",
            "auto_accept must not force-overwrite a secret-bearing target"
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, PackageEvent::DotfileConflict { .. })),
            "the conflict must still be reported, got: {events:?}"
        );
        assert_no_event_mentions(&events, SECRET);
    }

    #[tokio::test]
    async fn auto_accept_still_overwrites_an_ordinary_repo_file() {
        // The guard above is specific to secret-bearing entries; `--yes` keeps
        // working for ordinary dotfiles, which have a diff and a recorded state.
        let dirs = TestDirs::new();
        let source_dir = dirs.package_dir.join("myapp");
        std::fs::create_dir_all(&source_dir).unwrap();
        std::fs::write(source_dir.join("config.toml"), "key = \"from-repo\"").unwrap();
        let target = dirs.target_dir.join("config.toml");
        std::fs::write(&target, "key = \"hand-edited\"").unwrap();
        create_package_with_dotfiles(
            &dirs.package_dir,
            "myapp",
            &[("myapp/config.toml", target.to_str().unwrap())],
        );

        let service = dirs.service();
        let options = ApplyOptions {
            auto_accept: true,
            ..Default::default()
        };
        let _ = collect_events(service.apply_all(options).await).await;

        assert_eq!(
            std::fs::read_to_string(&target).unwrap(),
            "key = \"from-repo\""
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn an_existing_but_unreadable_target_is_not_silently_overwritten() {
        use std::os::unix::fs::PermissionsExt as _;

        // An unreadable file is still a file, and it may be the very credential an
        // overwrite would destroy. Treating "cannot read" as "not there" would
        // deploy over it with no prompt.
        let dirs = TestDirs::new();
        let target = dirs.target_dir.join("credentials");
        std::fs::write(&target, "existing credential").unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o000)).unwrap();
        provider_package(&dirs.package_dir, target.to_str().unwrap(), "op read x");

        let runner = FakeCommandRunner::new().succeeding("op read x", SECRET.as_bytes());
        let service = dirs.service_with_runner(runner);

        let events = collect_events(service.apply_all(ApplyOptions::default()).await).await;

        assert!(
            events
                .iter()
                .any(|e| matches!(e, PackageEvent::DotfileConflict { .. })),
            "an unreadable target must be a conflict, got: {events:?}"
        );

        // Restore permissions so the content can be checked.
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(
            std::fs::read_to_string(&target).unwrap(),
            "existing credential",
            "the unreadable target must not have been overwritten"
        );
        assert_no_event_mentions(&events, SECRET);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn an_unreadable_target_conflict_says_so_rather_than_reporting_zero_lines() {
        use std::os::unix::fs::PermissionsExt as _;

        let dirs = TestDirs::new();
        let target = dirs.target_dir.join("credentials");
        std::fs::write(&target, "existing credential").unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o000)).unwrap();
        provider_package(&dirs.package_dir, target.to_str().unwrap(), "op read x");

        let runner = FakeCommandRunner::new().succeeding("op read x", SECRET.as_bytes());
        let service = dirs.service_with_runner(runner);

        let events = collect_events(service.apply_all(ApplyOptions::default()).await).await;
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o600)).unwrap();

        let summary = events
            .iter()
            .find_map(|e| match e {
                PackageEvent::DotfileConflict { diff, .. } => Some(diff),
                _ => None,
            })
            .expect("expected a conflict event");

        assert!(
            summary.contains("could not be read"),
            "an empty-looking '0 lines' would understate what an overwrite \
             destroys, got: {summary}"
        );
    }

    #[tokio::test]
    async fn a_template_source_escaping_the_package_directory_is_refused() {
        // Apply never runs validation, so the static `..` check on `source` is not
        // a gate. Without a runtime containment check, a crafted template source
        // splices the contents of a file outside the package directory into a
        // deployed dotfile.
        let dirs = TestDirs::new();
        let outside = dirs._temp.path().join("outside.tpl");
        std::fs::write(&outside, "STOLEN-FROM-OUTSIDE: {{ v }}\n").unwrap();

        let target = dirs.target_dir.join("credentials");
        let yaml = format!(
            "name: creds\nenvironments:\n  test:\n    install: \"echo i\"\ndotfiles:\n  \
             - source: \"../outside.tpl\"\n    target: \"{}\"\n    vars:\n      v: \"op read x\"\n",
            target.display()
        );
        std::fs::write(dirs.package_dir.join("creds.yml"), yaml).unwrap();

        let runner = FakeCommandRunner::new().succeeding("op read x", SECRET.as_bytes());
        let service = dirs.service_with_runner(runner);

        let events = collect_events(service.apply_all(ApplyOptions::default()).await).await;

        assert!(!target.exists(), "the escaping template must not deploy");
        assert!(
            format!("{events:?}").contains("escapes the package directory"),
            "expected a containment refusal, got: {events:?}"
        );
        assert!(
            !format!("{events:?}").contains("STOLEN-FROM-OUTSIDE"),
            "the outside file's contents must not surface"
        );
    }

    #[tokio::test]
    async fn a_repo_file_source_escaping_the_package_directory_is_still_refused() {
        // The pre-existing guard on the ordinary path, asserted here so moving it
        // into a shared module cannot quietly drop it.
        let dirs = TestDirs::new();
        let outside = dirs._temp.path().join("outside.conf");
        std::fs::write(&outside, "outside content").unwrap();

        let target = dirs.target_dir.join("escaped.conf");
        create_package_with_dotfiles(
            &dirs.package_dir,
            "myapp",
            &[("../outside.conf", target.to_str().unwrap())],
        );

        let service = dirs.service();
        let events = collect_events(service.apply_all(ApplyOptions::default()).await).await;

        assert!(!target.exists(), "the escaping source must not deploy");
        assert!(
            format!("{events:?}").contains("escapes YAML base directory"),
            "got: {events:?}"
        );
    }

    #[tokio::test]
    async fn a_dry_run_refuses_a_relative_target_the_same_way_a_real_apply_does() {
        // The dry-run short-circuit must sit after the checks that would refuse
        // the entry outright, or a preview claims it "would run N commands" for
        // something a real apply would never touch.
        let dirs = TestDirs::new();
        let yaml = "name: creds\nenvironments:\n  test:\n    install: \"echo i\"\ndotfiles:\n  \
                    - command: \"op read x\"\n    target: \"relative/credentials\"\n";
        std::fs::write(dirs.package_dir.join("creds.yml"), yaml).unwrap();

        let runner = FakeCommandRunner::new().succeeding("op read x", SECRET.as_bytes());
        let service = dirs.service_with_runner(runner.clone());

        let options = ApplyOptions {
            dry_run: true,
            ..Default::default()
        };
        let events = collect_events(service.apply_all(options).await).await;

        assert_eq!(runner.call_count(), 0);
        assert!(
            format!("{events:?}").contains("is not absolute"),
            "a dry run should report the same refusal a real apply would, got: {events:?}"
        );
        assert!(
            !format!("{events:?}").contains("would run"),
            "must not claim it would run commands for an entry that can never deploy"
        );
    }

    #[tokio::test]
    async fn a_dry_run_refuses_an_escaping_template_the_same_way_a_real_apply_does() {
        // Containment is decidable from the path alone, so the preview can and must
        // apply it. Same rule as the relative-target case above: a dry run that says
        // it "would run 1 command(s)" for an entry a real apply refuses outright is
        // describing something that will never happen.
        let dirs = TestDirs::new();
        let outside = dirs._temp.path().join("outside.tpl");
        std::fs::write(&outside, "STOLEN: {{ v }}\n").unwrap();

        let target = dirs.target_dir.join("credentials");
        let yaml = format!(
            "name: creds\nenvironments:\n  test:\n    install: \"echo i\"\ndotfiles:\n  \
             - source: \"../outside.tpl\"\n    target: \"{}\"\n    vars:\n      v: \"op read x\"\n",
            target.display()
        );
        std::fs::write(dirs.package_dir.join("creds.yml"), yaml).unwrap();

        let runner = FakeCommandRunner::new().succeeding("op read x", SECRET.as_bytes());
        let service = dirs.service_with_runner(runner.clone());

        let options = ApplyOptions {
            dry_run: true,
            ..Default::default()
        };
        let events = collect_events(service.apply_all(options).await).await;

        assert_eq!(runner.call_count(), 0);
        assert!(
            format!("{events:?}").contains("escapes the package directory"),
            "a dry run should report the same refusal a real apply would, got: {events:?}"
        );
        assert!(
            !format!("{events:?}").contains("would run"),
            "must not claim it would run commands for an entry that can never deploy"
        );
    }

    #[tokio::test]
    async fn stopping_on_error_still_records_what_was_already_deployed() {
        // An abort must not discard the deploy state for files already written in
        // the same run: the files are on disk, so dropping their record would make
        // the next drift check report correctly-deployed files as untracked.
        let dirs = TestDirs::new();

        // Relies on packages being enumerated in sorted path order, so "aaa"
        // is processed before "zzz" and the ordinary dotfile deploys before the
        // provider fails. That ordering is a guarantee of the repository, pinned
        // by `list_yaml_files_returns_them_in_sorted_order` — it is not an
        // assumption about the filesystem. It was exactly that before, and CI on
        // ext4 (hash order, unlike APFS) deployed "zzz" first and failed here.
        let source_dir = dirs.package_dir.join("aaa");
        std::fs::create_dir_all(&source_dir).unwrap();
        std::fs::write(source_dir.join("config.toml"), "key = \"value\"").unwrap();
        let ok_target = dirs.target_dir.join("config.toml");
        create_package_with_dotfiles(
            &dirs.package_dir,
            "aaa",
            &[("aaa/config.toml", ok_target.to_str().unwrap())],
        );

        let bad_target = dirs.target_dir.join("credentials");
        let yaml = format!(
            "name: zzz\nenvironments:\n  test:\n    install: \"echo i\"\ndotfiles:\n  \
             - command: \"op read x\"\n    target: \"{}\"\n",
            bad_target.display()
        );
        std::fs::write(dirs.package_dir.join("zzz.yml"), yaml).unwrap();

        let runner = FakeCommandRunner::new().failing("op read x", b"not logged in");
        let service = dirs.service_with_runner(runner);

        let events = collect_events(service.apply_all(ApplyOptions::default()).await).await;

        assert!(
            matches!(
                get_operation_result(&events),
                Some(OperationResult::Failure(_))
            ),
            "the run should still report failure"
        );
        assert_eq!(
            std::fs::read_to_string(&ok_target).unwrap(),
            "key = \"value\"",
            "the earlier dotfile really was deployed"
        );

        let state = std::fs::read_to_string(state_file(&dirs)).unwrap_or_default();
        assert!(
            state.contains("aaa/config.toml"),
            "the successful deployment must still be recorded, got: {state}"
        );
    }

    #[tokio::test]
    async fn a_misspelled_dotfile_key_is_skipped_with_a_warning_while_the_package_still_applies() {
        // `var:` for `vars:` leaves a template indistinguishable from a plain
        // repository file. Deploying it would write the *unrendered* template —
        // literal `{{ api_key }}` — over the credentials target and record that
        // content in deploy state, so the entry has to be refused outright.
        //
        // The template file really exists and really contains a placeholder: if
        // the entry were treated as a repository file the target would be written
        // with that body, so `exists=false` can only mean the entry was skipped.
        // A missing template would make this pass for the wrong reason.
        let dirs = TestDirs::new();
        std::fs::create_dir_all(dirs.package_dir.join("creds")).unwrap();
        std::fs::write(
            dirs.package_dir.join("creds/t.tpl"),
            "api_key = \"{{ api_key }}\"\n",
        )
        .unwrap();

        let bad_target = dirs.target_dir.join("credentials");
        let good_target = dirs.target_dir.join("bat.conf");
        std::fs::write(dirs.package_dir.join("bat.conf"), "fine\n").unwrap();

        let yaml = format!(
            "name: creds\nenvironments:\n  test:\n    install: \"echo i\"\ndotfiles:\n  \
             - source: \"creds/t.tpl\"\n    target: \"{}\"\n    var:\n      api_key: \"op read x\"\n  \
             - source: \"bat.conf\"\n    target: \"{}\"\n",
            bad_target.display(),
            good_target.display()
        );
        std::fs::write(dirs.package_dir.join("creds.yml"), yaml).unwrap();

        let service = dirs.service();
        let events = collect_events(service.apply_all(ApplyOptions::default()).await).await;

        assert!(
            !bad_target.exists(),
            "the typo'd entry must not deploy, got: {:?}",
            std::fs::read_to_string(&bad_target)
        );
        assert_eq!(
            std::fs::read_to_string(&good_target).unwrap(),
            "fine\n",
            "the package's other dotfile must still deploy"
        );

        let rendered = format!("{events:?}");
        assert!(
            rendered.contains(bad_target.to_str().unwrap()),
            "the warning must name the skipped target, got: {events:?}"
        );
    }

    #[tokio::test]
    async fn an_unparsable_package_is_named_rather_than_silently_dropped() {
        // `valid_packages()` drops parse failures. Without a warning, a package
        // directory holding exactly one unparsable package produces a successful
        // apply that deployed nothing — and the user's credentials dotfile
        // quietly stops deploying, surfacing later as an auth failure nobody
        // traces back to the package file.
        //
        // The fixture is malformed YAML rather than a schema violation on
        // purpose: this test was defanged once already when the schema changed
        // under it, and a syntax error cannot stop being a parse failure.
        let dirs = TestDirs::new();
        std::fs::write(
            dirs.package_dir.join("creds.yml"),
            "name: creds\ndotfiles:\n  - [unclosed\n",
        )
        .unwrap();

        let service = dirs.service();
        let events = collect_events(service.apply_all(ApplyOptions::default()).await).await;

        let rendered = format!("{events:?}");
        assert!(
            rendered.contains("creds.yml"),
            "the warning must name the file, got: {events:?}"
        );
        assert!(
            rendered.contains("unparsable"),
            "the warning must say why it was skipped, got: {events:?}"
        );
    }

    #[tokio::test]
    async fn a_valid_package_still_applies_alongside_an_unparsable_one() {
        // The warning must not become an abort: one bad file should not stop the
        // rest of the directory deploying.
        //
        // Malformed YAML, not a schema violation: the previous fixture
        // (`- nope: 1`) was unparsable only incidentally, because it omitted the
        // required `target` — so it kept passing while testing something other
        // than what its name claims.
        let dirs = TestDirs::new();
        std::fs::write(
            dirs.package_dir.join("broken.yml"),
            "name: broken\ndotfiles:\n  - [unclosed\n",
        )
        .unwrap();

        let good_target = dirs.target_dir.join("credentials");
        provider_package(
            &dirs.package_dir,
            good_target.to_str().unwrap(),
            "op read x",
        );

        let runner = FakeCommandRunner::new().succeeding("op read x", SECRET.as_bytes());
        let service = dirs.service_with_runner(runner);

        let events = collect_events(service.apply_all(ApplyOptions::default()).await).await;

        assert_eq!(std::fs::read_to_string(&good_target).unwrap(), SECRET);
        assert!(
            format!("{events:?}").contains("broken.yml"),
            "the unparsable file must still be named: {events:?}"
        );
        assert_no_event_mentions(&events, SECRET);
    }

    // ─── Leak regression: the failure path ──────────────────────────────────

    #[tokio::test]
    async fn a_failing_provider_does_not_leak_its_stdout() {
        // A provider's stdout IS the secret. Two separate things keep it out of
        // the event stream: this path reports failures with its own error type,
        // which carries the command and its stderr and never the output, and
        // `CommandFailure::ExecutionFailed` has no `stdout` field for a failure to
        // be routed into. This test covers the first — prefer the resolve path's
        // own variants over `OperationFailure::from` regardless, since those name
        // the entry and the var rather than only saying a command failed.
        let dirs = TestDirs::new();
        let target = dirs.target_dir.join("credentials");
        provider_package(&dirs.package_dir, target.to_str().unwrap(), "op read x");

        let runner = FakeCommandRunner::new().failing_with_stdout(
            "op read x",
            SECRET.as_bytes(),
            b"error: vault sealed",
        );
        let service = dirs.service_with_runner(runner);

        let events = collect_events(service.apply_all(ApplyOptions::default()).await).await;

        assert_no_event_mentions(&events, SECRET);

        // Positive control: the failure really was reported, so this is not
        // passing because nothing happened.
        assert!(
            format!("{events:?}").contains("vault sealed"),
            "the failure must stay diagnosable: {events:?}"
        );
    }

    #[tokio::test]
    async fn a_zero_length_output_failure_does_not_leak_stderr() {
        // Empty stdout is an error, and on that path stderr was never a failure
        // signal — the command exited zero — so it must not be forwarded either.
        let dirs = TestDirs::new();
        let target = dirs.target_dir.join("credentials");
        provider_package(&dirs.package_dir, target.to_str().unwrap(), "op read x");

        let runner = FakeCommandRunner::new().succeeding_noisy(
            "op read x",
            b"",
            format!("debug: retrieved token={SECRET}").as_bytes(),
        );
        let service = dirs.service_with_runner(runner);

        let events = collect_events(service.apply_all(ApplyOptions::default()).await).await;

        assert_no_event_mentions(&events, SECRET);
        assert!(
            format!("{events:?}").contains("produced no output"),
            "the empty-output error must still be reported: {events:?}"
        );
    }

    #[tokio::test]
    async fn a_failing_binding_does_not_leak_its_stdout() {
        let dirs = TestDirs::new();
        let target = dirs.target_dir.join("credentials");
        template_package(
            &dirs.package_dir,
            target.to_str().unwrap(),
            "key: {{ api_key }}\n",
            &[("api_key", "op read a")],
        );

        let runner = FakeCommandRunner::new().failing_with_stdout(
            "op read a",
            SECRET.as_bytes(),
            b"error: not logged in",
        );
        let service = dirs.service_with_runner(runner);

        let events = collect_events(service.apply_all(ApplyOptions::default()).await).await;

        assert_no_event_mentions(&events, SECRET);
        assert!(format!("{events:?}").contains("not logged in"));
    }

    /// A resolver that records what it was handed and always accepts.
    #[derive(Default)]
    struct RecordingResolver {
        seen: Arc<Mutex<Vec<String>>>,
    }

    impl ConflictResolver for RecordingResolver {
        fn resolve(&self, _target: &str, detail: ConflictDetail<'_>) -> ConflictResolution {
            // Stands in for an interactive adapter that offers `[r]eveal`.
            if let ConflictDetail::Secret {
                incoming, current, ..
            } = detail
            {
                let mut seen = self.seen.lock().unwrap();
                seen.push(String::from_utf8_lossy(incoming).into_owned());
                seen.push(String::from_utf8_lossy(current).into_owned());
            }
            ConflictResolution::Accept
        }
    }

    #[tokio::test]
    async fn a_resolver_receives_the_values_but_events_still_do_not() {
        let dirs = TestDirs::new();
        let target = dirs.target_dir.join("credentials");
        std::fs::write(&target, "previous-credential").unwrap();
        provider_package(&dirs.package_dir, target.to_str().unwrap(), "op read x");

        let runner = FakeCommandRunner::new().succeeding("op read x", SECRET.as_bytes());
        let service = dirs.service_with_runner(runner);

        let resolver = Arc::new(RecordingResolver::default());
        let options = ApplyOptions {
            conflict_resolver: Some(resolver.clone()),
            ..Default::default()
        };
        let events = collect_events(service.apply_all(options).await).await;

        let seen = resolver.seen.lock().unwrap().clone();
        assert!(
            seen.iter().any(|v| v.contains(SECRET)),
            "the resolver is the one place the values may go, but it saw: {seen:?}"
        );
        assert!(
            seen.iter().any(|v| v.contains("previous-credential")),
            "the resolver should see both sides, saw: {seen:?}"
        );

        assert_no_event_mentions(&events, SECRET);
    }

    // ─── Entries refused before anything runs (selfie-n310/3c5a/kj5y) ────────
    //
    // A consumer can drop a refused entry and still compile — `let Ok(..) else
    // { continue }` builds — which would leave the user a dotfile that never
    // deploys and no diagnostic. One test per consumer path asserts the refusal
    // is reported, so a consumer that swallows it fails a test.

    /// A template whose var name cannot be substituted, plus its template file.
    ///
    /// The template really exists and really contains the placeholder: if the
    /// entry were treated as any kind of deployable entry the target would be
    /// written, so `!target.exists()` can only mean it was refused.
    fn bad_var_name_package(package_dir: &std::path::Path, target: &str, var: &str) {
        std::fs::create_dir_all(package_dir.join("creds")).unwrap();
        std::fs::write(
            package_dir.join("creds/credentials.tpl"),
            format!("api_key: {{{{ {var} }}}}\n"),
        )
        .unwrap();

        let yaml = format!(
            "name: creds\nenvironments:\n  test:\n    install: \"echo i\"\ndotfiles:\n  \
             - source: \"creds/credentials.tpl\"\n    target: \"{target}\"\n    vars:\n      \
             \"{var}\": \"op read x\"\n"
        );
        std::fs::write(package_dir.join("creds.yml"), yaml).unwrap();
    }

    #[tokio::test]
    async fn a_var_name_that_cannot_be_substituted_runs_no_command() {
        // selfie-3c5a. `template::render` cannot substitute `not-a-name`, so this
        // entry provably cannot produce the file it describes. Nothing may run for
        // it: `op read x` is a REAL credential fetch that can raise a biometric or
        // password prompt, and the value would be fetched, discarded, and the
        // placeholder deployed verbatim over the credentials target.
        let dirs = TestDirs::new();
        let target = dirs.target_dir.join("credentials");
        bad_var_name_package(&dirs.package_dir, target.to_str().unwrap(), "not-a-name");

        // Scripted to succeed: the assertion is that the fetch never happens, not
        // that it fails.
        let runner = FakeCommandRunner::new().succeeding("op read x", SECRET.as_bytes());
        let service = dirs.service_with_runner(runner.clone());

        let events = collect_events(service.apply_all(ApplyOptions::default()).await).await;

        assert_eq!(
            runner.call_count(),
            0,
            "no command may run for a binding that cannot be substituted: {:?}",
            runner.calls()
        );
        assert!(!target.exists(), "the entry must not deploy");
        assert!(
            format!("{events:?}").contains("not-a-name"),
            "the refusal must name the var, got: {events:?}"
        );
        assert_no_event_mentions(&events, SECRET);
    }

    #[tokio::test]
    async fn a_valid_var_name_still_runs_its_command_and_renders_it() {
        // The control for the test above. `FakeCommandRunner` records every call
        // unconditionally, before it looks a response up, so a call whose output
        // was discarded would still show in `call_count()` — but only if this path
        // reaches the runner at all. This proves it does, on a fixture differing
        // by one character. Asserting the *rendered* content matters too:
        // "a command ran" alone could hold while the entry took another branch.
        let dirs = TestDirs::new();
        let target = dirs.target_dir.join("credentials");
        bad_var_name_package(&dirs.package_dir, target.to_str().unwrap(), "not_a_name");

        let runner = FakeCommandRunner::new().succeeding("op read x", SECRET.as_bytes());
        let service = dirs.service_with_runner(runner.clone());

        collect_events(service.apply_all(ApplyOptions::default()).await).await;

        assert_eq!(runner.call_count(), 1, "got: {:?}", runner.calls());
        assert_eq!(
            std::fs::read_to_string(&target).unwrap(),
            format!("api_key: {SECRET}\n"),
            "the binding must actually be substituted"
        );
    }

    #[tokio::test]
    async fn an_anchor_colliding_with_a_dotfile_field_is_refused_before_deploying() {
        // selfie-kj5y. `_vars:` was read as a YAML anchor definition and dropped,
        // leaving a template that looked like a plain repository file — so it
        // deployed *unrendered*, literal `{{ api_key }}`, over the credentials
        // target, with the bindings silently absent and `selfie spec validate`
        // reporting nothing at all.
        //
        // Every colliding name is exercised, not `_vars` alone: a fix hard-coded
        // to one of them passes a single-name test.
        for field in ["vars", "source", "command", "target"] {
            let dirs = TestDirs::new();
            std::fs::create_dir_all(dirs.package_dir.join("creds")).unwrap();
            std::fs::write(
                dirs.package_dir.join("creds/credentials.tpl"),
                "api_key: {{ api_key }}\n",
            )
            .unwrap();

            let target = dirs.target_dir.join("credentials");
            let yaml = format!(
                "name: creds\nenvironments:\n  test:\n    install: \"echo i\"\ndotfiles:\n  \
                 - source: \"creds/credentials.tpl\"\n    target: \"{}\"\n    _{field}:\n      \
                 api_key: \"op read x\"\n",
                target.display()
            );
            std::fs::write(dirs.package_dir.join("creds.yml"), yaml).unwrap();

            let runner = FakeCommandRunner::new().succeeding("op read x", SECRET.as_bytes());
            let service = dirs.service_with_runner(runner.clone());

            let events = collect_events(service.apply_all(ApplyOptions::default()).await).await;

            assert!(
                !target.exists(),
                "'_{field}' must not deploy, got: {:?}",
                std::fs::read_to_string(&target)
            );
            assert_eq!(runner.call_count(), 0, "for _{field}");
            assert!(
                format!("{events:?}").contains("cannot be told apart from a misspelling"),
                "the refusal must name the ambiguity, got: {events:?}"
            );
        }
    }

    #[tokio::test]
    async fn an_anchor_not_named_like_a_dotfile_field_still_deploys() {
        // The control that keeps YAML anchors working, and the reason the rule is
        // a *collision* rule rather than "no underscore keys inside an entry".
        // Consuming the alias proves the key was really parsed, not just
        // tolerated.
        let dirs = TestDirs::new();
        std::fs::write(dirs.package_dir.join("bat.conf"), "fine\n").unwrap();

        let target = dirs.target_dir.join("bat.conf");
        let yaml = format!(
            "name: bat\nenvironments:\n  test:\n    install: \"echo i\"\ndotfiles:\n  \
             - _anchor: &a \"bat.conf\"\n    source: *a\n    target: \"{}\"\n",
            target.display()
        );
        std::fs::write(dirs.package_dir.join("bat.yml"), yaml).unwrap();

        let service = dirs.service();
        collect_events(service.apply_all(ApplyOptions::default()).await).await;

        assert_eq!(std::fs::read_to_string(&target).unwrap(), "fine\n");
    }

    #[tokio::test]
    async fn a_drift_check_reports_a_refused_entry_rather_than_calling_it_unverifiable() {
        // The drift consumer. A refused entry reported as "provider-sourced (not
        // verifiable without resolving)" would be filed under the one status a
        // user is trained to ignore, hiding a dotfile that can never deploy.
        let dirs = TestDirs::new();
        let target = dirs.target_dir.join("credentials");
        bad_var_name_package(&dirs.package_dir, target.to_str().unwrap(), "not-a-name");

        let runner = FakeCommandRunner::new().succeeding("op read x", SECRET.as_bytes());
        let service = dirs.service_with_runner(runner.clone());

        let events = collect_events(service.check_drift().await).await;

        assert_eq!(runner.call_count(), 0, "got: {:?}", runner.calls());
        assert!(
            format!("{events:?}").contains("not-a-name"),
            "drift must name the refused entry, got: {events:?}"
        );
        assert!(
            !events.iter().any(|e| matches!(
                e,
                PackageEvent::DotfileSkipped { reason, .. } if reason.contains("provider-sourced")
            )),
            "an undeployable entry is not merely unverifiable, got: {events:?}"
        );
    }

    /// Ctrl+C during `selfie apply`.
    ///
    /// Apply gained the ability to run commands after the cancellation token was
    /// threaded through the rest of the service layer, and did not inherit it:
    /// the resolve path built a fresh `CancellationToken` nobody held, so an `op
    /// read` blocked on a biometric prompt could only be escaped by waiting out
    /// `command_timeout`.
    mod cancellation {
        use super::*;
        use selfie::commands::{
            CommandError, CommandOutput, CommandRunner, ContentOutput, OutputChunk,
        };
        use std::path::Path;
        use std::time::Duration;

        /// What the runner should do on a given call.
        #[derive(Clone, Copy, Debug, Default)]
        struct CallEffect {
            /// Cancel the token the *service* holds, as a signal handler would
            /// while this command is in flight.
            cancel: bool,
            /// Report the command as killed, which is what `ShellCommandRunner`
            /// does once the token fires.
            fail_as_cancelled: bool,
        }

        /// A runner that records the token it is **handed** and cancels the token
        /// the service was **built with**.
        ///
        /// Those being two different things is the entire mechanism, and it is not
        /// self-evident. `resolve_content` takes one `&CancellationToken` and
        /// reuses that same reference for every command of an entry — so a fake
        /// that cancelled the token it was *handed* would cancel a placeholder
        /// just as readily as the real one, and every assertion below would hold
        /// with the bug still in place. Holding an independent clone of the real
        /// token means `observed` differs between the two worlds: the second call
        /// sees `true` only if what reached the runner *is* the service's token.
        #[derive(Clone, Debug)]
        struct TokenObservingRunner {
            /// The token the service was constructed with.
            real_token: CancellationToken,
            /// `is_cancelled()` of the token handed to each call, in call order.
            observed: Arc<Mutex<Vec<bool>>>,
            /// Commands seen, in call order.
            calls: Arc<Mutex<Vec<String>>>,
            /// Indexed by call number.
            effects: Vec<CallEffect>,
            /// Refuse any call handed an already-cancelled token, as
            /// `ShellCommandRunner::run_buffered` does in its pre-spawn check.
            ///
            /// Off by default so the other tests observe the token without the
            /// runner acting on it. On, it makes this fake faithful to the
            /// production runner on the one behavior that closes the
            /// between-bindings window.
            refuse_when_handed_cancelled: bool,
            /// Returned as stdout by any call that is not failing.
            stdout: Vec<u8>,
        }

        impl TokenObservingRunner {
            fn new(real_token: &CancellationToken, effects: Vec<CallEffect>) -> Self {
                Self {
                    real_token: real_token.clone(),
                    observed: Arc::new(Mutex::new(Vec::new())),
                    calls: Arc::new(Mutex::new(Vec::new())),
                    effects,
                    refuse_when_handed_cancelled: false,
                    stdout: SECRET.as_bytes().to_vec(),
                }
            }

            /// Behave like the real shell runner: refuse a pre-cancelled token.
            fn refusing_a_cancelled_token(mut self) -> Self {
                self.refuse_when_handed_cancelled = true;
                self
            }

            /// Cancel while the *first* command is in flight, and report that
            /// command as killed — a faithful Ctrl+C.
            fn cancelling_and_failing(real_token: &CancellationToken) -> Self {
                Self::new(
                    real_token,
                    vec![CallEffect {
                        cancel: true,
                        fail_as_cancelled: true,
                    }],
                )
            }

            fn observed(&self) -> Vec<bool> {
                self.observed.lock().unwrap().clone()
            }

            fn calls(&self) -> Vec<String> {
                self.calls.lock().unwrap().clone()
            }

            fn answer(
                &self,
                command: &str,
                handed: &CancellationToken,
            ) -> Result<CommandOutput, CommandError> {
                let index = {
                    let mut calls = self.calls.lock().unwrap();
                    calls.push(command.to_string());
                    calls.len() - 1
                };
                // Recorded before this call's own effect fires, so the value is
                // what the caller handed over rather than what this runner just
                // did.
                self.observed.lock().unwrap().push(handed.is_cancelled());

                // Before this call's own effect, and before any scripted answer:
                // the real runner checks the token before it spawns anything.
                if self.refuse_when_handed_cancelled && handed.is_cancelled() {
                    return Err(CommandError::Cancelled {
                        command: command.to_string(),
                        working_directory: Path::new(".").to_path_buf(),
                    });
                }

                let effect = self.effects.get(index).copied().unwrap_or_default();
                if effect.cancel {
                    self.real_token.cancel();
                }
                if effect.fail_as_cancelled {
                    return Err(CommandError::Cancelled {
                        command: command.to_string(),
                        working_directory: Path::new(".").to_path_buf(),
                    });
                }
                Ok(command_output(self.stdout.clone()))
            }
        }

        impl CommandRunner for TokenObservingRunner {
            async fn is_command_available(&self, _command: &str) -> bool {
                true
            }

            async fn execute(
                &self,
                command: &str,
                token: &CancellationToken,
            ) -> Result<CommandOutput, CommandError> {
                self.answer(command, token)
            }

            async fn execute_with_timeout(
                &self,
                command: &str,
                _timeout: Duration,
                token: &CancellationToken,
            ) -> Result<CommandOutput, CommandError> {
                self.answer(command, token)
            }

            async fn execute_in_dir(
                &self,
                command: &str,
                _working_dir: &Path,
                _timeout: Duration,
                token: &CancellationToken,
            ) -> Result<CommandOutput, CommandError> {
                self.answer(command, token)
            }

            async fn execute_streaming(
                &self,
                command: &str,
                _timeout: Duration,
                _output_sender: tokio::sync::mpsc::Sender<OutputChunk>,
                token: &CancellationToken,
            ) -> Result<CommandOutput, CommandError> {
                self.answer(command, token)
            }

            /// The path resolve actually takes, so this is the one these tests
            /// observe the token through.
            async fn execute_for_content(
                &self,
                command: &str,
                _working_dir: &Path,
                _timeout: Duration,
                token: &CancellationToken,
            ) -> Result<ContentOutput, CommandError> {
                let output = self.answer(command, token)?;
                let success = output.is_success();
                let stderr = output.stderr().to_vec();
                Ok(ContentOutput::from_parts(
                    success,
                    output.into_stdout(),
                    stderr,
                    0,
                    true,
                ))
            }
        }

        /// A successful `CommandOutput` carrying `stdout`.
        fn command_output(stdout: Vec<u8>) -> CommandOutput {
            use std::os::unix::process::ExitStatusExt as _;
            CommandOutput::from_parts(
                std::process::ExitStatus::from_raw(0),
                stdout,
                Vec::new(),
                Duration::ZERO,
            )
        }

        /// Assert the run ended in a failure whose message names cancellation.
        #[track_caller]
        fn assert_cancelled(events: &[PackageEvent]) {
            let result = get_operation_result(events)
                .expect("a cancelled run still has to emit its Completed event");
            let OperationResult::Failure(failure) = result else {
                panic!("expected a failure, got: {result:?}");
            };
            let rendered = failure.to_string();
            assert!(
                rendered.to_lowercase().contains("cancel"),
                "the run must report cancellation, got: {rendered}"
            );
        }

        // ── The token has to reach the runner ────────────────────────────────

        /// The discriminating test: proves the *service's* token is what a
        /// provider command is run with.
        ///
        /// Two `vars` rather than two entries on purpose. `resolve_content` loops
        /// over an entry's bindings with no cancellation check between them, so
        /// the second command is dispatched with whatever token was threaded and
        /// nothing else can intercept it. Two *entries* would be caught by the
        /// between-entries guard in `handle_apply` and would prove nothing about
        /// what the runner received.
        ///
        /// This does lean on that absence of a guard inside the `vars` loop. If
        /// one is ever added there, this test stops discriminating and starts
        /// passing for the wrong reason — rewrite it rather than trusting it.
        #[tokio::test]
        async fn the_live_token_reaches_the_command_runner() {
            let dirs = TestDirs::new();
            let target = dirs.target_dir.join("credentials");
            template_package(
                &dirs.package_dir,
                target.to_str().unwrap(),
                "one={{ alpha }}\ntwo={{ beta }}\n",
                &[("alpha", "op read a"), ("beta", "op read b")],
            );

            let token = CancellationToken::new();
            // Cancels during the first binding's command, succeeds for both.
            let runner = TokenObservingRunner::new(
                &token,
                vec![CallEffect {
                    cancel: true,
                    fail_as_cancelled: false,
                }],
            );
            let service = dirs.service_with_runner_and_token(runner.clone(), token);

            let _ = collect_events(service.apply_all(ApplyOptions::default()).await).await;

            // Control: both bindings really ran, so the observation below is of
            // two real dispatches and not of an empty vector.
            assert_eq!(
                runner.calls(),
                vec!["op read a".to_string(), "op read b".to_string()],
                "both bindings must run for this test to observe anything"
            );
            assert_eq!(
                runner.observed(),
                vec![false, true],
                "the second command must be handed the token the first one cancelled; \
                 [false, false] means a fresh token was passed to the resolve path"
            );
        }

        /// The third window: cancellation landing *between two bindings*.
        ///
        /// Neither guard in `handle_apply` covers this. The between-entries guard
        /// has already run for this entry, and the `Failed`-arm guard has not
        /// been reached — `resolve_content` is midway through the entry's `vars`,
        /// and it iterates them with no cancellation check of its own (the same
        /// absence `the_live_token_reaches_the_command_runner` relies on).
        ///
        /// What closes it is the runner: `ShellCommandRunner::run_buffered`
        /// checks the token before spawning and returns `Cancelled`, which
        /// becomes a resolve failure and lands in the `Failed` arm, where
        /// cancellation is read before `stop_on_error`. So the window is covered,
        /// but only because a live token reaches the runner — which is the very
        /// thing this unit fixed. Before it, the fresh token was never cancelled,
        /// so the pre-spawn check could never fire and the second binding would
        /// have run happily after the user pressed Ctrl+C.
        ///
        /// The fake models that pre-spawn refusal explicitly rather than
        /// inheriting it, because a test double that answers a cancelled token
        /// with success would show this window as closed when it is not.
        #[tokio::test]
        async fn a_cancellation_between_two_bindings_is_reported_honestly() {
            let dirs = TestDirs::new();
            let target = dirs.target_dir.join("credentials");
            template_package(
                &dirs.package_dir,
                target.to_str().unwrap(),
                "one={{ alpha }}\ntwo={{ beta }}\n",
                &[("alpha", "op read a"), ("beta", "op read b")],
            );

            let token = CancellationToken::new();
            // Cancels after the first binding succeeds; the second is then handed
            // an already-cancelled token and is refused, as the real runner would.
            let runner = TokenObservingRunner::new(
                &token,
                vec![CallEffect {
                    cancel: true,
                    fail_as_cancelled: false,
                }],
            )
            .refusing_a_cancelled_token();
            let service = dirs.service_with_runner_and_token(runner.clone(), token);

            let events = collect_events(service.apply_all(ApplyOptions::default()).await).await;

            // Control: the run really did reach the second binding. Without this,
            // the assertions below would also hold if it had stopped at the first.
            assert_eq!(
                runner.calls(),
                vec!["op read a".to_string(), "op read b".to_string()],
                "the cancellation has to land between the two bindings"
            );

            assert_cancelled(&events);

            let rendered = format!("{events:?}");
            assert!(
                !rendered.contains("stop_on_error"),
                "a mid-bindings cancellation must not be blamed on the spec: {rendered}"
            );
            assert!(
                !target.exists(),
                "a half-resolved template must never be written"
            );
        }

        // ── Between entries ──────────────────────────────────────────────────

        #[tokio::test]
        async fn a_cancelled_token_stops_apply_before_running_a_provider_command() {
            let dirs = TestDirs::new();
            let target = dirs.target_dir.join("credentials");
            provider_package(&dirs.package_dir, target.to_str().unwrap(), "op read x");

            let token = CancellationToken::new();
            token.cancel();
            let runner = TokenObservingRunner::new(&token, Vec::new());
            let service = dirs.service_with_runner_and_token(runner.clone(), token);

            let events = collect_events(service.apply_all(ApplyOptions::default()).await).await;

            assert_eq!(
                runner.calls(),
                Vec::<String>::new(),
                "a cancelled run must not start a provider command"
            );
            assert!(
                !target.exists(),
                "nothing may be written after cancellation"
            );
            assert_cancelled(&events);
        }

        /// The positive control for the test above.
        ///
        /// Without it, that test passes just as well if apply were failing for
        /// some reason having nothing to do with the token.
        #[tokio::test]
        async fn an_uncancelled_token_runs_the_provider_command() {
            let dirs = TestDirs::new();
            let target = dirs.target_dir.join("credentials");
            provider_package(&dirs.package_dir, target.to_str().unwrap(), "op read x");

            let token = CancellationToken::new();
            let runner = TokenObservingRunner::new(&token, Vec::new());
            let service = dirs.service_with_runner_and_token(runner.clone(), token);

            let events = collect_events(service.apply_all(ApplyOptions::default()).await).await;

            assert_eq!(runner.calls(), vec!["op read x".to_string()]);
            assert_eq!(std::fs::read_to_string(&target).unwrap(), SECRET);
            assert!(
                matches!(
                    get_operation_result(&events),
                    Some(OperationResult::Success(_))
                ),
                "got: {events:?}"
            );
        }

        // ── Mid-command ──────────────────────────────────────────────────────

        /// A command killed by Ctrl+C must not be blamed on the package file.
        ///
        /// `stop_on_error` defaults to **true**, and a cancelled command fails —
        /// so the failure arm reaches `stop_on_error`'s explanation first and the
        /// run reports "Stopped after failing to apply dotfile 'X' (stop_on_error
        /// is enabled)". That names the user's own interrupt as a spec problem
        /// and sends them looking for one. The between-entries guard cannot help
        /// here: the break happens in the same iteration the command died in.
        #[tokio::test]
        async fn a_command_killed_by_cancellation_is_not_reported_as_a_spec_failure() {
            let dirs = TestDirs::new();
            let target = dirs.target_dir.join("credentials");
            provider_package(&dirs.package_dir, target.to_str().unwrap(), "op read x");

            let token = CancellationToken::new();
            let runner = TokenObservingRunner::cancelling_and_failing(&token);
            let service = dirs.service_with_runner_and_token(runner.clone(), token);

            let events = collect_events(service.apply_all(ApplyOptions::default()).await).await;

            // The command really was dispatched and really did die, so what
            // follows is about attribution and not about an entry that never ran.
            assert_eq!(runner.calls(), vec!["op read x".to_string()]);

            // The terminal event exists and names cancellation — this is also the
            // assertion that a cancelled run still completes its stream rather
            // than dropping it.
            assert_cancelled(&events);

            let rendered = format!("{events:?}");
            assert!(
                !rendered.contains("stop_on_error"),
                "cancellation must not be attributed to stop_on_error, got: {rendered}"
            );
        }

        /// The fourth window: cancellation arriving once the *last* entry has
        /// started, where the loop's guard can never run again.
        ///
        /// This is the case the three-entry test below sets up deliberately to
        /// avoid — "with the provider last, the loop ends on its own and the
        /// guard is never reached". That sentence describes a real hole, and this
        /// is the follow-up it was owed.
        ///
        /// Distinct from the other three because nothing *fails* here: the
        /// command succeeds despite the cancellation, so no failure arm is
        /// reached and no guard is left to run. Without the post-loop check the
        /// run reports `Success` for a run the user interrupted — and for a
        /// provider entry that means a credential on disk with nothing in the
        /// stream mentioning Ctrl+C at all.
        ///
        /// The target is asserted **present** on purpose. Cancelling does not
        /// unwrite a file that was already written; the fix is to stop reporting
        /// the run as clean, not to pretend the write did not happen.
        #[tokio::test]
        async fn a_cancellation_during_the_last_entry_is_still_reported() {
            let dirs = TestDirs::new();
            let target = dirs.target_dir.join("credentials");
            provider_package(&dirs.package_dir, target.to_str().unwrap(), "op read x");

            let token = CancellationToken::new();
            // Cancels while the command runs, but the command still succeeds —
            // so the entry deploys and the loop then ends with nothing left to
            // check.
            let runner = TokenObservingRunner::new(
                &token,
                vec![CallEffect {
                    cancel: true,
                    fail_as_cancelled: false,
                }],
            );
            let service = dirs.service_with_runner_and_token(runner.clone(), token);

            let events = collect_events(service.apply_all(ApplyOptions::default()).await).await;

            // Control: the entry really did run and really did deploy, so this is
            // the silent-success window and not an entry that never started.
            assert_eq!(runner.calls(), vec!["op read x".to_string()]);
            assert!(
                target.exists(),
                "the write happened before the cancellation was noticed"
            );

            assert_cancelled(&events);
        }

        // ── What a cancelled run leaves behind ───────────────────────────────

        /// Cancelling must not discard the record of what was already deployed.
        ///
        /// The run breaks out of the loop rather than returning, so deploy state
        /// still reaches disk. Returning early would leave the files in place
        /// with no record of them, and the next drift check would report
        /// correctly-deployed files as untracked.
        ///
        /// Three entries, because the invariant needs an entry *after* the
        /// cancelling one: with the provider last, the loop ends on its own and
        /// the guard is never reached. Not a dry run, because dry runs skip
        /// saving state entirely — which would make the assertion pass for a
        /// reason that has nothing to do with cancellation.
        #[tokio::test]
        async fn a_cancelled_apply_still_records_what_it_already_deployed() {
            let dirs = TestDirs::new();
            let first = dirs.target_dir.join("first.conf");
            let secret_target = dirs.target_dir.join("credentials");
            let third = dirs.target_dir.join("third.conf");

            std::fs::create_dir_all(dirs.package_dir.join("creds")).unwrap();
            std::fs::write(dirs.package_dir.join("creds/first"), "first\n").unwrap();
            std::fs::write(dirs.package_dir.join("creds/third"), "third\n").unwrap();
            let yaml = format!(
                "name: creds\nenvironments:\n  test:\n    install: \"echo i\"\ndotfiles:\n  \
                 - source: \"creds/first\"\n    target: \"{}\"\n  \
                 - command: \"op read x\"\n    target: \"{}\"\n  \
                 - source: \"creds/third\"\n    target: \"{}\"\n",
                first.display(),
                secret_target.display(),
                third.display(),
            );
            std::fs::write(dirs.package_dir.join("creds.yml"), yaml).unwrap();

            let token = CancellationToken::new();
            // Cancels during the provider command but lets it succeed, so the
            // run reaches the third entry's guard rather than the failure arm.
            let runner = TokenObservingRunner::new(
                &token,
                vec![CallEffect {
                    cancel: true,
                    fail_as_cancelled: false,
                }],
            );
            let service = dirs.service_with_runner_and_token(runner, token);

            let events = collect_events(service.apply_all(ApplyOptions::default()).await).await;

            assert_cancelled(&events);
            assert!(
                first.exists(),
                "the entry before the cancellation must have deployed"
            );
            assert!(
                !third.exists(),
                "the entry after the cancellation must not deploy"
            );

            let state = std::fs::read_to_string(state_file(&dirs))
                .expect("deploy state must survive a cancelled run");
            assert!(
                state.contains("creds/first"),
                "what was deployed before the cancellation has to stay recorded, \
                 or the next drift check calls it untracked: {state}"
            );
            assert!(
                !state.contains("creds/third"),
                "nothing may be recorded for an entry that never deployed: {state}"
            );
        }
    }

    /// What a noisy login shell puts in a credentials file (selfie-evf9).
    ///
    /// The unit tests in `commands::shell` assert what the runner returns. These
    /// assert the thing that actually matters: the bytes on disk, through the
    /// whole service, with a real shell process in the middle.
    ///
    /// The shell is a script standing in for the user's, so nothing here reads or
    /// writes the developer's own configuration. See `commands::shell`'s
    /// `content_tests` for why that is a faithful stand-in.
    #[cfg(unix)]
    mod noisy_shell {
        use super::*;
        use selfie::commands::ShellCommandRunner;
        use std::time::Duration;

        /// A shell that writes before, during and after the command it is given.
        fn noisy_runner(dir: &std::path::Path) -> ShellCommandRunner {
            let path = dir.join("noisy-shell");
            test_common::write_executable(
                &path,
                "#!/bin/sh\n\
                 printf '%s' 'LEADBANNER'\n\
                 /bin/sh -c 'sleep 0.3; printf BACKGROUNDNOISE' &\n\
                 trap 'printf TRAILCHATTER' EXIT\n\
                 shift $(($# - 1)); eval \"$1\"\n",
            );

            ShellCommandRunner::new(path.to_str().unwrap(), Duration::from_secs(30))
        }

        /// Where the command reads the credential from.
        ///
        /// The command must not *contain* the secret: a package file's command
        /// string is quoted back in events and errors on purpose, so a fixture
        /// that spells the credential into it fails the leak scan for a reason
        /// that has nothing to do with what is under test. A real provider is
        /// `op read …`, which names a credential without being one.
        fn secret_source(dir: &std::path::Path) -> PathBuf {
            let path = dir.join("vault-stand-in");
            std::fs::write(&path, SECRET).unwrap();
            path
        }

        #[tokio::test]
        async fn a_noisy_shell_puts_nothing_of_its_own_in_the_deployed_file() {
            let dirs = TestDirs::new();
            let target = dirs.target_dir.join("credentials");
            // Slow enough for the backgrounded writer to land mid-command. A
            // command that returns instantly wins the race by accident, and the
            // test would pass without separating anything.
            let source = secret_source(dirs.package_dir.as_path());
            provider_package(
                &dirs.package_dir,
                target.to_str().unwrap(),
                &format!("sleep 0.6; cat {}", source.display()),
            );
            let service = dirs.service_with_runner_and_token(
                noisy_runner(dirs.package_dir.as_path()),
                CancellationToken::new(),
            );

            let events = collect_events(service.apply_all(ApplyOptions::default()).await).await;

            // The positive control: the credential really was produced and
            // written, so the scans below are not passing over an empty file.
            let deployed = std::fs::read_to_string(&target).unwrap();
            assert_eq!(deployed, SECRET);
            for noise in ["LEADBANNER", "BACKGROUNDNOISE", "TRAILCHATTER"] {
                assert!(
                    !deployed.contains(noise),
                    "the shell's own {noise} reached the credentials file: {deployed:?}"
                );
            }
            assert!(
                events
                    .iter()
                    .any(|e| matches!(e, PackageEvent::DotfileDeployed { .. })),
                "the entry did not deploy: {events:?}"
            );
            assert_no_event_mentions(&events, SECRET);
        }

        #[tokio::test]
        async fn a_command_that_installs_an_exit_trap_deploys_and_warns() {
            // selfie cannot find the end of this command's output, so what the
            // command's own trap prints is appended to the credential. It still
            // deploys — the command works and the user asked for it — but the run
            // says so rather than reporting a clean success.
            let dirs = TestDirs::new();
            let target = dirs.target_dir.join("credentials");
            let source = secret_source(dirs.package_dir.as_path());
            provider_package(
                &dirs.package_dir,
                target.to_str().unwrap(),
                &format!("trap 'printf MYCLEANUP' EXIT; cat {}", source.display()),
            );
            let service = dirs.service_with_runner_and_token(
                noisy_runner(dirs.package_dir.as_path()),
                CancellationToken::new(),
            );

            let events = collect_events(service.apply_all(ApplyOptions::default()).await).await;

            assert_eq!(
                std::fs::read_to_string(&target).unwrap(),
                format!("{SECRET}MYCLEANUP"),
                "the appended bytes are what the warning is about"
            );
            let warned = events
                .iter()
                .any(|event| format!("{event:?}").contains("could not establish where the output"));
            assert!(warned, "appending foreign bytes silently: {events:?}");
            assert_no_event_mentions(&events, SECRET);
        }
    }
}

/// Deploying a repository file onto a symlinked target, and the permissions of the
/// deploy state file.
///
/// Unix-only: symlink following and permission bits are not observable through
/// `MockFileSystem`, whose `write_file` is a stub with no filesystem behind it. The
/// assertions that matter here — where the bytes actually landed, and what mode the
/// state file carries — cannot be expressed against it. Everything runs inside a
/// `TempDir`, so nothing outside it is touched.
#[cfg(unix)]
mod symlinked_targets {
    use super::*;
    use std::os::unix::fs::PermissionsExt as _;
    use std::path::Path;

    fn repo_source(dirs: &TestDirs, relative: &str, content: &str) {
        let path = dirs.package_dir.join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }

    fn is_symlink(path: &Path) -> bool {
        std::fs::symlink_metadata(path)
            .unwrap()
            .file_type()
            .is_symlink()
    }

    fn warnings(events: &[PackageEvent]) -> Vec<String> {
        events
            .iter()
            .filter_map(|event| match event {
                PackageEvent::Warning { message, .. } => Some(message.clone()),
                _ => None,
            })
            .collect()
    }

    /// `(deployed, skipped, conflict)` — the conflict count is part of the tuple
    /// because a refusal deliberately lands in a different bucket from a conflict.
    fn deploy_counts(events: &[PackageEvent]) -> (usize, usize, usize) {
        match get_operation_result(events).expect("no Completed event") {
            OperationResult::Success(OperationSuccess::DotfilesApplied {
                deployed_count,
                skipped_count,
                conflict_count,
                ..
            }) => (*deployed_count, *skipped_count, *conflict_count),
            other => panic!("expected DotfilesApplied, got {other:?}"),
        }
    }

    /// The refusal must not fire on a target apply had no reason to write to.
    ///
    /// Deploying by copying does not forbid a symlinked target; it declines to
    /// *write through* one. Someone who keeps their dotfiles symlinked from
    /// elsewhere and is already in sync should see nothing at all. Without this,
    /// `a_symlinked_target_is_refused_on_a_routine_repo_update` would also pass
    /// against an implementation that refused every symlinked target outright —
    /// confirmed by mutation.
    #[tokio::test]
    async fn an_in_sync_symlinked_target_is_left_alone_and_not_reported() {
        let dirs = TestDirs::new();
        repo_source(&dirs, "myapp/config.toml", "SAME");
        let destination = dirs.target_dir.join("destination");
        std::fs::write(&destination, "SAME").unwrap();
        let target = dirs.target_dir.join("config.toml");
        std::os::unix::fs::symlink(&destination, &target).unwrap();
        create_package_with_dotfiles(
            &dirs.package_dir,
            "myapp",
            &[("myapp/config.toml", target.to_str().unwrap())],
        );

        let events = collect_events(dirs.service().apply_all(ApplyOptions::default()).await).await;

        assert!(is_symlink(&target), "an untouched target must stay a link");
        assert_eq!(std::fs::read_to_string(&destination).unwrap(), "SAME");
        assert!(
            warnings(&events).is_empty(),
            "nothing was written, so nothing should be reported: {:?}",
            warnings(&events)
        );
    }

    /// The routine path, and the reason a refusal beats replacing the link.
    ///
    /// `RepoChanged` routes to `DeployDecision::Deploy` with no conflict and no
    /// `--yes`, so this is the ordinary apply after any edit to a repository file
    /// — not a rare case. Replacing the link here would silently discard it and
    /// orphan its destination on the first apply after any edit.
    #[tokio::test]
    async fn a_symlinked_target_is_refused_on_a_routine_repo_update() {
        let dirs = TestDirs::new();
        repo_source(&dirs, "myapp/config.toml", "V1");
        let destination = dirs.target_dir.join("destination");
        std::fs::write(&destination, "V1").unwrap();
        let target = dirs.target_dir.join("config.toml");
        std::os::unix::fs::symlink(&destination, &target).unwrap();
        create_package_with_dotfiles(
            &dirs.package_dir,
            "myapp",
            &[("myapp/config.toml", target.to_str().unwrap())],
        );

        // Records deploy state, so the second apply sees RepoChanged rather than
        // an untracked entry.
        let _ = collect_events(dirs.service().apply_all(ApplyOptions::default()).await).await;
        repo_source(&dirs, "myapp/config.toml", "V2");

        let events = collect_events(dirs.service().apply_all(ApplyOptions::default()).await).await;

        assert_eq!(
            std::fs::read_to_string(&destination).unwrap(),
            "V1",
            "the link's destination must not be written through"
        );
        assert!(is_symlink(&target), "the link itself must be left in place");
        let warnings = warnings(&events);
        assert!(
            warnings
                .iter()
                .any(|w| w.contains("is a symlink")
                    && w.contains(&destination.display().to_string())),
            "the refusal must name where the link points: {warnings:?}"
        );
        assert_eq!(
            deploy_counts(&events),
            (0, 1, 0),
            "a refusal is a skip, not a deploy"
        );
    }

    /// A dangling link is refused too, and its destination is not created.
    ///
    /// This is the case a caller cannot detect by inspecting the target
    /// afterwards: `path_exists` follows the link and reports false, so apply
    /// decides to deploy with no conflict, and `fs::write` would then create the
    /// file at whatever path the link names.
    #[tokio::test]
    async fn a_dangling_symlinked_target_does_not_create_its_destination() {
        let dirs = TestDirs::new();
        repo_source(&dirs, "myapp/config.toml", "REPO");
        let never_created = dirs.target_dir.join("never-created");
        let target = dirs.target_dir.join("config.toml");
        std::os::unix::fs::symlink(&never_created, &target).unwrap();
        create_package_with_dotfiles(
            &dirs.package_dir,
            "myapp",
            &[("myapp/config.toml", target.to_str().unwrap())],
        );

        let events = collect_events(dirs.service().apply_all(ApplyOptions::default()).await).await;

        assert!(
            !never_created.exists(),
            "the link's destination was created"
        );
        assert!(is_symlink(&target));
        assert_eq!(deploy_counts(&events), (0, 1, 0));
    }

    /// `--yes` resolves a conflict; it does not authorize writing through a link.
    ///
    /// The two are independent decisions, and the flag speaks only to the first.
    #[tokio::test]
    async fn auto_accept_does_not_override_the_refusal() {
        let dirs = TestDirs::new();
        repo_source(&dirs, "myapp/config.toml", "REPO");
        let destination = dirs.target_dir.join("destination");
        std::fs::write(&destination, "USER EDITED").unwrap();
        let target = dirs.target_dir.join("config.toml");
        std::os::unix::fs::symlink(&destination, &target).unwrap();
        create_package_with_dotfiles(
            &dirs.package_dir,
            "myapp",
            &[("myapp/config.toml", target.to_str().unwrap())],
        );

        let options = ApplyOptions {
            auto_accept: true,
            ..Default::default()
        };
        let events = collect_events(dirs.service().apply_all(options).await).await;

        assert_eq!(
            std::fs::read_to_string(&destination).unwrap(),
            "USER EDITED"
        );
        assert!(is_symlink(&target));
        assert_eq!(deploy_counts(&events), (0, 1, 0));
    }

    /// A preview must report the refusal, not promise a deploy that will not happen.
    ///
    /// The same ordering rule the secret path already follows: checks that a real
    /// apply would refuse on come before the dry-run short-circuit, so a preview
    /// describes the run you are about to perform rather than a different one.
    #[tokio::test]
    async fn a_dry_run_reports_the_refusal_rather_than_previewing_a_deploy() {
        let dirs = TestDirs::new();
        repo_source(&dirs, "myapp/config.toml", "V1");
        let destination = dirs.target_dir.join("destination");
        std::fs::write(&destination, "V1").unwrap();
        let target = dirs.target_dir.join("config.toml");
        std::os::unix::fs::symlink(&destination, &target).unwrap();
        create_package_with_dotfiles(
            &dirs.package_dir,
            "myapp",
            &[("myapp/config.toml", target.to_str().unwrap())],
        );

        // Same setup as the routine-update test, so the entry reaches the deploy
        // decision rather than being reported as a conflict: tracked, in sync, and
        // then the repository file changes.
        let _ = collect_events(dirs.service().apply_all(ApplyOptions::default()).await).await;
        repo_source(&dirs, "myapp/config.toml", "V2");

        let options = ApplyOptions {
            dry_run: true,
            ..Default::default()
        };
        let events = collect_events(dirs.service().apply_all(options).await).await;

        let warnings = warnings(&events);
        assert!(
            warnings.iter().any(|w| w.contains("is a symlink")),
            "a dry run must say the entry would be refused: {warnings:?}"
        );
        assert!(
            !events.iter().any(
                |e| matches!(e, PackageEvent::DotfileSkipped { reason, .. } if reason == "dry run")
            ),
            "the entry must not also be previewed as a deploy: {events:?}"
        );
        assert_eq!(std::fs::read_to_string(&destination).unwrap(), "V1");
    }

    /// The user is never asked a question whose answer cannot be honored.
    ///
    /// A conflict on a symlinked target used to reach the interactive resolver,
    /// which showed a diff and asked whether to overwrite — and then refused the
    /// write whichever way the user answered. The refusal is settled before the
    /// resolver is consulted, so the prompt never happens.
    #[tokio::test]
    async fn a_conflicting_symlinked_target_is_refused_without_prompting() {
        use selfie::dotfile_service::port::{ConflictDetail, ConflictResolution, ConflictResolver};
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};

        struct RecordsBeingAsked(Arc<AtomicBool>);
        impl ConflictResolver for RecordsBeingAsked {
            fn resolve(&self, _target: &str, _detail: ConflictDetail<'_>) -> ConflictResolution {
                self.0.store(true, Ordering::SeqCst);
                ConflictResolution::Accept
            }
        }

        let dirs = TestDirs::new();
        repo_source(&dirs, "myapp/config.toml", "REPO");
        let destination = dirs.target_dir.join("destination");
        std::fs::write(&destination, "USER EDITED").unwrap();
        let target = dirs.target_dir.join("config.toml");
        std::os::unix::fs::symlink(&destination, &target).unwrap();
        create_package_with_dotfiles(
            &dirs.package_dir,
            "myapp",
            &[("myapp/config.toml", target.to_str().unwrap())],
        );

        let asked = Arc::new(AtomicBool::new(false));
        let options = ApplyOptions {
            conflict_resolver: Some(Arc::new(RecordsBeingAsked(Arc::clone(&asked)))),
            ..Default::default()
        };
        let events = collect_events(dirs.service().apply_all(options).await).await;

        assert!(
            !asked.load(Ordering::SeqCst),
            "the resolver was asked to settle a conflict that would be refused anyway"
        );
        assert_eq!(
            std::fs::read_to_string(&destination).unwrap(),
            "USER EDITED"
        );
        assert!(is_symlink(&target));
        // Counted as skipped, NOT as a conflict, though the content differs and the
        // entry would otherwise have been one. A conflict is a question for the
        // user; this one is already settled, so it is not asked. Asserted so the
        // bucket cannot move back without someone deciding to.
        assert_eq!(
            deploy_counts(&events),
            (0, 1, 0),
            "a refused entry must be a skip, not a conflict"
        );
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, PackageEvent::DotfileConflict { .. })),
            "no conflict should be reported for a refused entry: {events:?}"
        );
    }

    /// The writer refuses on its own, with the hoisted check taken out of the way.
    ///
    /// This is the TOCTOU defense, and it is the half of the fix that nothing else
    /// observes. With the check in `handle_apply` doing its job, reverting
    /// `perform_deploy` to `write_file` fails **no other test in the workspace** —
    /// measured, 787 passing either way. So without this test a future reader can
    /// find the writer redundant, delete it, see a green suite, and have removed the
    /// only protection against a link planted *during* an apply.
    ///
    /// Blinding `symlink_refusal` reproduces that race deterministically: the check
    /// sees nothing, the write goes ahead, and `O_NOFOLLOW` has to catch it. No
    /// sleeping, no threads, no flakiness.
    ///
    /// `auto_accept` is load-bearing. Without it a differing target takes the
    /// conflict branch and never reaches `perform_deploy`, so a weaker version of
    /// this test — one that only checked the victim file was untouched — would pass
    /// without exercising the writer at all.
    #[tokio::test]
    async fn the_writer_refuses_even_when_the_check_is_blinded() {
        use selfie::config::SelfieConfigBuilder;
        use selfie::dotfile_service::service::DotfileServiceImpl;
        use selfie::fs::{FileSystem, FileSystemError, RealFileSystem, TargetPath};
        use selfie::package::repository::YamlPackageRepository;
        use std::path::PathBuf;
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        /// `RealFileSystem` with the symlink *report* suppressed and nothing else
        /// changed — an attacker who wins the race between the check and the write.
        ///
        /// Counts writes so the test can prove the writer was actually reached.
        /// Without that, a refactor that detects the link by some route other than
        /// `symlink_refusal` would leave this decorator blinding nothing, the test
        /// would pass having exercised none of what it is named for, and
        /// `O_NOFOLLOW` would become deletable again — the exact hole this guards.
        #[derive(Clone, Debug)]
        struct BlindToSymlinks(RealFileSystem, Arc<AtomicUsize>);

        impl FileSystem for BlindToSymlinks {
            fn symlink_refusal(&self, _path: &TargetPath) -> Option<FileSystemError> {
                None
            }

            fn read_file(&self, path: &Path) -> Result<String, FileSystemError> {
                self.0.read_file(path)
            }
            fn read_file_bytes(&self, path: &Path) -> Result<Vec<u8>, FileSystemError> {
                self.0.read_file_bytes(path)
            }
            fn write_file(&self, path: &Path, data: &[u8]) -> Result<(), FileSystemError> {
                self.0.write_file(path, data)
            }
            fn write_file_private(
                &self,
                path: &TargetPath,
                data: &[u8],
            ) -> Result<(), FileSystemError> {
                self.0.write_file_private(path, data)
            }
            fn write_file_no_follow(
                &self,
                path: &TargetPath,
                data: &[u8],
            ) -> Result<(), FileSystemError> {
                self.1.fetch_add(1, Ordering::SeqCst);
                self.0.write_file_no_follow(path, data)
            }
            fn is_owner_only(&self, path: &TargetPath) -> Result<bool, FileSystemError> {
                self.0.is_owner_only(path)
            }
            fn remove_file(&self, path: &Path) -> Result<(), FileSystemError> {
                self.0.remove_file(path)
            }
            fn path_exists(&self, path: &Path) -> bool {
                self.0.path_exists(path)
            }
            fn expand_path(&self, path: &Path) -> Result<PathBuf, FileSystemError> {
                self.0.expand_path(path)
            }
            fn list_directory(&self, path: &Path) -> Result<Vec<PathBuf>, FileSystemError> {
                self.0.list_directory(path)
            }
            fn canonicalize(&self, path: &Path) -> Result<PathBuf, FileSystemError> {
                self.0.canonicalize(path)
            }
            fn config_dir(&self) -> Result<PathBuf, FileSystemError> {
                self.0.config_dir()
            }
        }

        let dirs = TestDirs::new();
        repo_source(&dirs, "myapp/config.toml", "ATTACKER_PAYLOAD");
        let destination = dirs.target_dir.join("destination");
        std::fs::write(&destination, "ORIGINAL").unwrap();
        let target = dirs.target_dir.join("config.toml");
        std::os::unix::fs::symlink(&destination, &target).unwrap();
        create_package_with_dotfiles(
            &dirs.package_dir,
            "myapp",
            &[("myapp/config.toml", target.to_str().unwrap())],
        );

        let config = SelfieConfigBuilder::default()
            .environment("test")
            .package_directory(&dirs.package_dir)
            .dotfiles_directory(dirs.dotfiles_dir.clone())
            .state_directory(dirs.state_dir.clone())
            .build();
        let repo = YamlPackageRepository::new(RealFileSystem, config.package_directory().clone());
        let writes = Arc::new(AtomicUsize::new(0));
        let service = DotfileServiceImpl::new(
            repo,
            BlindToSymlinks(RealFileSystem, Arc::clone(&writes)),
            FakeCommandRunner::new(),
            config,
            CancellationToken::new(),
        );

        let options = ApplyOptions {
            // Without this the entry is a conflict and never reaches the write.
            auto_accept: true,
            ..Default::default()
        };
        let events = collect_events(service.apply_all(options).await).await;

        // The control. Everything below is about what the writer did, so the test
        // has to establish that the writer ran at all — otherwise a green result
        // could mean the refusal came from somewhere else entirely.
        assert!(
            writes.load(Ordering::SeqCst) > 0,
            "the write site was never reached, so this test proved nothing about it"
        );
        assert_eq!(
            std::fs::read_to_string(&destination).unwrap(),
            "ORIGINAL",
            "the writer let the content through to the link's destination"
        );
        assert!(is_symlink(&target));
        let warnings = warnings(&events);
        assert!(
            warnings.iter().any(|w| w.contains("is a symlink")),
            "the writer's own refusal must still be reported: {warnings:?}"
        );
        assert_eq!(deploy_counts(&events), (0, 1, 0));
    }

    /// An ordinary target is unaffected by any of the above.
    #[tokio::test]
    async fn an_ordinary_target_still_deploys() {
        let dirs = TestDirs::new();
        repo_source(&dirs, "myapp/config.toml", "REPO");
        let target = dirs.target_dir.join("config.toml");
        create_package_with_dotfiles(
            &dirs.package_dir,
            "myapp",
            &[("myapp/config.toml", target.to_str().unwrap())],
        );

        let events = collect_events(dirs.service().apply_all(ApplyOptions::default()).await).await;

        assert_eq!(std::fs::read_to_string(&target).unwrap(), "REPO");
        assert_eq!(deploy_counts(&events), (1, 0, 0));
    }

    /// The deploy state file names every path selfie manages here, so it must not
    /// be readable by anyone but its owner.
    #[tokio::test]
    async fn the_deploy_state_file_is_owner_only() {
        let dirs = TestDirs::new();
        repo_source(&dirs, "myapp/config.toml", "REPO");
        let target = dirs.target_dir.join("config.toml");
        create_package_with_dotfiles(
            &dirs.package_dir,
            "myapp",
            &[("myapp/config.toml", target.to_str().unwrap())],
        );

        let _ = collect_events(dirs.service().apply_all(ApplyOptions::default()).await).await;

        // The control says what an ordinary write produces in this environment.
        // Under a umask of 077 an owner-only assertion passes whatever the code
        // does, so without this the test could be green while proving nothing.
        let control = dirs.state_dir.join("control");
        std::fs::write(&control, b"x").unwrap();
        let control_mode = std::fs::metadata(&control).unwrap().permissions().mode();
        if control_mode & 0o077 == 0 {
            let message = "the ambient umask makes ordinary writes owner-only, so this \
                           cannot tell an owner-only write from a default one";
            assert!(
                std::env::var_os("CI").is_none(),
                "the_deploy_state_file_is_owner_only: {message}"
            );
            eprintln!("SKIP the_deploy_state_file_is_owner_only: {message}");
            return;
        }

        let state_file = dirs.state_dir.join("deploy-state.yml");
        let mode = std::fs::metadata(&state_file).unwrap().permissions().mode();
        // `& 0o077`, not `& 0o007`: group-readable exposes the map to exactly the
        // people it is being kept from on a shared machine.
        assert_eq!(
            mode & 0o077,
            0,
            "group/other bits set on the deploy state file: {:04o}",
            mode & 0o777
        );
    }

    /// A state file left world-readable by an earlier version is corrected.
    #[tokio::test]
    async fn an_existing_world_readable_state_file_is_tightened() {
        let dirs = TestDirs::new();
        repo_source(&dirs, "myapp/config.toml", "REPO");
        let target = dirs.target_dir.join("config.toml");
        create_package_with_dotfiles(
            &dirs.package_dir,
            "myapp",
            &[("myapp/config.toml", target.to_str().unwrap())],
        );
        let state_file = dirs.state_dir.join("deploy-state.yml");
        std::fs::write(&state_file, "deployed: {}\n").unwrap();
        std::fs::set_permissions(&state_file, std::fs::Permissions::from_mode(0o644)).unwrap();

        let _ = collect_events(dirs.service().apply_all(ApplyOptions::default()).await).await;

        // An implementation that opened the existing file and truncated it would
        // leave the old mode in place, since a creation mode applies only when the
        // file is created.
        let mode = std::fs::metadata(&state_file).unwrap().permissions().mode();
        assert_eq!(
            mode & 0o077,
            0,
            "a pre-existing state file kept its permissive mode: {:04o}",
            mode & 0o777
        );
    }
}

/// Consequences of expanding a target without canonicalizing it.
///
/// These are behavior changes rather than fixes, asserted so they are deliberate
/// and visible rather than discovered later.
#[cfg(unix)]
mod target_expansion {
    use selfie::fs::{RealFileSystem, expand_target_path};
    use tempfile::TempDir;

    /// The final component is never resolved, which is what lets the writers see a
    /// symlink at all. See `expand_target_path`'s documentation.
    #[test]
    fn a_symlinked_target_keeps_its_own_path() {
        let temp = TempDir::new().unwrap();
        let destination = temp.path().join("destination");
        std::fs::write(&destination, "x").unwrap();
        let target = temp.path().join("link");
        std::os::unix::fs::symlink(&destination, &target).unwrap();

        let expanded = expand_target_path(&RealFileSystem, target.to_str().unwrap());

        assert_eq!(
            expanded.path(),
            target,
            "the target was resolved to its destination"
        );
    }

    /// Two spellings of one file that differ through a symlinked directory no
    /// longer compare equal.
    ///
    /// Duplicate detection in `dotfiles track` compares expanded paths, so it now
    /// misses this case where canonicalizing used to catch it. Recorded because it
    /// is the regression most likely to tempt someone into putting `expand_path`
    /// back — which would reopen selfie-4m9. Fix it by comparing differently, not
    /// by resolving here.
    #[test]
    fn paths_differing_through_a_symlinked_directory_no_longer_match() {
        let temp = TempDir::new().unwrap();
        let real = temp.path().join("real");
        std::fs::create_dir(&real).unwrap();
        let linked = temp.path().join("linked");
        std::os::unix::fs::symlink(&real, &linked).unwrap();
        std::fs::write(real.join("config"), "x").unwrap();

        let via_link = expand_target_path(&RealFileSystem, linked.join("config").to_str().unwrap());
        let via_real = expand_target_path(&RealFileSystem, real.join("config").to_str().unwrap());

        assert_ne!(via_link, via_real);
    }
}

/// What `dotfiles drift` and `dotfiles track` say about a symlinked target, and
/// what the lexical containment guard does not say about a symlinked source.
///
/// `apply` refuses to write through a symlinked target (selfie-4m9). These cover the
/// two commands that used to be silent about it and the guard that documents a limit
/// it has to keep. Unix-only: `MockFileSystem` has no filesystem behind it, so none
/// of this is observable through it. Everything runs inside a `TempDir`.
#[cfg(unix)]
mod symlink_consistency {
    use super::*;
    use std::path::Path;

    fn repo_source(dirs: &TestDirs, relative: &str, content: &str) {
        let path = dirs.package_dir.join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }

    fn warnings(events: &[PackageEvent]) -> Vec<String> {
        events
            .iter()
            .filter_map(|event| match event {
                PackageEvent::Warning { message, .. } => Some(message.clone()),
                _ => None,
            })
            .collect()
    }

    fn drift_types(events: &[PackageEvent]) -> Vec<String> {
        events
            .iter()
            .filter_map(|event| match event {
                PackageEvent::DotfileDriftDetected { drift_type, .. } => Some(drift_type.clone()),
                _ => None,
            })
            .collect()
    }

    fn symlink_warnings(events: &[PackageEvent]) -> Vec<String> {
        warnings(events)
            .into_iter()
            .filter(|w| w.contains("is a symlink"))
            .collect()
    }

    fn failure_message(events: &[PackageEvent]) -> String {
        match get_operation_result(events).expect("no Completed event") {
            OperationResult::Failure(failure) => failure.to_string(),
            other => panic!("expected a Failure, got {other:?}"),
        }
    }

    /// A package whose one entry targets `target`, with `content` in the repository.
    fn package_targeting(dirs: &TestDirs, content: &str, target: &Path) {
        repo_source(dirs, "myapp/config.toml", content);
        create_package_with_dotfiles(
            &dirs.package_dir,
            "myapp",
            &[("myapp/config.toml", target.to_str().unwrap())],
        );
    }

    /// Deploy normally, then migrate the target to a stow-style link: move the
    /// deployed file aside and symlink to it. Leaves the recorded deploy state
    /// matching the link's destination exactly, which is the state a user reaches by
    /// adopting a symlink layout after using selfie.
    async fn deploy_then_link_aside(dirs: &TestDirs, target: &Path) -> PathBuf {
        collect_events(dirs.service().apply_all(ApplyOptions::default()).await).await;
        let destination = dirs.target_dir.join("destination");
        std::fs::rename(target, &destination).unwrap();
        std::os::unix::fs::symlink(&destination, target).unwrap();
        destination
    }

    // ── drift ───────────────────────────────────────────────────────────────

    /// D1. The case in selfie-qvwq's title: a repository edit that can never reach
    /// the target, reported as `repo changed` on every run forever because the
    /// deploy state can never advance. Drift now names the symlink alongside it.
    #[tokio::test]
    async fn drift_names_the_symlink_when_it_reports_drift() {
        let dirs = TestDirs::new();
        let target = dirs.target_dir.join("config.toml");
        package_targeting(&dirs, "V1", &target);
        let destination = deploy_then_link_aside(&dirs, &target).await;
        repo_source(&dirs, "myapp/config.toml", "V2");

        let events = collect_events(dirs.service().check_drift().await).await;

        assert_eq!(drift_types(&events), vec!["repo changed"]);
        let named = symlink_warnings(&events);
        assert_eq!(named.len(), 1, "expected one refusal, got {named:?}");
        assert!(
            named[0].contains(&*target.to_string_lossy())
                && named[0].contains(&*destination.to_string_lossy()),
            "the refusal must name both the target and where the link points: {named:?}"
        );
    }

    /// D2. Parity in the silent direction: a symlinked target already in sync is one
    /// apply has no reason to write to, and apply says nothing about it
    /// (`an_in_sync_symlinked_target_is_left_alone_and_not_reported`). Drift must not
    /// invent a complaint apply does not make.
    ///
    /// Without this, D1 would also pass against an implementation that warned about
    /// every symlinked target it saw.
    #[tokio::test]
    async fn drift_says_nothing_about_an_in_sync_symlinked_target() {
        let dirs = TestDirs::new();
        let target = dirs.target_dir.join("config.toml");
        package_targeting(&dirs, "V1", &target);
        deploy_then_link_aside(&dirs, &target).await;

        let events = collect_events(dirs.service().check_drift().await).await;

        assert_eq!(drift_types(&events), Vec::<String>::new());
        assert_eq!(
            warnings(&events),
            Vec::<String>::new(),
            "nothing is out of sync, so there is nothing to report"
        );
    }

    /// D3 (row 1a). The fresh-machine case: a target already symlinked into place by
    /// another tool, never deployed by selfie, whose destination differs from the
    /// repository file. Drift classifies it `not tracked` rather than `repo changed`,
    /// so a fix that only handled `RepoChanged` would leave this silent — which is
    /// why the fixture varies along the drift-type axis.
    #[tokio::test]
    async fn drift_names_the_symlink_on_a_never_deployed_target() {
        let dirs = TestDirs::new();
        let destination = dirs.target_dir.join("destination");
        std::fs::write(&destination, "SOMETHING ELSE").unwrap();
        let target = dirs.target_dir.join("config.toml");
        std::os::unix::fs::symlink(&destination, &target).unwrap();
        package_targeting(&dirs, "V1", &target);

        let events = collect_events(dirs.service().check_drift().await).await;

        assert_eq!(drift_types(&events), vec!["not tracked"]);
        assert_eq!(
            symlink_warnings(&events).len(),
            1,
            "a never-deployed symlinked target is refused too: {:?}",
            warnings(&events)
        );
    }

    /// D5 (row 1b). The fixture that separates `deploy_decision` from `drift != None`.
    ///
    /// Never deployed, so drift is `NotTracked` and the entry is reported as drifted
    /// — but the destination's contents already match the repository file, so
    /// `deploy_decision` returns `Skip` and **apply is silent**. Gating the refusal on
    /// the drift type instead of on apply's own decision would warn here, recreating
    /// the drift-vs-apply disagreement in the opposite direction.
    ///
    /// The apply half is asserted in the same test on purpose: the property is that
    /// the two commands agree, and a test that only looked at drift could not see it.
    #[tokio::test]
    async fn drift_is_silent_where_apply_is_silent_on_an_untracked_matching_link() {
        let dirs = TestDirs::new();
        let destination = dirs.target_dir.join("destination");
        std::fs::write(&destination, "SAME BYTES").unwrap();
        let target = dirs.target_dir.join("config.toml");
        std::os::unix::fs::symlink(&destination, &target).unwrap();
        package_targeting(&dirs, "SAME BYTES", &target);

        let drift = collect_events(dirs.service().check_drift().await).await;
        let apply = collect_events(dirs.service().apply_all(ApplyOptions::default()).await).await;

        assert_eq!(
            symlink_warnings(&apply),
            Vec::<String>::new(),
            "control: apply must be silent here, or this fixture proves nothing"
        );
        assert_eq!(
            symlink_warnings(&drift),
            Vec::<String>::new(),
            "drift warned where apply did not"
        );
        assert_eq!(
            drift_types(&drift),
            vec!["not tracked"],
            "control: the entry is still reported as drifted, so a `drift != None` \
             gate really would have fired here"
        );
    }

    /// D4. Drift and apply describe the same refusal with the same sentence, because
    /// they call the same `refusal_warning`. A user who runs one then the other must
    /// not have to work out whether two different messages mean the same thing.
    #[tokio::test]
    async fn drift_and_apply_word_the_refusal_identically() {
        let dirs = TestDirs::new();
        let target = dirs.target_dir.join("config.toml");
        package_targeting(&dirs, "V1", &target);
        deploy_then_link_aside(&dirs, &target).await;
        repo_source(&dirs, "myapp/config.toml", "V2");

        let drift = collect_events(dirs.service().check_drift().await).await;
        let apply = collect_events(dirs.service().apply_all(ApplyOptions::default()).await).await;

        let from_drift = symlink_warnings(&drift);
        let from_apply = symlink_warnings(&apply);
        assert_eq!(from_drift.len(), 1, "drift said nothing: {from_drift:?}");
        assert_eq!(from_apply.len(), 1, "apply said nothing: {from_apply:?}");
        assert_eq!(from_drift[0], from_apply[0]);
    }

    // ── track ───────────────────────────────────────────────────────────────

    /// T1. Tracking a symlinked target is refused rather than recorded.
    ///
    /// There is no configuration in which tracking one does what the user asked:
    /// apply refuses to write through it, so the entry is either permanently inert or
    /// permanently broken. The refusal names the destination, because deciding what
    /// to do about it needs to know where the link goes.
    #[tokio::test]
    async fn tracking_a_symlinked_target_is_refused() {
        let dirs = TestDirs::new();
        let destination = dirs.target_dir.join("real_config");
        std::fs::write(&destination, "content").unwrap();
        let target = dirs.target_dir.join("config.toml");
        std::os::unix::fs::symlink(&destination, &target).unwrap();

        let events = collect_events(
            dirs.service_with_dotfiles()
                .track_standalone("myapp", target.to_str().unwrap())
                .await,
        )
        .await;

        let message = failure_message(&events);
        assert!(
            message.contains("is a symlink") && message.contains(&*destination.to_string_lossy()),
            "the refusal must say symlink and name the destination: {message}"
        );
    }

    /// T2. The refusal lands before every write, which is the part that matters.
    ///
    /// Tracking reads *through* a link, so a refusal placed after any of the three
    /// writes would already have copied the destination's contents into the dotfiles
    /// directory — a file the user never named, and one `selfie sync push` would
    /// commit — written a spec, and recorded a deploy state entry for a deployment
    /// that never happened. T1 cannot see any of that; it only sees the verdict.
    #[tokio::test]
    async fn a_refused_track_writes_nothing() {
        let dirs = TestDirs::new();
        let destination = dirs.target_dir.join("real_config");
        std::fs::write(&destination, "NEVER COPIED ANYWHERE").unwrap();
        let target = dirs.target_dir.join("config.toml");
        std::os::unix::fs::symlink(&destination, &target).unwrap();

        let events = collect_events(
            dirs.service_with_dotfiles()
                .track_standalone("myapp", target.to_str().unwrap())
                .await,
        )
        .await;

        assert!(matches!(
            get_operation_result(&events).expect("no Completed event"),
            OperationResult::Failure(_)
        ));
        assert!(
            !dirs.dotfiles_dir.join("myapp/config.toml").exists(),
            "the link's destination was copied into the dotfiles directory"
        );
        assert!(
            !dirs.dotfiles_dir.join("myapp.yml").exists(),
            "a spec was written for an entry that cannot deploy"
        );
        assert!(
            !dirs.state_dir.join("deploy-state.yml").exists(),
            "deploy state was recorded for a deployment that never happened"
        );
        assert_eq!(
            std::fs::read_to_string(&destination).unwrap(),
            "NEVER COPIED ANYWHERE",
            "control: the destination itself must be untouched"
        );
    }

    /// T3. A dangling link is refused as a symlink, not reported as a missing file.
    ///
    /// `path_exists` follows the link, so the existence check answers "no" for a path
    /// the user can see in their own shell. This is the only fixture on which the
    /// refusal's position relative to that check is observable.
    #[tokio::test]
    async fn tracking_a_dangling_symlink_says_symlink_not_missing() {
        let dirs = TestDirs::new();
        let target = dirs.target_dir.join("config.toml");
        std::os::unix::fs::symlink(dirs.target_dir.join("never_created"), &target).unwrap();

        let events = collect_events(
            dirs.service_with_dotfiles()
                .track_standalone("myapp", target.to_str().unwrap())
                .await,
        )
        .await;

        let message = failure_message(&events);
        assert!(
            message.contains("is a symlink"),
            "a dangling link is still a link: {message}"
        );
        assert!(
            !message.contains("does not exist"),
            "the path the user typed does exist; calling it missing sends them \
             looking for the wrong problem: {message}"
        );
    }

    /// T4. The other track handler refuses too.
    ///
    /// `track_for_package` is a separate function with its own copy of the read, the
    /// write and the state record, so a fix applied to one handler leaves the other
    /// exfiltrating the destination exactly as before.
    #[tokio::test]
    async fn tracking_a_symlinked_target_for_a_package_is_refused() {
        let dirs = TestDirs::new();
        std::fs::write(
            dirs.package_dir.join("myapp.yml"),
            "name: myapp\nenvironments:\n  test:\n    install: \"echo installed\"\n",
        )
        .unwrap();
        let destination = dirs.target_dir.join("real_config");
        std::fs::write(&destination, "NEVER COPIED ANYWHERE").unwrap();
        let target = dirs.target_dir.join("config.toml");
        std::os::unix::fs::symlink(&destination, &target).unwrap();

        let events = collect_events(
            dirs.service()
                .track_for_package("myapp", target.to_str().unwrap())
                .await,
        )
        .await;

        let message = failure_message(&events);
        assert!(
            message.contains("is a symlink"),
            "track_for_package accepted a symlinked target: {message}"
        );
        assert!(
            !dirs.package_dir.join("myapp/config.toml").exists(),
            "the link's destination was copied into the package directory"
        );
    }

    // ── the lexical containment guard (selfie-86o) ──────────────────────────

    /// The containment guard is lexical, so a symlink inside the package directory
    /// escapes it. Recorded as an executable fact rather than only as prose.
    ///
    /// **This test asserts a limitation, and it is meant to fail if the limitation is
    /// removed.** Anyone who makes the guard symlink-aware should delete it together
    /// with the paragraph on `crate::paths::is_within` that it pins — the two must not
    /// be able to disagree.
    ///
    /// Both forms are covered because they are not equally visible. With a symlinked
    /// **file** the escape is at the final component. With a symlinked **directory**
    /// it is not: `symlink_metadata` on the full source path reports a regular file,
    /// asserted below, so a guard that inspected only the final component would report
    /// containment with complete confidence and let this through unchanged.
    #[tokio::test]
    async fn a_symlinked_source_escapes_the_containment_guard() {
        // A symlinked file inside the package directory.
        {
            let dirs = TestDirs::new();
            let outside = dirs.state_dir.join("outside.txt");
            std::fs::write(&outside, "OUTSIDE THE PACKAGE DIRECTORY").unwrap();
            std::fs::create_dir_all(dirs.package_dir.join("myapp")).unwrap();
            std::os::unix::fs::symlink(&outside, dirs.package_dir.join("myapp/config.toml"))
                .unwrap();
            let target = dirs.target_dir.join("config.toml");
            create_package_with_dotfiles(
                &dirs.package_dir,
                "myapp",
                &[("myapp/config.toml", target.to_str().unwrap())],
            );

            let events =
                collect_events(dirs.service().apply_all(ApplyOptions::default()).await).await;

            assert!(
                warnings(&events).is_empty(),
                "the guard is lexical; if it started refusing this, update \
                 `is_within`'s documentation too: {:?}",
                warnings(&events)
            );
            assert_eq!(
                std::fs::read_to_string(&target).unwrap(),
                "OUTSIDE THE PACKAGE DIRECTORY"
            );
        }

        // A symlinked directory inside the package directory.
        {
            let dirs = TestDirs::new();
            let outside = dirs.state_dir.join("outside_dir");
            std::fs::create_dir_all(&outside).unwrap();
            std::fs::write(
                outside.join("config.toml"),
                "OUTSIDE VIA A LINKED DIRECTORY",
            )
            .unwrap();
            std::os::unix::fs::symlink(&outside, dirs.package_dir.join("myapp")).unwrap();
            let target = dirs.target_dir.join("config.toml");
            create_package_with_dotfiles(
                &dirs.package_dir,
                "myapp",
                &[("myapp/config.toml", target.to_str().unwrap())],
            );

            assert!(
                !std::fs::symlink_metadata(dirs.package_dir.join("myapp/config.toml"))
                    .unwrap()
                    .file_type()
                    .is_symlink(),
                "the source path's final component is a regular file, which is why \
                 checking only the final component would not catch this"
            );

            let events =
                collect_events(dirs.service().apply_all(ApplyOptions::default()).await).await;

            assert!(warnings(&events).is_empty(), "{:?}", warnings(&events));
            assert_eq!(
                std::fs::read_to_string(&target).unwrap(),
                "OUTSIDE VIA A LINKED DIRECTORY"
            );
        }
    }
}
