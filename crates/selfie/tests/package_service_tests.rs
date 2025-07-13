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

use tempfile::TempDir;
use test_common::{
    assert_failed_operation, assert_successful_operation, collect_events,
    create_service_install_test_package_file, create_service_invalid_package_file,
    create_service_test_package_file, create_service_test_service, get_operation_result,
};

use selfie::package::{
    event::{OperationResult, PackageEvent},
    service::PackageService,
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
    let stream = service.install("install-package").await;
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
        7,
        "Should have 7 progress events for install operation"
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
    let stream = service.list().await.unwrap();
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

/// Test the info service with a real package file
/// This verifies that package information is correctly extracted and environment status is checked
#[tokio::test]
async fn test_service_info_package() {
    // Arrange
    let temp_dir = TempDir::new().unwrap();
    create_test_package_file(&temp_dir, "info-package", true);
    let service = create_service_test_service(&temp_dir);

    // Act
    let stream = service.info("info-package").await.unwrap();
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
        assert_eq!(package_info.version, "1.0.0");
        assert_eq!(package_info.current_environment, "test");
        assert!(package_info.environments.contains(&"test".to_string()));
    } else {
        panic!("Expected PackageInfoLoaded event");
    }

    // Should have environment status events
    let env_status_events: Vec<_> = events
        .iter()
        .filter(|e| matches!(e, PackageEvent::EnvironmentStatusChecked { .. }))
        .collect();
    assert_eq!(
        env_status_events.len(),
        1,
        "Should have environment status event for test environment"
    );
}

/// Test the validate service with a well-formed package
/// This verifies that validation logic works correctly for valid packages
#[tokio::test]
async fn test_service_validate_package() {
    // Arrange
    let temp_dir = TempDir::new().unwrap();
    create_test_package_file(&temp_dir, "valid-package", true);
    let service = create_service_test_service(&temp_dir);

    // Act
    let stream = service.validate("valid-package", None).await.unwrap();
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

/// Test that all events have proper metadata and operation context
/// This verifies the event system works correctly across the service layer
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

/// Test error handling when operations fail
/// This verifies that failures are properly handled and communicated through events
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
