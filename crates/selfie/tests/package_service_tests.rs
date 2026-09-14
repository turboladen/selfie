//! Integration tests for the package service layer business logic
//!
//! These tests focus on testing the business logic of the service layer
//! using real implementations but controlled test data. They verify that:
//!
//! 1. **Service Layer Business Logic**: Tests the core business logic without mocking
//! 2. **Event Generation**: Verifies proper event emission and metadata
//! 3. **Error Handling**: Tests various failure scenarios and error propagation
//! 4. **Progress Tracking**: Ensures operations emit proper progress events
//! 5. **Data Flow**: Validates that operations produce expected structured data
//!
//! These tests complement the unit tests by testing the full service layer
//! integration with real file system and command runner implementations.

use futures::StreamExt;
use tempfile::TempDir;
use test_common::{
    assert_failed_operation, assert_successful_operation, collect_events,
    create_circular_dependency, create_dependency_chain, create_service_install_test_package_file,
    create_service_install_test_package_file_with_note, create_service_invalid_package_file,
    create_service_test_package_file, create_service_test_package_file_with_deps,
    create_service_test_service, get_operation_result,
};

use selfie::package::{
    event::{DependencyFailure, OperationFailure, OperationResult, PackageEvent},
    service::{InstallOptions, PackageService, SpecService},
};

fn create_test_package_file(dir: &TempDir, name: &str, has_check: bool) -> std::path::PathBuf {
    create_service_test_package_file(dir, name, has_check)
}

fn create_invalid_package_file(dir: &TempDir, name: &str) -> std::path::PathBuf {
    create_service_invalid_package_file(dir, name)
}

// Event processing helpers are now provided by test_common crate

#[tokio::test]
async fn test_service_check_success() {
    // Arrange
    let temp_dir = TempDir::new().unwrap();
    create_test_package_file(&temp_dir, "test-package", true);
    let service = create_service_test_service(&temp_dir);

    // Act
    let stream = service.check("test-package").await;
    let events = collect_events(stream).await;

    // Assert
    assert_successful_operation(&events);

    // Verify we have the expected number of progress events for check operation
    let progress_events: Vec<_> = events
        .iter()
        .filter(|e| matches!(e, PackageEvent::Progress { .. }))
        .collect();
    assert_eq!(
        progress_events.len(),
        3,
        "Should have 3 progress events for check operation"
    );
}

#[tokio::test]
async fn test_service_check_package_not_found() {
    // Arrange
    let temp_dir = TempDir::new().unwrap();
    // Don't create any package files
    let service = create_service_test_service(&temp_dir);

    // Act
    let stream = service.check("non-existent-package").await;
    let events = collect_events(stream).await;

    // Assert
    assert_failed_operation(&events);
}

#[tokio::test]
async fn test_service_check_no_check_command() {
    // Arrange
    let temp_dir = TempDir::new().unwrap();
    create_test_package_file(&temp_dir, "no-check-package", false);
    let service = create_service_test_service(&temp_dir);

    // Act
    let stream = service.check("no-check-package").await;
    let events = collect_events(stream).await;

    // Assert
    // The service should fail when no check command is defined
    let result = get_operation_result(&events);
    assert!(result.is_some());
    assert!(matches!(result, Some(OperationResult::Failure(_))));

    // Should have exactly one completed event with failure
    let completed_events: Vec<_> = events
        .iter()
        .filter(|e| matches!(e, PackageEvent::Completed { .. }))
        .collect();
    assert_eq!(
        completed_events.len(),
        1,
        "Should have exactly one completed event"
    );
}

#[tokio::test]
async fn test_service_install_success() {
    // Arrange
    let temp_dir = TempDir::new().unwrap();
    let _ = create_service_install_test_package_file(&temp_dir, "install-package");
    let service = create_service_test_service(&temp_dir);

    // Act
    let stream = service
        .install("install-package", InstallOptions::default())
        .await;
    let events = collect_events(stream).await;

    // Assert
    assert_successful_operation(&events);

    // Verify we have progress events for install (should be 7 steps)
    let progress_events: Vec<_> = events
        .iter()
        .filter(|e| matches!(e, PackageEvent::Progress { .. }))
        .collect();
    assert_eq!(
        progress_events.len(),
        8,
        "Should have 8 progress events for install operation (1 dep resolve + 7 install steps)"
    );
}

