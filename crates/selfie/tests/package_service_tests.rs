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
    assert_failed_answer, assert_failed_operation, assert_successful_operation, collect_events,
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

    // The one refused entry a listing reported, after asserting there is exactly
    // one: a listing that refused the clean spec as well would still hold this.
    fn only_refusal(refused: &[selfie::package::event::RefusedSpec]) -> &str {
        assert_eq!(
            refused.len(),
            1,
            "exactly the shadowed spec is refused: {refused:?}"
        );
        assert_eq!(refused[0].package_name, "shadowed");
        assert!(
            refused[0].reason.contains("_environments"),
            "the refusal must name the key it refused over, got: {}",
            refused[0].reason
        );
        &refused[0].reason
    }

    // A listing is not refused for one spec in it. The shadowed spec leaves the
    // list of packages and joins the refusals, with or without `--all`: without
    // it is where the spec would otherwise vanish, since the filter reads the
    // mapping the key shadows.
    #[tokio::test]
    async fn package_list_reports_it_as_refused() {
        for show_all in [false, true] {
            let temp_dir = TempDir::new().unwrap();
            write_shadowed(&temp_dir, "shadowed");
            create_test_package_file(&temp_dir, "clean", true);
            let service = create_service_test_service(&temp_dir);

            let events = collect_events(PackageService::list(&service, show_all).await).await;

            assert_successful_operation(&events);
            let data = events
                .iter()
                .find_map(|event| match event {
                    PackageEvent::PackageListLoaded { package_list, .. } => Some(package_list),
                    _ => None,
                })
                .expect("the listing must be sent");
            only_refusal(&data.refused);
            // The completion counts it too, since a consumer may read nothing else.
            let Some(OperationResult::Success(success)) = get_operation_result(&events) else {
                panic!("the listing must succeed");
            };
            let completion = success.to_string();
            assert!(completion.contains("1 refused package(s)"), "{completion}");
            let listed: Vec<&str> = data
                .valid_packages
                .iter()
                .map(|p| p.name.as_str())
                .collect();
            assert_eq!(listed, ["clean"], "show_all={show_all}");
            // No item is sent for it either, so its check command never ran.
            assert!(
                !events.iter().any(|event| matches!(
                    event,
                    PackageEvent::PackageListItemCompleted { package_item, .. }
                        if package_item.name == "shadowed"
                )),
                "show_all={show_all}"
            );
        }
    }

    #[tokio::test]
    async fn spec_list_reports_it_as_refused() {
        let temp_dir = TempDir::new().unwrap();
        write_shadowed(&temp_dir, "shadowed");
        create_test_package_file(&temp_dir, "clean", true);
        let service = create_service_test_service(&temp_dir);

        for show_all in [false, true] {
            let events = collect_events(SpecService::list(&service, show_all).await).await;
            assert_successful_operation(&events);
            let data = spec_list_data(&events);
            only_refusal(&data.refused);
            let Some(OperationResult::Success(success)) = get_operation_result(&events) else {
                panic!("the listing must succeed");
            };
            let completion = success.to_string();
            assert!(completion.contains("1 refused spec(s)"), "{completion}");
            assert!(data.specs.iter().all(|spec| spec.name != "shadowed"));
        }
    }

    // A search lists a refused spec as it lists a clean one: only when it
    // matches. Two refused specs, one matching, so a search ignoring its
    // pattern for refused specs lists both.
    #[tokio::test]
    async fn spec_search_reports_it_as_refused_only_when_it_matches() {
        let temp_dir = TempDir::new().unwrap();
        write_shadowed(&temp_dir, "shadowed");
        write_shadowed(&temp_dir, "unrelated");
        let service = create_service_test_service(&temp_dir);

        let events = collect_events(service.search("shad").await).await;
        assert_successful_operation(&events);
        only_refusal(&spec_list_data(&events).refused);

        let events = collect_events(service.search("no-such-pattern").await).await;
        assert_successful_operation(&events);
        let data = spec_list_data(&events);
        assert!(data.refused.is_empty(), "{:?}", data.refused);
        assert!(data.specs.is_empty());

        // The description comes from the file selfie refused, so a refused spec
        // is not matched on it. Both fixtures carry this description.
        let events = collect_events(service.search("service layer").await).await;
        let data = spec_list_data(&events);
        assert!(data.refused.is_empty(), "{:?}", data.refused);
    }

    fn spec_list_data(events: &[PackageEvent]) -> &selfie::package::event::SpecListData {
        events
            .iter()
            .find_map(|event| match event {
                PackageEvent::SpecListLoaded { spec_list, .. } => Some(spec_list),
                _ => None,
            })
            .expect("the listing must be sent")
    }

    // The rule follows the environments a listing shows. Without `--all` it is
    // apply's question here: a key only in 'work' leaves the spec readable, and a
    // spec declaring no environment is refused. With `--all`, or in a search,
    // every environment is shown, so the key in 'work' refuses the spec, and
    // declaring no environment is not a key selfie cannot trust.
    fn write_mode_fixtures(dir: &TempDir) {
        std::fs::write(
            dir.path().join("partial.yml"),
            "name: partial\nenvironments:\n  test:\n    install: \"true\"\n  work:\n    \
             install: \"true\"\n    audt: x\n",
        )
        .unwrap();
        std::fs::write(dir.path().join("noenv.yml"), "name: noenv\n").unwrap();
    }

    fn refused_names(refused: &[selfie::package::event::RefusedSpec]) -> Vec<&str> {
        let mut names: Vec<&str> = refused.iter().map(|r| r.package_name.as_str()).collect();
        names.sort_unstable();
        names
    }

    #[tokio::test]
    async fn listings_refuse_by_the_environments_they_show() {
        let temp_dir = TempDir::new().unwrap();
        write_mode_fixtures(&temp_dir);
        let service = create_service_test_service(&temp_dir);

        for (show_all, expected) in [(false, ["noenv"]), (true, ["partial"])] {
            let events = collect_events(PackageService::list(&service, show_all).await).await;
            let data = events
                .iter()
                .find_map(|event| match event {
                    PackageEvent::PackageListLoaded { package_list, .. } => Some(package_list),
                    _ => None,
                })
                .expect("the listing must be sent");
            assert_eq!(
                refused_names(&data.refused),
                expected,
                "package list, all={show_all}"
            );

            let events = collect_events(SpecService::list(&service, show_all).await).await;
            let data = spec_list_data(&events);
            assert_eq!(
                refused_names(&data.refused),
                expected,
                "spec list, all={show_all}"
            );
        }

        let events = collect_events(service.search("part").await).await;
        assert_eq!(refused_names(&spec_list_data(&events).refused), ["partial"]);
    }

    #[tokio::test]
    async fn status_refuses_it() {
        let temp_dir = TempDir::new().unwrap();
        write_shadowed(&temp_dir, "shadowed");
        let service = create_service_test_service(&temp_dir);

        let events = collect_events(service.status("shadowed").await).await;

        assert_failed_operation(&events);
        assert_refused_over_the_shadowing_key(&events);
    }

    // The root is clean, so the dependency's status is the only place the
    // refusal can show. A status reporting the dependency from its decoy mapping
    // would read "not in current environment" or run the decoy's check.
    #[tokio::test]
    async fn status_reports_a_dependency_carrying_it_as_unknown() {
        let temp_dir = TempDir::new().unwrap();
        let _ = create_service_test_package_file_with_deps(&temp_dir, "root", &["shadowed"]);
        write_shadowed(&temp_dir, "shadowed");
        let service = create_service_test_service(&temp_dir);

        let events = collect_events(service.status("root").await).await;

        assert_successful_operation(&events);
        let statuses = events
            .iter()
            .find_map(|event| match event {
                PackageEvent::EnvironmentStatusChecked {
                    environment_status, ..
                } => Some(&environment_status.dependency_statuses),
                _ => None,
            })
            .expect("the root's status must be sent");
        assert_eq!(statuses.len(), 1);
        match &statuses[0].status {
            selfie::package::event::EnvironmentStatus::Unknown(reason) => assert!(
                reason.starts_with("is refused: ") && reason.contains("_environments"),
                "the status must say it is refused and name the key, got: {reason}"
            ),
            other => panic!("the dependency must be unknown, got: {other:?}"),
        }
    }

    // Spec info describes the file rather than refusing to, and says why the
    // environments are missing. Exits 0: `spec validate` is the command whose
    // job is to fail over a file.
    #[tokio::test]
    async fn spec_info_describes_it_with_the_reason() {
        let temp_dir = TempDir::new().unwrap();
        write_shadowed(&temp_dir, "shadowed");
        let service = create_service_test_service(&temp_dir);

        let events = collect_events(service.spec_info("shadowed").await).await;

        assert_successful_operation(&events);
        let info = events
            .iter()
            .find_map(|event| match event {
                PackageEvent::PackageInfoLoaded { package_info, .. } => Some(package_info),
                _ => None,
            })
            .expect("the info must be sent");
        let reason = info
            .refusal
            .as_deref()
            .expect("the refusal must be carried");
        assert!(reason.contains("_environments"), "got: {reason}");
        assert!(info.environments.is_empty(), "{:?}", info.environments);
        assert!(info.dotfiles.is_empty());
        assert_eq!(info.apply_commands, 0);
    }

    // The same rule as every other command: a key in an environment this run
    // does not use leaves the spec readable here. Spec info shows it in full and
    // says, separately, that apply would refuse it in that environment.
    #[tokio::test]
    async fn spec_info_shows_a_spec_refused_only_elsewhere_and_names_why() {
        let temp_dir = TempDir::new().unwrap();
        std::fs::write(
            temp_dir.path().join("partial.yml"),
            "name: partial\nenvironments:\n  test:\n    install: \"true\"\n    dotfiles:\n      \
             - source: t.conf\n        target: ~/.t\n  work:\n    install: \"true\"\n    \
             audt: x\n    dotfiles:\n      - source: w.conf\n        target: ~/.w\n",
        )
        .unwrap();
        let service = create_service_test_service(&temp_dir);

        let events = collect_events(service.spec_info("partial").await).await;

        assert_successful_operation(&events);
        let info = events
            .iter()
            .find_map(|event| match event {
                PackageEvent::PackageInfoLoaded { package_info, .. } => Some(package_info),
                _ => None,
            })
            .expect("the info must be sent");
        assert!(info.refusal.is_none(), "{:?}", info.refusal);
        let mut environments = info.environments.clone();
        environments.sort();
        assert_eq!(environments, ["test", "work"]);
        let elsewhere = info
            .refusal_elsewhere
            .as_deref()
            .expect("the other environment's refusal must be carried");
        assert!(
            elsewhere.contains("work") && elsewhere.contains("audt"),
            "{elsewhere}"
        );
        // Each entry says whether apply would deploy it where it is declared.
        let refused: Vec<(Option<&str>, bool)> = info
            .dotfiles
            .iter()
            .map(|d| (d.environment.as_deref(), d.refused))
            .collect();
        assert_eq!(refused, [(Some("test"), false), (Some("work"), true)]);
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
    async fn the_same_spec_without_the_key_still_reports_status() {
        let temp_dir = TempDir::new().unwrap();
        create_test_package_file(&temp_dir, "clean", true);
        let service = create_service_test_service(&temp_dir);

        let events = collect_events(service.status("clean").await).await;

        assert_successful_operation(&events);
    }

    #[tokio::test]
    async fn the_same_spec_without_the_key_still_has_its_environments_in_spec_info() {
        let temp_dir = TempDir::new().unwrap();
        create_test_package_file(&temp_dir, "clean", true);
        let service = create_service_test_service(&temp_dir);

        let events = collect_events(service.spec_info("clean").await).await;

        let info = events
            .iter()
            .find_map(|event| match event {
                PackageEvent::PackageInfoLoaded { package_info, .. } => Some(package_info),
                _ => None,
            })
            .expect("the info must be sent");
        assert!(info.refusal.is_none());
        assert!(!info.environments.is_empty());
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
        fn resolved_directory(&self) -> Option<PathBuf> {
            self.inner.resolved_directory()
        }

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
        let config = service_test_config_with_dir(&package_dir);
        let dotfiles_repo = RootStopsLoading {
            inner: YamlPackageRepository::new(
                RealFileSystem,
                config.dotfiles_directory(),
                SpecOrigin::DotfilesDirectory,
            ),
            runner: runner.clone(),
        };
        let service = PackageServiceImpl::new(
            repo,
            dotfiles_repo,
            runner,
            GixGitStatusProvider,
            config,
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

// A Ctrl+C that lands while a recommend installs ends the stream as a
// cancellation. The handler itself returns the root's success, since recommends
// never fail the parent, so this holds only because the operation wrapper asks
// the token after the handler.
#[tokio::test]
async fn a_cancel_during_the_recommends_ends_the_stream_cancelled() {
    use selfie::{
        fs::RealFileSystem,
        package::{
            SpecOrigin, git_adapter::GixGitStatusProvider, repository::YamlPackageRepository,
            service::PackageServiceImpl,
        },
    };
    use test_common::{FakeCommandRunner, config::service_test_config_with_dir};
    use tokio_util::sync::CancellationToken;

    let temp_dir = TempDir::new().unwrap();
    let package_dir = temp_dir.path().to_path_buf();
    std::fs::write(
        package_dir.join("root.yml"),
        "name: root\nenvironments:\n  test:\n    check: \"check-root\"\n    install: \
         \"install-root\"\n    recommends:\n      - rec\n",
    )
    .unwrap();
    std::fs::write(
        package_dir.join("rec.yml"),
        "name: rec\nenvironments:\n  test:\n    check: \"check-rec\"\n    install: \
         \"install-rec\"\n",
    )
    .unwrap();

    let token = CancellationToken::new();
    // The root is installed, so the recommend is the only work left, and the
    // cancel lands inside it.
    let runner = FakeCommandRunner::new()
        .succeeding("check-root", b"")
        .succeeding("check-rec", b"")
        .cancelling("check-rec", &token);
    let config = service_test_config_with_dir(&package_dir);
    let service = PackageServiceImpl::new(
        YamlPackageRepository::new(
            RealFileSystem,
            package_dir.clone(),
            SpecOrigin::PackageDirectory,
        ),
        YamlPackageRepository::new(
            RealFileSystem,
            config.dotfiles_directory(),
            SpecOrigin::DotfilesDirectory,
        ),
        runner.clone(),
        GixGitStatusProvider,
        config,
        token,
    );

    let events = collect_events(service.install("root", InstallOptions::default()).await).await;

    assert!(
        runner
            .calls()
            .iter()
            .any(|(command, _)| command == "check-rec"),
        "the recommend never ran, so the cancel never landed: {events:?}"
    );
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
}

// `spec info` reports where each dotfile's content comes from, and how many
// commands apply would run here to produce it. The fixture varies the count
// along each axis a wrong count could take: a template with two vars (counted
// once, it reads 1), a shared command overridden here by a plain file (counted
// from the shared list, it adds 1), and a command in another environment
// (counted across environments, it adds 1).
#[tokio::test]
async fn spec_info_reports_dotfile_sources_and_the_commands_apply_runs() {
    let temp_dir = TempDir::new().unwrap();
    std::fs::write(
        temp_dir.path().join("sourced.yml"),
        r#"name: sourced
dotfiles:
  - source: a.tpl
    target: ~/.a
    vars:
      x: echo x
      y: echo y
  - command: echo shared
    target: ~/.b
environments:
  test:
    install: "true"
    dotfiles:
      - source: b.conf
        target: ~/.b
  other:
    install: "true"
    dotfiles:
      - command: echo other
        target: ~/.c
"#,
    )
    .unwrap();
    let service = create_service_test_service(&temp_dir);

    let events = collect_events(service.spec_info("sourced").await).await;

    assert_successful_operation(&events);
    let info = events
        .iter()
        .find_map(|event| match event {
            PackageEvent::PackageInfoLoaded { package_info, .. } => Some(package_info),
            _ => None,
        })
        .expect("the info must be sent");
    assert_eq!(
        info.apply_commands, 2,
        "two vars, and nothing else runs here"
    );
    let rows: Vec<(Option<&str>, &str)> = info
        .dotfiles
        .iter()
        .map(|d| (d.environment.as_deref(), d.entry.target()))
        .collect();
    assert_eq!(
        rows,
        [
            (None, "~/.a"),
            (None, "~/.b"),
            (Some("other"), "~/.c"),
            (Some("test"), "~/.b"),
        ],
        "every entry in every scope is reported"
    );
}

// A package spec declaring no environment is refused by apply, so apply runs
// none of its commands however many its entries name, and spec info says why
// in place of the entries.
#[tokio::test]
async fn spec_info_counts_no_commands_for_a_spec_apply_refuses() {
    let temp_dir = TempDir::new().unwrap();
    std::fs::write(
        temp_dir.path().join("unscoped.yml"),
        "name: unscoped\ndotfiles:\n  - command: echo shared\n    target: ~/.b\n",
    )
    .unwrap();
    let service = create_service_test_service(&temp_dir);

    let events = collect_events(service.spec_info("unscoped").await).await;

    let info = events
        .iter()
        .find_map(|event| match event {
            PackageEvent::PackageInfoLoaded { package_info, .. } => Some(package_info),
            _ => None,
        })
        .expect("the info must be sent");
    assert!(
        info.refusal
            .as_deref()
            .is_some_and(|reason| reason.contains("environment")),
        "{:?}",
        info.refusal
    );
    assert!(info.dotfiles.is_empty());
    assert_eq!(info.apply_commands, 0);
}

// `spec validate --all` validates what apply collects: the package specs and the
// standalone dotfile specs beside them, with apply's rules for which files it
// would use.
mod validate_all_covers_standalone_specs {
    use std::path::Path;

    use selfie::{config::SelfieConfigBuilder, package::event::Outcome};
    use test_common::create_test_service_with_config;

    use super::*;

    struct Dirs {
        _temp: TempDir,
        packages: std::path::PathBuf,
        dotfiles: std::path::PathBuf,
    }

    fn dirs() -> Dirs {
        let temp = TempDir::new().unwrap();
        let packages = temp.path().join("packages");
        let dotfiles = temp.path().join("dotfiles");
        std::fs::create_dir_all(&packages).unwrap();
        std::fs::create_dir_all(&dotfiles).unwrap();
        Dirs {
            _temp: temp,
            packages,
            dotfiles,
        }
    }

    impl Dirs {
        fn service(&self) -> impl SpecService {
            create_test_service_with_config(
                SelfieConfigBuilder::default()
                    .environment("test")
                    .package_directory(&self.packages)
                    .dotfiles_directory(self.dotfiles.clone())
                    .build(),
            )
        }
    }

    // A spec as `dotfiles track` writes one: no environments, one entry, and its
    // source beside it.
    fn write_standalone(dir: &Path, name: &str, extra_entry_key: &str) {
        std::fs::create_dir_all(dir.join(name)).unwrap();
        std::fs::write(dir.join(name).join("rc"), "x").unwrap();
        std::fs::write(
            dir.join(format!("{name}.yml")),
            format!(
                "name: {name}\ndotfiles:\n  - source: {name}/rc\n    target: ~/.{name}rc\n\
                 {extra_entry_key}"
            ),
        )
        .unwrap();
    }

    // A package spec whose name matches its file, so it carries no warning of
    // its own.
    fn write_package(dir: &Path, file: &str, extra: &str) {
        let name = Path::new(file).file_stem().unwrap().to_str().unwrap();
        std::fs::write(
            dir.join(file),
            format!("name: {name}\nenvironments:\n  test:\n    install: \"true\"\n{extra}"),
        )
        .unwrap();
    }

    fn results(events: &[PackageEvent]) -> Vec<(String, Outcome)> {
        events
            .iter()
            .filter_map(|event| match event {
                PackageEvent::ValidationResultCompleted {
                    validation_result, ..
                } => Some((
                    validation_result.package_name.clone(),
                    validation_result.outcome,
                )),
                _ => None,
            })
            .collect()
    }

    #[tokio::test]
    async fn a_clean_standalone_spec_is_valid() {
        let dirs = dirs();
        write_standalone(&dirs.dotfiles, "gemrc", "");

        let events = collect_events(dirs.service().validate_all().await).await;

        assert_successful_operation(&events);
        assert_eq!(results(&events), [("gemrc".to_string(), Outcome::Clean)]);
    }

    #[tokio::test]
    async fn a_broken_standalone_spec_fails_the_run() {
        let dirs = dirs();
        write_standalone(&dirs.dotfiles, "gemrc", "    extra: x\n");

        let events = collect_events(dirs.service().validate_all().await).await;

        assert_failed_answer(&events);
        assert_eq!(results(&events), [("gemrc".to_string(), Outcome::Failed)]);
    }

    // The package spec wins the name, as it does for apply, so the dotfiles copy
    // is warned about and its own defect is not reported.
    #[tokio::test]
    async fn a_standalone_spec_a_package_spec_shadows_is_warned_about_and_not_validated() {
        let dirs = dirs();
        write_package(&dirs.packages, "gemrc.yml", "");
        write_standalone(&dirs.dotfiles, "gemrc", "    extra: x\n");

        let events = collect_events(dirs.service().validate_all().await).await;

        assert_successful_operation(&events);
        assert_eq!(results(&events), [("gemrc".to_string(), Outcome::Clean)]);
        let warnings: Vec<_> = events
            .iter()
            .filter_map(|event| match event {
                PackageEvent::Warning { message, .. } => Some(message.as_str()),
                _ => None,
            })
            .collect();
        assert!(
            warnings
                .iter()
                .any(|m| m.contains("'gemrc'") && m.contains("packages/")),
            "{warnings:?}"
        );
    }

    // Two files claiming one name are refused, as install refuses them, and
    // neither is validated: which one a report describes would be a guess. The
    // failure names the cause. Both files are install-only, so apply would let
    // the ambiguity through, and a count that ignored it would pass the run.
    #[tokio::test]
    async fn an_ambiguous_name_fails_the_run_without_validating_either_file() {
        use selfie::package::event::RefusalKind;

        let dirs = dirs();
        write_package(&dirs.packages, "dup.yml", "");
        write_package(&dirs.packages, "dup.yaml", "    audt: x\n");

        let events = collect_events(dirs.service().validate_all().await).await;

        assert_failed_answer(&events);
        assert!(results(&events).is_empty(), "{:?}", results(&events));
        assert!(
            events.iter().any(|event| matches!(
                event,
                PackageEvent::PackagesRefused { kind: RefusalKind::AmbiguousName, reason, packages, .. }
                    if reason.contains("dup.yaml")
                        && reason.contains("dup.yml")
                        && packages.len() == 1
                        && packages[0].name == "dup"
                        && packages[0].paths.len() == 2
            )),
            "the failure must name the ambiguous files: {events:#?}"
        );
    }

    // A spec whose mapping cannot be trusted is validated whatever environments
    // it appears to declare, since those are what the key may be hiding.
    #[tokio::test]
    async fn a_refused_spec_for_another_environment_is_still_validated() {
        let dirs = dirs();
        std::fs::write(
            dirs.packages.join("elsewhere.yml"),
            "name: elsewhere\nenvironments:\n  other:\n    install: \"true\"\n\
             _environments:\n  test:\n    install: \"echo decoy\"\n",
        )
        .unwrap();

        let events = collect_events(dirs.service().validate_all().await).await;

        assert_failed_answer(&events);
        assert_eq!(
            results(&events),
            [("elsewhere".to_string(), Outcome::Failed)]
        );
    }

    // A dotfiles directory selfie cannot list may hold specs it never saw, so
    // the run cannot say every spec is valid.
    #[tokio::test]
    async fn an_unlistable_dotfiles_directory_fails_the_run() {
        use std::os::unix::fs::PermissionsExt;

        let dirs = dirs();
        write_package(&dirs.packages, "clean.yml", "");
        std::fs::set_permissions(&dirs.dotfiles, std::fs::Permissions::from_mode(0o000)).unwrap();
        // Root lists it anyway, and then there is nothing to test.
        let listable = std::fs::read_dir(&dirs.dotfiles).is_ok();

        let events = collect_events(dirs.service().validate_all().await).await;
        std::fs::set_permissions(&dirs.dotfiles, std::fs::Permissions::from_mode(0o755)).unwrap();
        if listable {
            return;
        }

        assert_failed_answer(&events);
        assert!(
            events.iter().any(|event| matches!(
                event,
                PackageEvent::Warning { message, .. }
                    if message.contains("standalone dotfiles")
            )),
            "the warning must say which directory could not be read"
        );
    }

    // Apply ignores an unparsable dotfiles spec whose name a package spec
    // claims, so the run reports it without failing over it.
    #[tokio::test]
    async fn an_unparsable_standalone_spec_a_package_spec_shadows_does_not_fail_the_run() {
        let dirs = dirs();
        write_package(&dirs.packages, "gemrc.yml", "");
        std::fs::write(dirs.dotfiles.join("gemrc.yml"), "name: [unclosed\n").unwrap();

        let events = collect_events(dirs.service().validate_all().await).await;

        assert_successful_operation(&events);
        assert!(
            events
                .iter()
                .any(|event| matches!(event, PackageEvent::SpecSkipped { .. })),
            "the file is still reported"
        );
    }

    // A key in an environment this run does not use is no reason apply refuses
    // the package here, so the run does not validate it, as it does not
    // validate any package declaring only another environment.
    #[tokio::test]
    async fn a_spec_refused_only_in_another_environment_is_not_validated() {
        let dirs = dirs();
        std::fs::write(
            dirs.packages.join("elsewhere.yml"),
            "name: elsewhere\nenvironments:\n  other:\n    install: \"true\"\n    audt: x\n",
        )
        .unwrap();

        let events = collect_events(dirs.service().validate_all().await).await;

        assert_successful_operation(&events);
        assert!(results(&events).is_empty(), "{:?}", results(&events));
    }

    // A mistyped name is answered about the package directory, whatever is wrong
    // with the dotfiles directory.
    #[tokio::test]
    async fn spec_validate_of_a_missing_name_names_the_package_directory() {
        let dirs = dirs();
        std::fs::remove_dir(&dirs.dotfiles).unwrap();
        std::os::unix::fs::symlink(dirs.packages.join("gone"), &dirs.dotfiles).unwrap();

        let events = collect_events(dirs.service().validate("typo", None).await).await;

        assert_failed_operation(&events);
        let Some(OperationResult::Failure(failure)) = get_operation_result(&events) else {
            panic!("the run must fail");
        };
        let message = failure.to_string();
        assert!(
            message.contains("not found") && message.contains(&*dirs.packages.to_string_lossy()),
            "{message}"
        );
    }

    // A dotfiles directory selfie cannot list says nothing about a mistyped
    // name, so the answer is still the package directory's.
    #[tokio::test]
    async fn spec_validate_of_a_missing_name_with_an_unreadable_dotfiles_directory_names_the_package_directory()
     {
        use std::os::unix::fs::PermissionsExt;

        let dirs = dirs();
        std::fs::set_permissions(&dirs.dotfiles, std::fs::Permissions::from_mode(0o000)).unwrap();
        // Root reads it anyway, and then there is nothing to test.
        let listable = std::fs::read_dir(&dirs.dotfiles).is_ok();

        let events = collect_events(dirs.service().validate("typo", None).await).await;
        std::fs::set_permissions(&dirs.dotfiles, std::fs::Permissions::from_mode(0o755)).unwrap();
        if listable {
            return;
        }

        assert_failed_operation(&events);
        let Some(OperationResult::Failure(failure)) = get_operation_result(&events) else {
            panic!("the run must fail");
        };
        let message = failure.to_string();
        assert!(
            message.contains("not found") && message.contains(&*dirs.packages.to_string_lossy()),
            "{message}"
        );
    }

    #[tokio::test]
    async fn spec_validate_finds_a_standalone_spec_by_name() {
        let dirs = dirs();
        write_standalone(&dirs.dotfiles, "gemrc", "");

        let events = collect_events(dirs.service().validate("gemrc", None).await).await;

        assert_successful_operation(&events);
        assert_eq!(results(&events), [("gemrc".to_string(), Outcome::Clean)]);
    }
}

// A failed install command's warning names the command that failed, since the
// warning carries nothing else to locate it by.
#[tokio::test]
async fn a_failed_command_warning_names_the_command() {
    let temp_dir = TempDir::new().unwrap();
    std::fs::write(
        temp_dir.path().join("broken.yaml"),
        "name: broken\nenvironments:\n  test:\n    install: \"exit 3\"\n    check: \"false\"\n",
    )
    .unwrap();
    let service = create_service_test_service(&temp_dir);

    let events = collect_events(service.install("broken", InstallOptions::default()).await).await;

    let warnings: Vec<&String> = events
        .iter()
        .filter_map(|e| match e {
            PackageEvent::Warning { message, .. } if message.contains("failed with exit code") => {
                Some(message)
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        warnings,
        vec!["The `install` command failed with exit code 3"],
        "{events:#?}"
    );
}

// `audit --all` reports the packages it leaves out once per reason, naming
// each, and counts every one as a refusal.
#[tokio::test]
async fn an_audit_of_every_package_groups_the_packages_it_refuses() {
    let temp_dir = TempDir::new().unwrap();
    for (name, key) in [("a", "version"), ("b", "audt"), ("c", "version")] {
        std::fs::write(
            temp_dir.path().join(format!("{name}.yaml")),
            format!("name: {name}\n{key}: 1\nenvironments:\n  test:\n    install: \"true\"\n"),
        )
        .unwrap();
    }
    let service = create_service_test_service(&temp_dir);

    let events = collect_events(service.audit_all().await).await;

    let refused: Vec<(String, Vec<String>)> = events
        .iter()
        .filter_map(|e| match e {
            PackageEvent::PackagesRefused {
                reason, packages, ..
            } => Some((
                reason.clone(),
                packages.iter().map(|p| p.name.clone()).collect(),
            )),
            _ => None,
        })
        .collect();
    assert_eq!(refused.len(), 2, "{refused:#?}");
    assert!(refused[0].0.contains("version"), "{refused:#?}");
    assert_eq!(refused[0].1, ["a", "c"]);
    assert_eq!(refused[1].1, ["b"]);
    match get_operation_result(&events) {
        Some(OperationResult::Success(
            selfie::package::event::OperationSuccess::PackagesAudited { refused_count, .. },
        )) => assert_eq!(*refused_count, 3, "{events:#?}"),
        other => panic!("expected an audit result, got {other:?}"),
    }
}

// A cancel during the recommends names every recommend it left untried: the
// ones in a chunk it never reached, and the ones in its own chunk that had not
// started. Four recommends two at a time; the cancel lands in `r1`, so `r2`
// returns from inside the first chunk without starting, and `r3` and `r4` are
// in a chunk never reached. `r1` started, so it is not named.
#[tokio::test]
async fn a_cancel_during_the_recommends_names_the_ones_left_untried() {
    use selfie::{
        config::SelfieConfigBuilder,
        fs::RealFileSystem,
        package::{
            SpecOrigin, git_adapter::GixGitStatusProvider, repository::YamlPackageRepository,
            service::PackageServiceImpl,
        },
    };
    use test_common::FakeCommandRunner;
    use tokio_util::sync::CancellationToken;

    let temp_dir = TempDir::new().unwrap();
    let package_dir = temp_dir.path().to_path_buf();
    std::fs::write(
        package_dir.join("root.yml"),
        "name: root\nenvironments:\n  test:\n    check: \"check-root\"\n    install: \
         \"install-root\"\n    recommends:\n      - r1\n      - r2\n      - r3\n      - r4\n",
    )
    .unwrap();
    for name in ["r1", "r2", "r3", "r4"] {
        std::fs::write(
            package_dir.join(format!("{name}.yml")),
            format!(
                "name: {name}\nenvironments:\n  test:\n    check: \"check-{name}\"\n    \
                 install: \"install-{name}\"\n"
            ),
        )
        .unwrap();
    }

    let token = CancellationToken::new();
    let runner = FakeCommandRunner::new()
        .succeeding("check-root", b"")
        .succeeding("check-r1", b"")
        .cancelling("check-r1", &token);
    let config = SelfieConfigBuilder::default()
        .environment("test")
        .package_directory(&package_dir)
        .max_concurrency_unchecked(2)
        .build();
    let service = PackageServiceImpl::new(
        YamlPackageRepository::new(
            RealFileSystem,
            package_dir.clone(),
            SpecOrigin::PackageDirectory,
        ),
        YamlPackageRepository::new(
            RealFileSystem,
            config.dotfiles_directory(),
            SpecOrigin::DotfilesDirectory,
        ),
        runner.clone(),
        GixGitStatusProvider,
        config,
        token,
    );

    let events = collect_events(service.install("root", InstallOptions::default()).await).await;

    assert!(
        events.iter().any(|e| matches!(
            e,
            PackageEvent::RecommendStarted { recommend_name, .. } if recommend_name == "r1"
        )),
        "control: the first recommend started, so the cancel landed inside the chunk: {events:#?}"
    );
    let untried: Vec<&Vec<String>> = events
        .iter()
        .filter_map(|e| match e {
            PackageEvent::RecommendsUntried { names, .. } => Some(names),
            _ => None,
        })
        .collect();
    assert_eq!(
        untried,
        [&vec!["r2".to_string(), "r3".to_string(), "r4".to_string()]],
        "{events:#?}"
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, PackageEvent::Canceled { .. })),
        "{events:#?}"
    );
}

// A recommend the cancel stops part way, here between its dependency and
// itself, is named with the untried ones and not reported as failed.
#[tokio::test]
async fn a_recommend_the_cancel_stops_part_way_is_named_untried() {
    use selfie::{
        fs::RealFileSystem,
        package::{
            SpecOrigin, git_adapter::GixGitStatusProvider, repository::YamlPackageRepository,
            service::PackageServiceImpl,
        },
    };
    use test_common::{FakeCommandRunner, config::service_test_config_with_dir};
    use tokio_util::sync::CancellationToken;

    let temp_dir = TempDir::new().unwrap();
    let package_dir = temp_dir.path().to_path_buf();
    std::fs::write(
        package_dir.join("root.yml"),
        "name: root\nenvironments:\n  test:\n    check: \"check-root\"\n    install: \
         \"install-root\"\n    recommends:\n      - rec\n",
    )
    .unwrap();
    std::fs::write(
        package_dir.join("rec.yml"),
        "name: rec\nenvironments:\n  test:\n    check: \"check-rec\"\n    install: \
         \"install-rec\"\n    dependencies:\n      - dep\n",
    )
    .unwrap();
    std::fs::write(
        package_dir.join("dep.yml"),
        "name: dep\nenvironments:\n  test:\n    check: \"check-dep\"\n    install: \
         \"install-dep\"\n",
    )
    .unwrap();

    let token = CancellationToken::new();
    let runner = FakeCommandRunner::new()
        .succeeding("check-root", b"")
        .succeeding("check-dep", b"")
        .cancelling("check-dep", &token);
    let config = service_test_config_with_dir(&package_dir);
    let service = PackageServiceImpl::new(
        YamlPackageRepository::new(
            RealFileSystem,
            package_dir.clone(),
            SpecOrigin::PackageDirectory,
        ),
        YamlPackageRepository::new(
            RealFileSystem,
            config.dotfiles_directory(),
            SpecOrigin::DotfilesDirectory,
        ),
        runner.clone(),
        GixGitStatusProvider,
        config,
        token,
    );

    let events = collect_events(service.install("root", InstallOptions::default()).await).await;

    assert!(
        events.iter().any(|e| matches!(
            e,
            PackageEvent::RecommendStarted { recommend_name, .. } if recommend_name == "rec"
        )),
        "control: the recommend started before the cancel: {events:#?}"
    );
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, PackageEvent::RecommendFailed { .. })),
        "{events:#?}"
    );
    assert!(
        events.iter().any(|e| matches!(
            e,
            PackageEvent::RecommendsUntried { names, .. } if names == &["rec".to_string()]
        )),
        "{events:#?}"
    );
}

