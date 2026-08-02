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
    event::{OperationResult, PackageEvent},
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
            package_list.invalid_packages[0].path,
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
