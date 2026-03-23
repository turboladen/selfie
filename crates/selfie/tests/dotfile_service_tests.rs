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
version: "1.0"
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
    fn service(&self) -> DotfileServiceImpl<YamlPackageRepository<RealFileSystem>, RealFileSystem> {
        let fs = RealFileSystem;
        let config = SelfieConfigBuilder::default()
            .environment("test")
            .package_directory(&self.package_dir)
            .dotfiles_directory(self.dotfiles_dir.clone())
            .state_directory(self.state_dir.clone())
            .build();
        let repo = YamlPackageRepository::new(fs, config.package_directory().clone());
        DotfileServiceImpl::new(repo, fs, config)
    }

    /// Create a service backed by both `packages/` and `dotfiles/` directories.
    fn service_with_dotfiles(
        &self,
    ) -> DotfileServiceImpl<YamlPackageRepository<RealFileSystem>, RealFileSystem> {
        let fs = RealFileSystem;
        let config = SelfieConfigBuilder::default()
            .environment("test")
            .package_directory(&self.package_dir)
            .dotfiles_directory(self.dotfiles_dir.clone())
            .state_directory(self.state_dir.clone())
            .build();
        let package_repo = YamlPackageRepository::new(fs, config.package_directory().clone());
        let dotfiles_repo = YamlPackageRepository::new(fs, self.dotfiles_dir.clone());
        DotfileServiceImpl::new(package_repo, fs, config).with_dotfiles_repository(dotfiles_repo)
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
    use selfie::dotfile_service::port::{ConflictResolution, ConflictResolver};
    use std::sync::Arc;

    /// A test resolver that always accepts conflicts.
    struct AlwaysAccept;
    impl ConflictResolver for AlwaysAccept {
        fn resolve(&self, _source: &str, _target: &str, _diff: &str) -> ConflictResolution {
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
    use selfie::dotfile_service::port::{ConflictResolution, ConflictResolver};
    use std::sync::Arc;

    /// A test resolver that always skips conflicts.
    struct AlwaysSkip;
    impl ConflictResolver for AlwaysSkip {
        fn resolve(&self, _source: &str, _target: &str, _diff: &str) -> ConflictResolution {
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
version: "1.0"
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
        spec_content.contains("starship.toml"),
        "Spec should reference the source file"
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
version: "1.0"
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

    // Source file should be copied alongside the YAML
    let copied = dirs.package_dir.join("alacritty.toml");
    assert!(
        copied.exists(),
        "Source file should be copied alongside the package YAML"
    );
    assert_eq!(
        std::fs::read_to_string(&copied).unwrap(),
        "[font]\nsize = 12"
    );

    // Package YAML should now contain a dotfiles section
    let updated_yaml = std::fs::read_to_string(dirs.package_dir.join("alacritty.yml")).unwrap();
    assert!(
        updated_yaml.contains("dotfiles"),
        "Updated YAML should contain dotfiles section"
    );
    assert!(
        updated_yaml.contains("alacritty.toml"),
        "Updated YAML should reference the tracked file"
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
