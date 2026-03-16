use futures::StreamExt;
use selfie::package::event::{AuditResult, EventStream, OperationResult, PackageEvent};
use serde_json::Value;

pub struct EventCollectorResult {
    pub success: bool,
    pub data: Value,
}

pub async fn collect_events(stream: EventStream) -> EventCollectorResult {
    let mut final_result = None;
    let mut data_events: Vec<Value> = Vec::new();

    tokio::pin!(stream);

    while let Some(event) = stream.next().await {
        match &event {
            PackageEvent::Completed { result, .. } => {
                final_result = Some(result.clone());
            }
            PackageEvent::CheckResultCompleted { check_result, .. } => {
                data_events.push(serde_json::json!({
                    "type": "check_result",
                    "package": &check_result.package_name,
                    "environment": &check_result.environment,
                    "command": &check_result.check_command,
                    "status": format!("{}", check_result.result),
                }));
            }
            PackageEvent::AuditResultCompleted { audit_result, .. } => {
                data_events.push(serde_json::json!({
                    "type": "audit_result",
                    "package": &audit_result.package_name,
                    "environment": &audit_result.environment,
                    "command": &audit_result.audit_command,
                    "status": format!("{}", audit_result.result),
                    "details": match &audit_result.result {
                        AuditResult::Clean { sources } =>
                            serde_json::json!({ "sources": sources }),
                        AuditResult::Conflicts { sources, expected } =>
                            serde_json::json!({ "sources": sources, "expected": expected }),
                        AuditResult::NotInstalled => serde_json::json!(null),
                        AuditResult::NoAuditCommand => serde_json::json!(null),
                        AuditResult::Error(e) => serde_json::json!({ "error": e }),
                    },
                }));
            }
            PackageEvent::PackageInfoLoaded { package_info, .. } => {
                data_events.push(serde_json::json!({
                    "type": "package_info",
                    "name": &package_info.name,
                    "version": &package_info.version,
                    "description": &package_info.description,
                    "homepage": &package_info.homepage,
                    "environments": &package_info.environments,
                    "current_environment": &package_info.current_environment,
                }));
            }
            PackageEvent::PackageListReady { packages, .. } => {
                let pkg_list: Vec<Value> = packages
                    .iter()
                    .map(|p| {
                        serde_json::json!({
                            "name": &p.name,
                            "version": &p.version,
                            "environments": &p.environments,
                        })
                    })
                    .collect();
                data_events.push(serde_json::json!({
                    "type": "package_list",
                    "packages": pkg_list,
                }));
            }
            PackageEvent::ValidationResultCompleted {
                validation_result, ..
            } => {
                let issues: Vec<Value> = validation_result
                    .issues
                    .iter()
                    .map(|i| {
                        serde_json::json!({
                            "category": &i.category,
                            "field": &i.field,
                            "message": &i.message,
                            "suggestion": &i.suggestion,
                        })
                    })
                    .collect();
                data_events.push(serde_json::json!({
                    "type": "validation_result",
                    "package": &validation_result.package_name,
                    "status": format!("{}", validation_result.status),
                    "issues": issues,
                }));
            }
            PackageEvent::RemovalDependencyInfo {
                dependent_packages,
                package_name,
                ..
            } => {
                data_events.push(serde_json::json!({
                    "type": "removal_dependency_info",
                    "package": package_name,
                    "dependent_packages": dependent_packages,
                }));
            }
            _ => {}
        }
    }

    let (success, result_data) = match final_result {
        Some(OperationResult::Success(s)) => (
            true,
            serde_json::json!({
                "status": "success",
                "message": format!("{s}"),
            }),
        ),
        Some(OperationResult::Failure(f)) => (
            false,
            serde_json::json!({
                "status": "failure",
                "error": format!("{f}"),
            }),
        ),
        None => (
            false,
            serde_json::json!({
                "status": "unknown",
                "error": "No completion event received",
            }),
        ),
    };

    let data = serde_json::json!({
        "result": result_data,
        "data": data_events,
    });

    EventCollectorResult { success, data }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::stream;
    use selfie::package::event::{
        AuditResultData, CheckResult, CheckResultData, OperationContext, OperationFailure,
        OperationInfo, OperationSuccess, StepCount, metadata::OperationType,
    };
    use std::time::Instant;
    use uuid::Uuid;

    fn test_op_info() -> OperationInfo {
        OperationInfo {
            id: Uuid::new_v4(),
            operation_type: OperationType::PackageCheck,
            package_name: "test-pkg".to_string(),
            environment: "test".to_string(),
            context: OperationContext::default(),
            timestamp: Instant::now(),
        }
    }

    #[tokio::test]
    async fn test_collect_success_with_check_result() {
        let events = vec![
            PackageEvent::CheckResultCompleted {
                operation_info: test_op_info(),
                check_result: CheckResultData {
                    package_name: "test-pkg".to_string(),
                    environment: "test".to_string(),
                    check_command: Some("which test".to_string()),
                    result: CheckResult::Success {
                        stdout: "found".to_string(),
                        stderr: String::new(),
                    },
                },
            },
            PackageEvent::Completed {
                operation_info: test_op_info(),
                result: OperationResult::Success(OperationSuccess::package_checked(
                    "test-pkg".to_string(),
                    "test".to_string(),
                    CheckResult::Success {
                        stdout: "found".to_string(),
                        stderr: String::new(),
                    },
                    StepCount::new(3, 3),
                )),
            },
        ];

        let stream: EventStream = Box::pin(stream::iter(events));
        let result = collect_events(stream).await;

        assert!(result.success);
        assert_eq!(result.data["result"]["status"], "success");
        assert_eq!(result.data["data"][0]["type"], "check_result");
        assert_eq!(result.data["data"][0]["package"], "test-pkg");
        assert_eq!(result.data["data"][0]["status"], "successfully");
    }

    #[tokio::test]
    async fn test_collect_failure() {
        let events = vec![PackageEvent::Completed {
            operation_info: test_op_info(),
            result: OperationResult::Failure(OperationFailure::Generic(
                "something went wrong".to_string(),
            )),
        }];

        let stream: EventStream = Box::pin(stream::iter(events));
        let result = collect_events(stream).await;

        assert!(!result.success);
        assert_eq!(result.data["result"]["status"], "failure");
        assert!(
            result.data["result"]["error"]
                .as_str()
                .unwrap()
                .contains("something went wrong")
        );
    }

    #[tokio::test]
    async fn test_collect_no_completion_event() {
        let events: Vec<PackageEvent> = vec![];
        let stream: EventStream = Box::pin(stream::iter(events));
        let result = collect_events(stream).await;

        assert!(!result.success);
        assert_eq!(result.data["result"]["status"], "unknown");
    }

    #[tokio::test]
    async fn test_collect_audit_result() {
        let events = vec![
            PackageEvent::AuditResultCompleted {
                operation_info: test_op_info(),
                audit_result: AuditResultData {
                    package_name: "prettier".to_string(),
                    environment: "macos".to_string(),
                    audit_command: Some("audit-cmd".to_string()),
                    result: AuditResult::Conflicts {
                        sources: vec!["bun".to_string(), "npm".to_string()],
                        expected: vec!["bun".to_string(), "prettier".to_string()],
                    },
                },
            },
            PackageEvent::Completed {
                operation_info: test_op_info(),
                result: OperationResult::Success(OperationSuccess::Generic("done".to_string())),
            },
        ];

        let stream: EventStream = Box::pin(stream::iter(events));
        let result = collect_events(stream).await;

        assert!(result.success);
        assert_eq!(result.data["data"][0]["type"], "audit_result");
        assert_eq!(result.data["data"][0]["status"], "with conflicts");
        assert_eq!(result.data["data"][0]["details"]["sources"][0], "bun");
        assert_eq!(result.data["data"][0]["details"]["sources"][1], "npm");
        assert_eq!(result.data["data"][0]["details"]["expected"][0], "bun");
    }

    #[tokio::test]
    async fn test_collect_removal_dependency_info() {
        let events = vec![
            PackageEvent::RemovalDependencyInfo {
                operation_info: test_op_info(),
                package_name: "target-pkg".to_string(),
                dependent_packages: vec!["dep-a".to_string(), "dep-b".to_string()],
            },
            PackageEvent::Completed {
                operation_info: test_op_info(),
                result: OperationResult::Success(OperationSuccess::Generic("removed".to_string())),
            },
        ];

        let stream: EventStream = Box::pin(stream::iter(events));
        let result = collect_events(stream).await;

        assert!(result.success);
        assert_eq!(result.data["data"][0]["type"], "removal_dependency_info");
        assert_eq!(result.data["data"][0]["package"], "target-pkg");
        assert_eq!(result.data["data"][0]["dependent_packages"][0], "dep-a");
    }
}