#[tokio::test]
async fn test_service_list_packages() {
    // Arrange
    let temp_dir = TempDir::new().unwrap();
    create_test_package_file(&temp_dir, "package-one", true);
    create_test_package_file(&temp_dir, "package-two", false);
    create_invalid_package_file(&temp_dir, "invalid-package");
    let service = create_service_test_service(&temp_dir);

    // Act
    let stream = PackageService::list(&service, false).await;
    let events = collect_events(stream).await;

    // Assert
    assert_successful_operation(&events);

    // Should have package list data
    let list_events: Vec<_> = events
        .iter()
        .filter(|e| matches!(e, PackageEvent::PackageListLoaded { .. }))
        .collect();
    assert_eq!(
        list_events.len(),
        1,
        "Should have exactly one package list event"
    );

    if let PackageEvent::PackageListLoaded { package_list, .. } = &list_events[0] {
        // Should have 2 valid packages
        assert_eq!(package_list.valid_packages.len(), 2);

        // Should have 1 invalid package
        assert_eq!(package_list.invalid_packages.len(), 1);

        // Packages should be sorted alphabetically
        assert_eq!(package_list.valid_packages[0].name, "package-one");
        assert_eq!(package_list.valid_packages[1].name, "package-two");

        // Verify invalid package is listed
        assert_eq!(
            package_list.invalid_packages[0]
                .package_path()
                .display()
                .to_string(),
            format!("{}/invalid-package.yml", temp_dir.path().display())
        );
    } else {
        panic!("Expected PackageListLoaded event");
    }
}

// Test the spec_info service with a real package file
// This verifies that package definition info is correctly extracted
#[tokio::test]
async fn test_service_spec_info_package() {
    // Arrange
    let temp_dir = TempDir::new().unwrap();
    create_test_package_file(&temp_dir, "info-package", true);
    let service = create_service_test_service(&temp_dir);

    // Act
    let stream = service.spec_info("info-package").await;
    let events = collect_events(stream).await;

    // Assert
    assert_successful_operation(&events);

    // Should have package info data
    let info_events: Vec<_> = events
        .iter()
        .filter(|e| matches!(e, PackageEvent::PackageInfoLoaded { .. }))
        .collect();
    assert_eq!(
        info_events.len(),
        1,
        "Should have exactly one package info event"
    );

    if let PackageEvent::PackageInfoLoaded { package_info, .. } = &info_events[0] {
        assert_eq!(package_info.name, "info-package");
        assert_eq!(package_info.current_environment, "test");
        assert!(package_info.environments.contains(&"test".to_string()));
    } else {
        panic!("Expected PackageInfoLoaded event");
    }

    // spec_info does NOT check environment status — no EnvironmentStatusChecked events
    let env_status_events: Vec<_> = events
        .iter()
        .filter(|e| matches!(e, PackageEvent::EnvironmentStatusChecked { .. }))
        .collect();
    assert_eq!(
        env_status_events.len(),
        0,
        "spec_info should not emit environment status events"
    );
}

// Test the validate service with a well-formed package
// This verifies that validation logic works correctly for valid packages
#[tokio::test]
async fn test_service_validate_package() {
    // Arrange
    let temp_dir = TempDir::new().unwrap();
    create_test_package_file(&temp_dir, "valid-package", true);
    let service = create_service_test_service(&temp_dir);

    // Act
    let stream = service.validate("valid-package", None).await;
    let events = collect_events(stream).await;

    // Assert
    assert_successful_operation(&events);

    // Should have validation result data
    let validation_events: Vec<_> = events
        .iter()
        .filter(|e| matches!(e, PackageEvent::ValidationResultCompleted { .. }))
        .collect();
    assert_eq!(
        validation_events.len(),
        1,
        "Should have exactly one validation result event"
    );
}

