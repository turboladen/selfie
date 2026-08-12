//! Event stream processing helpers to eliminate duplication in async service tests.

use selfie::package::event::{
    EventStream, OperationContext, OperationInfo, OperationResult, PackageEvent,
    metadata::OperationType,
};
use std::time::Instant;

/// Collect every event a stream produces.
pub async fn collect_events(mut stream: EventStream) -> Vec<PackageEvent> {
    let mut events = Vec::new();
    while let Some(event) = futures::StreamExt::next(&mut stream).await {
        events.push(event);
    }
    events
}

/// The operation's final result, if the events contain a completion.
#[must_use]
pub fn get_operation_result(events: &[PackageEvent]) -> Option<&OperationResult> {
    for event in events {
        if let PackageEvent::Completed { result, .. } = event {
            return Some(result);
        }
    }
    None
}

/// How many events satisfy `predicate`.
pub fn count_events_of_type<F>(events: &[PackageEvent], predicate: F) -> usize
where
    F: Fn(&PackageEvent) -> bool,
{
    events.iter().filter(|e| predicate(e)).count()
}

/// Assert the standard success sequence: exactly one started event, at least
/// one progress event, exactly one completed event, and a successful result.
///
/// # Panics
/// Panics if the event sequence doesn't match expected successful operation pattern.
pub fn assert_successful_operation(events: &[PackageEvent]) {
    // Should have exactly one started event
    assert_eq!(
        count_events_of_type(events, |e| matches!(e, PackageEvent::Started { .. })),
        1,
        "Should have exactly one started event"
    );

    // Should have at least one progress event
    assert!(
        count_events_of_type(events, |e| matches!(e, PackageEvent::Progress { .. })) > 0,
        "Should have at least one progress event"
    );

    // Should have exactly one completed event
    assert_eq!(
        count_events_of_type(events, |e| matches!(e, PackageEvent::Completed { .. })),
        1,
        "Should have exactly one completed event"
    );

    // Result should be successful
    let result = get_operation_result(events).expect("Should have an operation result");
    assert!(
        matches!(result, OperationResult::Success(_)),
        "Operation should be successful, got: {result:?}"
    );
}

/// Assert the operation completed with a failure result.
///
/// # Panics
/// Panics if the event sequence doesn't match expected failed operation pattern.
pub fn assert_failed_operation(events: &[PackageEvent]) {
    // Should have a completion result that is a failure
    let result = get_operation_result(events).expect("Should have an operation result");
    assert!(
        matches!(result, OperationResult::Failure(_)),
        "Operation result should be failure, got: {result:?}"
    );
}

/// Assert every string in `expected_steps` appears in some progress message.
///
/// # Panics
///
/// Panics if any `expected_steps` aren't in the `events` progress messages.
pub fn assert_has_progress_steps(events: &[PackageEvent], expected_steps: &[&str]) {
    let progress_messages: Vec<String> = events
        .iter()
        .filter_map(|e| {
            if let PackageEvent::Progress { message, .. } = e {
                Some(message.clone())
            } else {
                None
            }
        })
        .collect();

    for expected_step in expected_steps {
        assert!(
            progress_messages
                .iter()
                .any(|msg| msg.contains(expected_step)),
            "Expected progress step '{expected_step}' not found in messages: {progress_messages:?}"
        );
    }
}

/// The message from every error event.
#[must_use]
pub fn get_error_messages(events: &[PackageEvent]) -> Vec<String> {
    events
        .iter()
        .filter_map(|e| {
            if let PackageEvent::Error { message, .. } = e {
                Some(message.clone())
            } else {
                None
            }
        })
        .collect()
}

/// Assert no error event occurred.
///
/// # Panics
///
/// Panics if `events` has errors.
pub fn assert_no_errors(events: &[PackageEvent]) {
    let error_count = count_events_of_type(events, |e| matches!(e, PackageEvent::Error { .. }));
    assert_eq!(
        error_count,
        0,
        "Expected no error events, but found {} errors: {:?}",
        error_count,
        get_error_messages(events)
    );
}

/// Build an `OperationInfo` for constructing test events.
#[must_use]
pub fn create_test_operation_info(
    operation_type: &str,
    package_name: &str,
    environment: &str,
) -> OperationInfo {
    let op_type = match operation_type {
        "config_validate" => OperationType::ConfigValidate,
        "package_audit" => OperationType::PackageAudit,
        "package_check" => OperationType::PackageCheck,
        "package_create" => OperationType::PackageCreate,
        "spec_info" => OperationType::SpecInfo,
        "package_status" => OperationType::PackageStatus,
        "package_install" => OperationType::PackageInstall,
        "package_list" => OperationType::PackageList,
        "package_remove" => OperationType::PackageRemove,
        "package_update" => OperationType::PackageUpdate,
        "package_validate" => OperationType::PackageValidate,
        _ => OperationType::PackageCheck, // Default fallback
    };

    OperationInfo {
        id: "550e8400-e29b-41d4-a716-446655440000".parse().unwrap(),
        operation_type: op_type,
        package_name: package_name.to_string(),
        environment: environment.to_string(),
        context: OperationContext {
            package_path: None,
            target_environment: None,
        },
        timestamp: Instant::now(),
    }
}
