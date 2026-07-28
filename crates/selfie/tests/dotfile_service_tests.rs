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
        let fs = RealFileSystem;
        let config = SelfieConfigBuilder::default()
            .environment("test")
            .package_directory(&self.package_dir)
            .dotfiles_directory(self.dotfiles_dir.clone())
            .state_directory(self.state_dir.clone())
            .build();
        let repo = YamlPackageRepository::new(fs, config.package_directory().clone());
        DotfileServiceImpl::new(repo, fs, runner, config)
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
        DotfileServiceImpl::new(package_repo, fs, FakeCommandRunner::new(), config)
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

    /// Assert that no event mentions `needle` anywhere in its debug rendering.
    ///
    /// Scans every event and every field rather than one variant's diff: a leak
    /// added to a warning, or to a newly introduced field, has to fail this too.
    fn assert_no_event_mentions(events: &[PackageEvent], needle: &str) {
        for event in events {
            let rendered = format!("{event:?}");
            assert!(
                !rendered.contains(needle),
                "secret leaked into an event: {rendered}"
            );
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

        let skipped = events.iter().any(|e| {
            matches!(e, PackageEvent::DotfileSkipped { reason, .. } if reason == "already in sync")
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
        assert!(!conflict.contains(SECRET));
        assert!(
            conflict.contains("op read x"),
            "the command is a reference, not a credential, and should be shown: {conflict}"
        );
    }

    #[tokio::test]
    async fn auto_accept_does_not_overwrite_a_secret_target() {
        // `auto_accept` is caller-settable — the MCP server exposes it to an
        // assistant — so honouring it would let a non-interactive caller silently
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

    // ─── Leak regression: the failure path ──────────────────────────────────

    #[tokio::test]
    async fn a_failing_provider_does_not_leak_its_stdout() {
        // The likeliest leak of all: `CommandFailure::ExecutionFailed` carries
        // `stdout`, `PackageEvent::Completed` carries that, and the CLI prints it
        // verbatim line by line. A provider's stdout IS the secret, so the resolve
        // path must never route a failure through `OperationFailure::from`.
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
}
