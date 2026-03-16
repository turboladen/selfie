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