// Test that all events have proper metadata and operation context
// This verifies the event system works correctly across the service layer
#[tokio::test]
async fn test_service_event_metadata() {
    // Arrange
    let temp_dir = TempDir::new().unwrap();
    create_test_package_file(&temp_dir, "metadata-test", true);
    let service = create_service_test_service(&temp_dir);

    // Act
    let stream = service.check("metadata-test").await;
    let events = collect_events(stream).await;

    // Assert - verify all events have proper metadata
    for event in &events {
        match event {
            PackageEvent::Started { operation_info, .. }
            | PackageEvent::Progress { operation_info, .. }
            | PackageEvent::Completed { operation_info, .. } => {
                assert_eq!(operation_info.package_name, "metadata-test");
                assert_eq!(operation_info.environment, "test");
            }
            PackageEvent::Debug { message, .. } => {
                // Debug events don't have operation_info in all cases, that's OK
                assert!(!message.is_empty());
            }
            PackageEvent::Trace { message, .. } => {
                // Trace events don't have operation_info in all cases, that's OK
                assert!(!message.is_empty());
            }
            _ => {
                // Other events may or may not have metadata, that's implementation dependent
            }
        }
    }
}

// Test error handling when operations fail
// This verifies that failures are properly handled and communicated through events
#[tokio::test]
async fn test_service_error_handling() {
    // Arrange
    let temp_dir = TempDir::new().unwrap();
    // Don't create any package files - this will cause repository errors
    let service = create_service_test_service(&temp_dir);

    // Act - try to check a non-existent package
    let stream = service.check("non-existent").await;
    let events = collect_events(stream).await;

    // Assert
    assert_failed_operation(&events);

    // Should still have started and completed events even for failures
    let started_events: Vec<_> = events
        .iter()
        .filter(|e| matches!(e, PackageEvent::Started { .. }))
        .collect();
    assert_eq!(
        started_events.len(),
        1,
        "Should have started event even for failures"
    );

    let completed_events: Vec<_> = events
        .iter()
        .filter(|e| matches!(e, PackageEvent::Completed { .. }))
        .collect();
    assert_eq!(
        completed_events.len(),
        1,
        "Should have completed event even for failures"
    );
}

// === Dependency chain integration tests ===

// Test installing a package with a single dependency.
// Both packages should be installed in the correct order.
#[tokio::test]
async fn test_service_install_single_dependency() {
    let temp_dir = TempDir::new().unwrap();
    // B has no deps, A depends on B
    let _ = create_service_test_package_file_with_deps(&temp_dir, "dep-b", &[]);
    let _ = create_service_test_package_file_with_deps(&temp_dir, "dep-a", &["dep-b"]);
    let service = create_service_test_service(&temp_dir);

    let stream = service.install("dep-a", InstallOptions::default()).await;
    let events = collect_events(stream).await;

    assert_successful_operation(&events);
}

// Test installing a package with a chain of dependencies (A->B->C).
// All three packages should be installed in dependency order.
#[tokio::test]
async fn test_service_install_chain_dependencies() {
    let temp_dir = TempDir::new().unwrap();
    create_dependency_chain(&temp_dir, &["chain-a", "chain-b", "chain-c"]);
    let service = create_service_test_service(&temp_dir);

    let stream = service.install("chain-a", InstallOptions::default()).await;
    let events = collect_events(stream).await;

    assert_successful_operation(&events);
}

// Test that installing a package with a missing dependency fails
// with a DependencyError::MissingDependency.
#[tokio::test]
async fn test_service_install_missing_dependency() {
    let temp_dir = TempDir::new().unwrap();
    // A depends on "nonexistent" which doesn't exist
    let _ =
        create_service_test_package_file_with_deps(&temp_dir, "missing-dep-a", &["nonexistent"]);
    let service = create_service_test_service(&temp_dir);

    let stream = service
        .install("missing-dep-a", InstallOptions::default())
        .await;
    let events = collect_events(stream).await;

    let result = get_operation_result(&events).expect("Should have an operation result");
    match result {
        OperationResult::Failure(failure) => {
            assert!(
                failure.is_dependency_error(),
                "Expected dependency error, got: {failure}"
            );
        }
        _ => panic!("Expected failure result"),
    }
}

