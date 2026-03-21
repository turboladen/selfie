//! Integration tests for the config service layer
//!
//! These tests verify config deployment operations using real filesystem
//! and repository implementations with temporary directories.

use std::path::PathBuf;

use futures::StreamExt;
use tempfile::TempDir;

use selfie::{
    config::SelfieConfigBuilder,
    config_service::{
        port::{ApplyOptions, ConfigService},
        service::ConfigServiceImpl,
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

/// Helper to create a package YAML file with a configs section
fn create_package_with_configs(
    package_dir: &std::path::Path,
    name: &str,
    configs: &[(&str, &str)],
) -> PathBuf {
    let mut configs_yaml = String::from("configs:\n");
    for (source, target) in configs {
        configs_yaml.push_str(&format!(
            "  - source: \"{source}\"\n    target: \"{target}\"\n"
        ));
    }

    let yaml = format!(
        r#"name: {name}
version: "1.0"
environments:
  test:
    install: "echo installed"
{configs_yaml}"#
    );

    let file_path = package_dir.join(format!("{name}.yml"));
    std::fs::write(&file_path, yaml).unwrap();
    file_path
}

/// Create standard test directories under a temp dir
struct TestDirs {
    _temp: TempDir,
    package_dir: PathBuf,
    configs_dir: PathBuf,
    target_dir: PathBuf,
    state_dir: PathBuf,
}

impl TestDirs {
    fn new() -> Self {
        let temp = TempDir::new().unwrap();
        let package_dir = temp.path().join("packages");
        let configs_dir = temp.path().join("configs");
        let target_dir = temp.path().join("target");
        let state_dir = temp.path().join("state");
        std::fs::create_dir_all(&package_dir).unwrap();
        std::fs::create_dir_all(&configs_dir).unwrap();
        std::fs::create_dir_all(&target_dir).unwrap();
        std::fs::create_dir_all(&state_dir).unwrap();
        Self {
            _temp: temp,
            package_dir,
            configs_dir,
            target_dir,
            state_dir,
        }
    }

    fn service(&self) -> ConfigServiceImpl<YamlPackageRepository<RealFileSystem>, RealFileSystem> {
        let fs = RealFileSystem;
        let config = SelfieConfigBuilder::default()
            .environment("test")
            .package_directory(&self.package_dir)
            .configs_directory(self.configs_dir.clone())
            .state_directory(self.state_dir.clone())
            .build();
        let repo = YamlPackageRepository::new(fs, config.package_directory().clone());
        ConfigServiceImpl::new(repo, fs, config)
    }
}

#[tokio::test]
async fn test_apply_all_deploys_new_config_file() {
    let dirs = TestDirs::new();

    // Create a config source file
    let source_dir = dirs.configs_dir.join("myapp");
    std::fs::create_dir_all(&source_dir).unwrap();
    std::fs::write(source_dir.join("config.toml"), "key = \"value\"").unwrap();

    let target_file = dirs.target_dir.join("config.toml");
    create_package_with_configs(
        &dirs.package_dir,
        "myapp",
        &[("myapp/config.toml", target_file.to_str().unwrap())],
    );

    let service = dirs.service();
    let stream = service.apply_all(ApplyOptions::default()).await;
    let events = collect_events(stream).await;

    let has_deploying = events
        .iter()
        .any(|e| matches!(e, PackageEvent::ConfigDeploying { .. }));
    let has_deployed = events
        .iter()
        .any(|e| matches!(e, PackageEvent::ConfigDeployed { .. }));
    assert!(has_deploying, "Should emit ConfigDeploying event");
    assert!(has_deployed, "Should emit ConfigDeployed event");

    assert!(target_file.exists(), "Target file should be created");
    let content = std::fs::read_to_string(&target_file).unwrap();
    assert_eq!(content, "key = \"value\"");

    let result = get_operation_result(&events).expect("Should have a Completed event");
    match result {
        OperationResult::Success(OperationSuccess::ConfigApplied {
            deployed_count,
            skipped_count,
            conflict_count,
            ..
        }) => {
            assert_eq!(*deployed_count, 1);
            assert_eq!(*skipped_count, 0);
            assert_eq!(*conflict_count, 0);
        }
        other => panic!("Expected ConfigApplied success, got: {other:?}"),
    }
}

#[tokio::test]
async fn test_apply_all_skips_when_up_to_date() {
    let dirs = TestDirs::new();

    let source_dir = dirs.configs_dir.join("myapp");
    std::fs::create_dir_all(&source_dir).unwrap();
    std::fs::write(source_dir.join("config.toml"), "key = \"value\"").unwrap();

    let target_file = dirs.target_dir.join("config.toml");
    create_package_with_configs(
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
        .any(|e| matches!(e, PackageEvent::ConfigSkipped { .. }));
    assert!(
        has_skipped,
        "Should emit ConfigSkipped event on second apply"
    );

    let result = get_operation_result(&events).expect("Should have a Completed event");
    match result {
        OperationResult::Success(OperationSuccess::ConfigApplied {
            deployed_count,
            skipped_count,
            ..
        }) => {
            assert_eq!(*deployed_count, 0);
            assert_eq!(*skipped_count, 1);
        }
        other => panic!("Expected ConfigApplied success, got: {other:?}"),
    }
}

#[tokio::test]
async fn test_apply_dry_run_does_not_write() {
    let dirs = TestDirs::new();

    let source_dir = dirs.configs_dir.join("myapp");
    std::fs::create_dir_all(&source_dir).unwrap();
    std::fs::write(source_dir.join("config.toml"), "key = \"value\"").unwrap();

    let target_file = dirs.target_dir.join("config.toml");
    create_package_with_configs(
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
        .any(|e| matches!(e, PackageEvent::ConfigSkipped { reason, .. } if reason == "dry run"));
    assert!(
        has_skipped_dry_run,
        "Should emit ConfigSkipped with 'dry run' reason"
    );

    let has_deploying = events
        .iter()
        .any(|e| matches!(e, PackageEvent::ConfigDeploying { .. }));
    assert!(!has_deploying, "Should NOT emit ConfigDeploying in dry run");

    assert!(
        !target_file.exists(),
        "Target file should NOT exist in dry run"
    );
}

#[tokio::test]
async fn test_apply_specific_package() {
    let dirs = TestDirs::new();

    let source_dir_a = dirs.configs_dir.join("app-a");
    let source_dir_b = dirs.configs_dir.join("app-b");
    std::fs::create_dir_all(&source_dir_a).unwrap();
    std::fs::create_dir_all(&source_dir_b).unwrap();
    std::fs::write(source_dir_a.join("a.conf"), "config-a").unwrap();
    std::fs::write(source_dir_b.join("b.conf"), "config-b").unwrap();

    let target_a = dirs.target_dir.join("a.conf");
    let target_b = dirs.target_dir.join("b.conf");

    create_package_with_configs(
        &dirs.package_dir,
        "app-a",
        &[("app-a/a.conf", target_a.to_str().unwrap())],
    );
    create_package_with_configs(
        &dirs.package_dir,
        "app-b",
        &[("app-b/b.conf", target_b.to_str().unwrap())],
    );

    let service = dirs.service();
    let stream = service.apply("app-a", ApplyOptions::default()).await;
    let events = collect_events(stream).await;

    let result = get_operation_result(&events).expect("Should have a Completed event");
    match result {
        OperationResult::Success(OperationSuccess::ConfigApplied { deployed_count, .. }) => {
            assert_eq!(*deployed_count, 1);
        }
        other => panic!("Expected ConfigApplied success, got: {other:?}"),
    }

    assert!(target_a.exists(), "app-a config should be deployed");
    assert!(!target_b.exists(), "app-b config should NOT be deployed");
}

#[tokio::test]
async fn test_apply_conflict_detected() {
    let dirs = TestDirs::new();

    let source_dir = dirs.configs_dir.join("myapp");
    std::fs::create_dir_all(&source_dir).unwrap();
    std::fs::write(source_dir.join("config.toml"), "key = \"new-value\"").unwrap();

    let target_file = dirs.target_dir.join("config.toml");
    create_package_with_configs(
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
        .any(|e| matches!(e, PackageEvent::ConfigConflict { .. }));
    assert!(has_conflict, "Should emit ConfigConflict event");

    let content = std::fs::read_to_string(&target_file).unwrap();
    assert_eq!(content, "key = \"user-modified\"");
}

#[tokio::test]
async fn test_apply_conflict_auto_accept() {
    let dirs = TestDirs::new();

    let source_dir = dirs.configs_dir.join("myapp");
    std::fs::create_dir_all(&source_dir).unwrap();
    std::fs::write(source_dir.join("config.toml"), "key = \"original\"").unwrap();

    let target_file = dirs.target_dir.join("config.toml");
    create_package_with_configs(
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
        .any(|e| matches!(e, PackageEvent::ConfigDeployed { .. }));
    assert!(has_deployed, "Should deploy with auto_accept");

    let content = std::fs::read_to_string(&target_file).unwrap();
    assert_eq!(content, "key = \"updated-source\"");
}

#[tokio::test]
async fn test_check_drift_detects_target_change() {
    let dirs = TestDirs::new();

    let source_dir = dirs.configs_dir.join("myapp");
    std::fs::create_dir_all(&source_dir).unwrap();
    std::fs::write(source_dir.join("config.toml"), "key = \"value\"").unwrap();

    let target_file = dirs.target_dir.join("config.toml");
    create_package_with_configs(
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
        .any(|e| matches!(e, PackageEvent::ConfigDriftDetected { .. }));
    assert!(has_drift, "Should detect drift after target modification");

    let result = get_operation_result(&events).expect("Should have a Completed event");
    match result {
        OperationResult::Success(OperationSuccess::ConfigDriftChecked {
            drift_count,
            total_count,
            ..
        }) => {
            assert_eq!(*drift_count, 1);
            assert_eq!(*total_count, 1);
        }
        other => panic!("Expected ConfigDriftChecked success, got: {other:?}"),
    }
}

#[tokio::test]
async fn test_check_drift_no_drift_when_up_to_date() {
    let dirs = TestDirs::new();

    let source_dir = dirs.configs_dir.join("myapp");
    std::fs::create_dir_all(&source_dir).unwrap();
    std::fs::write(source_dir.join("config.toml"), "key = \"value\"").unwrap();

    let target_file = dirs.target_dir.join("config.toml");
    create_package_with_configs(
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
        .any(|e| matches!(e, PackageEvent::ConfigDriftDetected { .. }));
    assert!(!has_drift, "Should not detect drift when up to date");

    let result = get_operation_result(&events).expect("Should have a Completed event");
    match result {
        OperationResult::Success(OperationSuccess::ConfigDriftChecked { drift_count, .. }) => {
            assert_eq!(*drift_count, 0);
        }
        other => panic!("Expected ConfigDriftChecked success, got: {other:?}"),
    }
}

#[tokio::test]
async fn test_check_drift_missing_source_emits_warning() {
    let dirs = TestDirs::new();

    let source_dir = dirs.configs_dir.join("myapp");
    std::fs::create_dir_all(&source_dir).unwrap();
    std::fs::write(source_dir.join("config.toml"), "key = \"value\"").unwrap();

    let target_file = dirs.target_dir.join("config.toml");
    create_package_with_configs(
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
            OperationResult::Success(OperationSuccess::ConfigDriftChecked { .. })
        ),
        "Should still complete with ConfigDriftChecked even with missing source"
    );
}

#[tokio::test]
async fn test_apply_all_no_configs_packages() {
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
        OperationResult::Success(OperationSuccess::ConfigApplied {
            deployed_count,
            skipped_count,
            conflict_count,
            ..
        }) => {
            assert_eq!(*deployed_count, 0);
            assert_eq!(*skipped_count, 0);
            assert_eq!(*conflict_count, 0);
        }
        other => panic!("Expected ConfigApplied success, got: {other:?}"),
    }
}