// A recommend whose own command ran and failed is reported failed, and is not
// among the untried, whether the cancel interrupted the command or landed while
// the command failed for its own reason. Only what happened decides it, never
// the token's state afterwards.
#[tokio::test]
async fn a_recommend_whose_command_failed_under_a_cancel_is_reported_failed() {
    use selfie::{
        commands::CommandError,
        fs::RealFileSystem,
        package::{
            SpecOrigin, git_adapter::GixGitStatusProvider, repository::YamlPackageRepository,
            service::PackageServiceImpl,
        },
    };
    use test_common::{FakeCommandRunner, config::service_test_config_with_dir};
    use tokio_util::sync::CancellationToken;

    for (case, interrupted) in [
        ("interrupted", true),
        ("failed as the cancel landed", false),
    ] {
        let temp_dir = TempDir::new().unwrap();
        let package_dir = temp_dir.path().to_path_buf();
        std::fs::write(
            package_dir.join("root.yml"),
            "name: root\nenvironments:\n  test:\n    check: \"check-root\"\n    install: \
             \"install-root\"\n    recommends:\n      - rec\n",
        )
        .unwrap();
        std::fs::write(
            package_dir.join("rec.yml"),
            "name: rec\nenvironments:\n  test:\n    check: \"check-rec\"\n    install: \
             \"install-rec\"\n",
        )
        .unwrap();

        let token = CancellationToken::new();
        let runner = FakeCommandRunner::new()
            .succeeding("check-root", b"")
            .failing("check-rec", b"");
        let runner = if interrupted {
            runner.erroring(
                "install-rec",
                CommandError::Cancelled {
                    command: "install-rec".to_string(),
                    working_directory: package_dir.clone(),
                },
            )
        } else {
            runner.failing("install-rec", b"no such formula")
        }
        .cancelling("install-rec", &token);
        let config = service_test_config_with_dir(&package_dir);
        let service = PackageServiceImpl::new(
            YamlPackageRepository::new(
                RealFileSystem,
                package_dir.clone(),
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
        );

        let events = collect_events(service.install("root", InstallOptions::default()).await).await;

        assert!(
            events.iter().any(|e| matches!(
                e,
                PackageEvent::RecommendFailed { recommend_name, .. } if recommend_name == "rec"
            )),
            "{case}: the recommend must be reported failed: {events:#?}"
        );
        assert!(
            !events.iter().any(|e| matches!(
                e,
                PackageEvent::RecommendsUntried { names, .. } if names.iter().any(|n| n == "rec")
            )),
            "{case}: a recommend that ran is not untried: {events:#?}"
        );
    }
}