// Test that circular dependencies are detected and produce a clear error.
#[tokio::test]
async fn test_service_install_circular_dependency() {
    let temp_dir = TempDir::new().unwrap();
    create_circular_dependency(&temp_dir, &["cycle-a", "cycle-b"]);
    let service = create_service_test_service(&temp_dir);

    let stream = service.install("cycle-a", InstallOptions::default()).await;
    let events = collect_events(stream).await;

    let result = get_operation_result(&events).expect("Should have an operation result");
    match result {
        OperationResult::Failure(failure) => {
            assert!(
                failure.is_dependency_error(),
                "Expected dependency error, got: {failure}"
            );
            match failure.dependency_failure().unwrap() {
                selfie::package::event::DependencyFailure::CircularDependency { cycle, .. } => {
                    assert!(cycle.len() >= 2, "Cycle should have at least 2 entries");
                }
                _ => panic!("Expected CircularDependency"),
            }
        }
        _ => panic!("Expected failure result"),
    }
}

// Test that already-installed dependencies are skipped gracefully.
#[tokio::test]
async fn test_service_install_already_installed_dependency() {
    let temp_dir = TempDir::new().unwrap();
    // Create B and A where A depends on B
    let _ = create_service_test_package_file_with_deps(&temp_dir, "installed-b", &[]);
    let _ = create_service_test_package_file_with_deps(&temp_dir, "installed-a", &["installed-b"]);
    let service = create_service_test_service(&temp_dir);

    // Install B first
    let stream = service
        .install("installed-b", InstallOptions::default())
        .await;
    let events = collect_events(stream).await;
    assert_successful_operation(&events);

    // Now install A — B should be detected as already installed
    let stream = service
        .install("installed-a", InstallOptions::default())
        .await;
    let events = collect_events(stream).await;
    assert_successful_operation(&events);
}

// Test that PackageListReady is emitted before any PackageListItemCompleted,
// and PackageListLoaded is emitted after all PackageListItemCompleted events.
#[tokio::test]
async fn test_package_list_ready_emitted_before_item_completed() {
    let temp_dir = TempDir::new().unwrap();

    // Create a couple of test packages
    let _ = create_service_test_package_file(&temp_dir, "alpha-pkg", true);
    let _ = create_service_test_package_file(&temp_dir, "beta-pkg", true);

    let service = create_service_test_service(&temp_dir);
    let mut stream = PackageService::list(&service, false).await;

    let mut saw_ready = false;
    let mut saw_item_before_ready = false;
    let mut saw_loaded = false;
    let mut saw_item_after_loaded = false;
    let mut ready_count = 0;

    while let Some(event) = stream.next().await {
        match &event {
            PackageEvent::PackageListReady { packages, .. } => {
                saw_ready = true;
                ready_count = packages.len();
                // All items should have status: None
                for pkg in packages {
                    assert!(
                        pkg.status.is_none(),
                        "PackageListReady items should have status: None"
                    );
                }
            }
            PackageEvent::PackageListItemCompleted { .. } => {
                if !saw_ready {
                    saw_item_before_ready = true;
                }
                if saw_loaded {
                    saw_item_after_loaded = true;
                }
            }
            PackageEvent::PackageListLoaded { .. } => {
                saw_loaded = true;
            }
            _ => {}
        }
    }

    assert!(saw_ready, "PackageListReady should be emitted");
    assert!(
        !saw_item_before_ready,
        "No PackageListItemCompleted should appear before PackageListReady"
    );
    assert!(saw_loaded, "PackageListLoaded should be emitted");
    assert!(
        !saw_item_after_loaded,
        "No PackageListItemCompleted should appear after PackageListLoaded"
    );
    assert_eq!(ready_count, 2, "PackageListReady should contain 2 packages");
}

// Test that PostInstallNote is emitted during a fresh install when the package has a note
#[tokio::test]
async fn test_service_install_emits_post_install_note() {
    // Arrange
    let temp_dir = TempDir::new().unwrap();
    let _ = create_service_install_test_package_file_with_note(
        &temp_dir,
        "noted-package",
        "Run 'source ~/.bashrc' to activate",
    );
    let service = create_service_test_service(&temp_dir);

    // Act
    let stream = service
        .install("noted-package", InstallOptions::default())
        .await;
    let events = collect_events(stream).await;

    // Assert
    assert_successful_operation(&events);

    // Verify PostInstallNote event was emitted
    let note_events: Vec<_> = events
        .iter()
        .filter(|e| matches!(e, PackageEvent::PostInstallNote { .. }))
        .collect();
    assert_eq!(
        note_events.len(),
        1,
        "Should emit exactly one PostInstallNote event"
    );

    if let PackageEvent::PostInstallNote {
        package_name, note, ..
    } = &note_events[0]
    {
        assert_eq!(package_name, "noted-package");
        assert_eq!(note, "Run 'source ~/.bashrc' to activate");
    } else {
        panic!("Expected PostInstallNote event");
    }
}

// Test that PostInstallNote is NOT emitted when package is already installed
#[tokio::test]
async fn test_service_install_no_post_install_note_when_already_installed() {
    // Arrange
    let temp_dir = TempDir::new().unwrap();
    let _ = create_service_install_test_package_file_with_note(
        &temp_dir,
        "already-noted",
        "This note should not appear on reinstall",
    );
    let service = create_service_test_service(&temp_dir);

    // First install — should emit the note
    let stream = service
        .install("already-noted", InstallOptions::default())
        .await;
    let events = collect_events(stream).await;
    assert_successful_operation(&events);
    let note_count = events
        .iter()
        .filter(|e| matches!(e, PackageEvent::PostInstallNote { .. }))
        .count();
    assert_eq!(note_count, 1, "First install should emit PostInstallNote");

    // Second install — package is already installed, should NOT emit the note
    let stream = service
        .install("already-noted", InstallOptions::default())
        .await;
    let events = collect_events(stream).await;
    assert_successful_operation(&events);
    let note_count = events
        .iter()
        .filter(|e| matches!(e, PackageEvent::PostInstallNote { .. }))
        .count();
    assert_eq!(
        note_count, 0,
        "Second install should NOT emit PostInstallNote since package is already installed"
    );
}

// `install`, `check` and `audit` load a spec and then look up the environment's
// command in it. A key shadowing `environments:` makes that lookup miss, so the
// command they run is not the one the file's author wrote -- the install-side
// version of the harm apply was taught to refuse.
//
// The fixture carries a real `environments:` as well as the shadowing key, so
// these tests exercise the unknown-key rule alone. A file with only
// `_environments:` would also be refused, for declaring no environment at all,
// and would pass these tests with the unknown-key rule deleted.
mod a_spec_selfie_cannot_read {
    use super::*;

    // The shadowing key appended to what `create_service_test_package_file`
    // writes for a working package, so the fixture differs from one every command
    // below accepts in exactly that key, however that helper changes. Differing
    // in one way is the whole point: a fixture that also lacks a check command is
    // refused for lacking one, and reports nothing about the rule under test.
    fn write_shadowed(dir: &TempDir, name: &str) {
        let file_path = create_test_package_file(dir, name, true);
        let mut content = std::fs::read_to_string(&file_path).unwrap();
        content.push_str("\n_environments:\n  test:\n    install: \"echo decoy\"\n");
        std::fs::write(&file_path, content).unwrap();
    }

    // The refusal names the key it refused over, which is what tells a reader
    // which of the file's rules they broke. Asserting on it is what separates
    // these tests from ones that pass on any failure at all -- a package that
    // fails to load, or an environment the config does not name, would satisfy a
    // bare "did it fail".
    // Matches the variant, not the sentence. A fixture that is refused for some
    // other reason -- a missing check command, a parse error -- renders a message
    // of its own, and a test satisfied by any failure mentioning the key would
    // accept it. The reason string is still checked, because which key selfie
    // objected to is data rather than wording.
    fn assert_refused_over_the_shadowing_key(events: &[PackageEvent]) {
        let reason = match get_operation_result(events).expect("the operation must complete") {
            OperationResult::Failure(OperationFailure::UnreadableSpec { reason, .. })
            | OperationResult::Failure(OperationFailure::DependencyError(
                DependencyFailure::UnreadableSpec { reason, .. },
            )) => reason.clone(),
            other => panic!("the spec must be refused as unreadable, got: {other:?}"),
        };
        assert!(
            reason.contains("_environments"),
            "the refusal must name the key it refused over, got: {reason}"
        );
    }

    // The failure names the package that pulled the unreadable one in, so a user
    // who asked for one package is not handed the name of another with no way to
    // tell why selfie looked at it.
    fn assert_required_by(events: &[PackageEvent], expected: &str) {
        match get_operation_result(events).expect("the operation must complete") {
            OperationResult::Failure(OperationFailure::DependencyError(
                DependencyFailure::UnreadableSpec { required_by, .. },
            )) => assert_eq!(
                required_by.as_deref(),
                Some(expected),
                "the failure must name the package that required it"
            ),
            other => panic!("expected a dependency refusal, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn install_refuses_it() {
        let temp_dir = TempDir::new().unwrap();
        write_shadowed(&temp_dir, "shadowed");
        let service = create_service_test_service(&temp_dir);

        let events =
            collect_events(service.install("shadowed", InstallOptions::default()).await).await;

        assert_failed_operation(&events);
        assert_refused_over_the_shadowing_key(&events);
    }

    // A dependency's spec is read for its own `environments:` mapping, and a
    // shadowed one leaves the graph short rather than empty -- the root installs,
    // and what it needs does not. The root here is clean, so only the dependency
    // can be what stops the run.
    #[tokio::test]
    async fn install_refuses_a_dependency_carrying_it() {
        let temp_dir = TempDir::new().unwrap();
        let _ = create_service_test_package_file_with_deps(&temp_dir, "root", &["shadowed"]);
        write_shadowed(&temp_dir, "shadowed");
        let service = create_service_test_service(&temp_dir);

        let events = collect_events(service.install("root", InstallOptions::default()).await).await;

        assert_failed_operation(&events);
        assert_refused_over_the_shadowing_key(&events);
        assert_required_by(&events, "root");
    }

    #[tokio::test]
    async fn check_refuses_it() {
        let temp_dir = TempDir::new().unwrap();
        write_shadowed(&temp_dir, "shadowed");
        let service = create_service_test_service(&temp_dir);

        let events = collect_events(service.check("shadowed").await).await;

        assert_failed_operation(&events);
        assert_refused_over_the_shadowing_key(&events);
    }

    #[tokio::test]
    async fn audit_refuses_it() {
        let temp_dir = TempDir::new().unwrap();
        write_shadowed(&temp_dir, "shadowed");
        let service = create_service_test_service(&temp_dir);

        let events = collect_events(service.audit("shadowed").await).await;

        assert_failed_operation(&events);
        assert_refused_over_the_shadowing_key(&events);
    }

    // The controls. A guard that refuses everything passes every test above and
    // is worse than no guard at all, so the same spec without the key has to
    // reach each of the three commands.
    #[tokio::test]
    async fn the_same_spec_without_the_key_still_installs() {
        let temp_dir = TempDir::new().unwrap();
        create_test_package_file(&temp_dir, "clean", true);
        let service = create_service_test_service(&temp_dir);

        let events =
            collect_events(service.install("clean", InstallOptions::default()).await).await;

        assert_successful_operation(&events);
    }

    #[tokio::test]
    async fn the_same_spec_without_the_key_still_checks() {
        let temp_dir = TempDir::new().unwrap();
        create_test_package_file(&temp_dir, "clean", true);
        let service = create_service_test_service(&temp_dir);

        let events = collect_events(service.check("clean").await).await;

        assert_successful_operation(&events);
    }

    #[tokio::test]
    async fn the_same_spec_without_the_key_still_audits() {
        let temp_dir = TempDir::new().unwrap();
        create_test_package_file(&temp_dir, "clean", true);
        let service = create_service_test_service(&temp_dir);

        let events = collect_events(service.audit("clean").await).await;

        assert_successful_operation(&events);
    }
}

// A root package that stops loading once its install has succeeded. Its
// recommends must still install: a failure at that point has no install left to
// fail, so it could only skip them under a successful result.
mod recommends_after_the_root_stops_loading {
    use std::{
        path::{Path, PathBuf},
        sync::Arc,
    };

    use selfie::{
        fs::{FileSystemError, RealFileSystem},
        package::{
            GetPackage, Package, SpecOrigin,
            git_adapter::GixGitStatusProvider,
            port::{
                ListPackagesOutput, PackageListError, PackageParseError, PackageRepoError,
                PackageRepository,
            },
            repository::YamlPackageRepository,
            service::PackageServiceImpl,
        },
    };
    use test_common::{FakeCommandRunner, config::service_test_config_with_dir};
    use tokio_util::sync::CancellationToken;

    use super::*;

    // The root fails to load from the moment its check command has run. Every
    // check in this fixture reports installed, so that is where the root's
    // install ends. Keyed to that step rather than to a read count, so the test
    // does not depend on how many times install reads the root first.
    #[derive(Debug, Clone)]
    struct RootStopsLoading {
        inner: YamlPackageRepository<RealFileSystem>,
        runner: FakeCommandRunner,
    }

    impl PackageRepository for RootStopsLoading {
        fn get_package(&self, name: &str) -> Result<GetPackage, PackageRepoError> {
            if name == "root"
                && self
                    .runner
                    .calls()
                    .iter()
                    .any(|(command, _)| command == "check-root")
            {
                return Err(PackageRepoError::IoError(Arc::new(std::io::Error::other(
                    "the root package stopped loading",
                ))));
            }
            self.inner.get_package(name)
        }

        fn path_is_occupied(&self, path: &Path) -> bool {
            self.inner.path_is_occupied(path)
        }

        fn read_referenced_file(
            &self,
            package_path: &Path,
            relative_path: &str,
        ) -> Result<String, FileSystemError> {
            self.inner.read_referenced_file(package_path, relative_path)
        }

        fn list_packages(&self) -> Result<ListPackagesOutput, PackageListError> {
            self.inner.list_packages()
        }

        fn find_package_files(&self, name: &str) -> Result<Vec<PathBuf>, PackageListError> {
            self.inner.find_package_files(name)
        }

        fn save_package(&self, package: &Package, path: &Path) -> Result<(), PackageRepoError> {
            self.inner.save_package(package, path)
        }

        fn remove_package(&self, name: &str) -> Result<(), PackageRepoError> {
            self.inner.remove_package(name)
        }

        fn find_dependent_packages(
            &self,
            target_package: &str,
        ) -> Result<(Vec<Package>, Vec<PackageParseError>), PackageRepoError> {
            self.inner.find_dependent_packages(target_package)
        }
    }

    #[tokio::test]
    async fn its_recommends_are_still_installed() {
        let temp_dir = TempDir::new().unwrap();
        let package_dir = temp_dir.path().to_path_buf();
        // The root has a hard dependency, and the dependency has a recommend of
        // its own. Only the root's recommend may install: recommends are one
        // level deep, and the dependency's list must neither replace the root's
        // nor join it.
        std::fs::write(
            package_dir.join("root.yml"),
            "name: root\nenvironments:\n  test:\n    check: \"check-root\"\n    install: \
             \"install-root\"\n    dependencies:\n      - dep\n    recommends:\n      - rec\n",
        )
        .unwrap();
        std::fs::write(
            package_dir.join("dep.yml"),
            "name: dep\nenvironments:\n  test:\n    check: \"check-dep\"\n    install: \
             \"install-dep\"\n    recommends:\n      - deprec\n",
        )
        .unwrap();
        for name in ["rec", "deprec"] {
            std::fs::write(
                package_dir.join(format!("{name}.yml")),
                format!(
                    "name: {name}\nenvironments:\n  test:\n    check: \"check-{name}\"\n    \
                     install: \"install-{name}\"\n"
                ),
            )
            .unwrap();
        }

        // Every check reports installed, so no install command runs.
        let runner = FakeCommandRunner::new()
            .succeeding("check-root", b"")
            .succeeding("check-dep", b"")
            .succeeding("check-rec", b"");
        let repo = RootStopsLoading {
            inner: YamlPackageRepository::new(
                RealFileSystem,
                package_dir.clone(),
                SpecOrigin::PackageDirectory,
            ),
            runner: runner.clone(),
        };
        let service = PackageServiceImpl::new(
            repo,
            runner,
            GixGitStatusProvider,
            service_test_config_with_dir(&package_dir),
            CancellationToken::new(),
        );

        let events = collect_events(service.install("root", InstallOptions::default()).await).await;

        assert_successful_operation(&events);
        assert!(
            events.iter().any(|e| matches!(
                e,
                PackageEvent::RecommendSucceeded { recommend_name, .. } if recommend_name == "rec"
            )),
            "the recommend was not installed; events: {events:?}"
        );
        assert!(
            !events.iter().any(|e| matches!(
                e,
                PackageEvent::RecommendStarted { recommend_name, .. } if recommend_name == "deprec"
            )),
            "a dependency's recommend was installed; events: {events:?}"
        );
    }
}
