use futures::StreamExt;
use selfie::package::event::{
    AuditResult, CheckResult, EventStream, OperationResult, PackageEvent,
};
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
        if let PackageEvent::Completed { result, .. } = &event {
            final_result = Some(result.clone());
        }

        if let Some(json) = event_to_json(&event) {
            data_events.push(json);
        }
    }

    let (success, result_data) = match final_result {
        // An operation that completed while refusing part of its work is
        // reported as an error result, the same call the CLI answers with exit
        // code 1. The payload's `status` moves in step with the envelope: a
        // caller reading `"status": "success"` inside a result flagged as an
        // error gets the same two-meanings-in-one-field problem selfie-c28 is
        // about, one layer up.
        // `refused` is its own field, not just a number inside `message`: the
        // tool description promises a count, and an assistant should not have to
        // parse one out of prose.
        Some(OperationResult::Success(s)) if s.had_refusals() => (
            false,
            serde_json::json!({
                "status": "refused",
                "refused": s.refused_count(),
                "message": format!("{s}"),
            }),
        ),
        Some(OperationResult::Success(s)) => (
            true,
            serde_json::json!({ "status": "success", "message": format!("{s}") }),
        ),
        Some(OperationResult::Failure(f)) => (
            false,
            serde_json::json!({ "status": "failure", "error": format!("{f}") }),
        ),
        None => (
            false,
            serde_json::json!({ "status": "unknown", "error": "No completion event received" }),
        ),
    };

    EventCollectorResult {
        success,
        data: serde_json::json!({ "result": result_data, "data": data_events }),
    }
}

fn event_to_json(event: &PackageEvent) -> Option<Value> {
    match event {
        PackageEvent::CheckResultCompleted { check_result, .. } => Some(serde_json::json!({
            "type": "check_result",
            "package": &check_result.package_name,
            "environment": &check_result.environment,
            "command": &check_result.check_command,
            "status": check_status_label(&check_result.result),
        })),
        PackageEvent::AuditResultCompleted { audit_result, .. } => Some(serde_json::json!({
            "type": "audit_result",
            "package": &audit_result.package_name,
            "environment": &audit_result.environment,
            "command": &audit_result.audit_command,
            "status": format!("{}", audit_result.result),
            "details": audit_details(&audit_result.result),
        })),
        PackageEvent::PackageInfoLoaded { package_info, .. } => Some(serde_json::json!({
            "type": "package_info",
            "name": &package_info.name,
            "description": &package_info.description,
            "homepage": &package_info.homepage,
            "environments": &package_info.environments,
            "current_environment": &package_info.current_environment,
            "git_status": git_status_label(package_info.git_status.as_ref()),
        })),
        PackageEvent::EnvironmentStatusChecked {
            environment_status, ..
        } => {
            let dep_statuses = dep_statuses_to_json(&environment_status.dependency_statuses);
            let rec_statuses = dep_statuses_to_json(&environment_status.recommend_statuses);

            Some(serde_json::json!({
                "type": "environment_status",
                "environment": &environment_status.environment_name,
                "is_current": environment_status.is_current,
                "install_command": &environment_status.install_command,
                "check_command": &environment_status.check_command,
                "dependencies": &environment_status.dependencies,
                "dependency_statuses": dep_statuses,
                "recommends": &environment_status.recommends,
                "recommend_statuses": rec_statuses,
                "status": environment_status.status.as_ref().map(|s| match s {
                    selfie::package::event::EnvironmentStatus::Installed => "installed",
                    selfie::package::event::EnvironmentStatus::NotInstalled => "not installed",
                    selfie::package::event::EnvironmentStatus::Unknown(_) => "unknown",
                }),
            }))
        }
        PackageEvent::PackageListReady { .. } => None, // CLI-specific event for spinner setup
        PackageEvent::PackageListItemCompleted { package_item, .. } => Some(serde_json::json!({
            "type": "package_list_item",
            "name": &package_item.name,
            "environments": &package_item.environments,
            "status": package_item.status.as_ref().map(check_status_label),
        })),
        PackageEvent::ValidationResultCompleted {
            validation_result, ..
        } => {
            let issues: Vec<Value> = validation_result
                .issues
                .iter()
                .map(|i| serde_json::json!({ "level": validation_level_label(&i.level), "category": &i.category, "field": &i.field, "message": &i.message, "suggestion": &i.suggestion, "location": &i.location }))
                .collect();
            Some(
                serde_json::json!({ "type": "validation_result", "package": &validation_result.package_name, "status": format!("{}", validation_result.status), "issues": issues }),
            )
        }
        PackageEvent::RemovalDependencyInfo {
            dependent_packages,
            package_name,
            ..
        } => Some(
            serde_json::json!({ "type": "removal_dependency_info", "package": package_name, "dependent_packages": dependent_packages }),
        ),
        PackageEvent::DotfileCleanupInfo {
            package_name,
            dotfile_targets,
            ..
        } => Some(serde_json::json!({
            "type": "dotfile_cleanup_info",
            "package_name": package_name,
            "dotfile_targets": dotfile_targets,
        })),
        PackageEvent::Info { output, .. } => Some(serde_json::json!({
            "type": "output",
            "text": match output {
                selfie::package::event::ConsoleOutput::Stdout(s) => s,
                selfie::package::event::ConsoleOutput::Stderr(s) => s,
            },
        })),
        PackageEvent::SpecListItemCompleted { spec_item, .. } => Some(serde_json::json!({
            "type": "spec_list_item",
            "name": &spec_item.name,
            "description": &spec_item.description,
            "environments": &spec_item.environments,
            "git_status": git_status_label(spec_item.git_status.as_ref()),
        })),
        PackageEvent::SpecListLoaded { spec_list, .. } => {
            let invalid: Vec<Value> = spec_list
                .invalid_packages
                .iter()
                .map(|ip| serde_json::json!({ "path": &ip.path, "error": &ip.error }))
                .collect();
            Some(serde_json::json!({
                "type": "spec_list_summary",
                "environment": &spec_list.current_environment,
                "package_directory": &spec_list.package_directory,
                "total_specs": spec_list.specs.len(),
                "invalid_packages": invalid,
            }))
        }
        PackageEvent::RecommendStarted { recommend_name, .. } => Some(serde_json::json!({
            "type": "recommend_started",
            "package": recommend_name,
        })),
        PackageEvent::RecommendSucceeded { recommend_name, .. } => Some(serde_json::json!({
            "type": "recommend_succeeded",
            "package": recommend_name,
        })),
        PackageEvent::RecommendFailed {
            recommend_name,
            error,
            ..
        } => Some(serde_json::json!({
            "type": "recommend_failed",
            "package": recommend_name,
            "error": error,
        })),
        PackageEvent::Warning { message, .. } => Some(serde_json::json!({
            "type": "warning",
            "message": message,
        })),
        PackageEvent::DotfileDeploying { source, target, .. } => Some(serde_json::json!({
            "type": "dotfile_deploying",
            "source": source,
            "target": target,
        })),
        PackageEvent::DotfileDeployed { source, target, .. } => Some(serde_json::json!({
            "type": "dotfile_deployed",
            "source": source,
            "target": target,
        })),
        PackageEvent::DotfileSkipped {
            source,
            target,
            reason,
            ..
        } => Some(serde_json::json!({
            "type": "dotfile_skipped",
            "source": source,
            "target": target,
            "reason": reason,
        })),
        PackageEvent::DotfileConflict {
            source,
            target,
            diff,
            ..
        } => Some(serde_json::json!({
            "type": "dotfile_conflict",
            "source": source,
            "target": target,
            "diff": diff,
        })),
        PackageEvent::DotfileDriftDetected {
            target, drift_type, ..
        } => Some(serde_json::json!({
            "type": "dotfile_drift_detected",
            "target": target,
            "drift_type": drift_type,
        })),
        PackageEvent::PostInstallNote {
            package_name, note, ..
        } => Some(serde_json::json!({
            "type": "post_install_note",
            "package": package_name,
            "note": note,
        })),
        PackageEvent::SyncRepoStatus {
            repo_root,
            branch,
            modified_count,
            staged_count,
            untracked_count,
            deleted_count,
            ahead,
            behind,
            ..
        } => Some(serde_json::json!({
            "type": "sync_repo_status",
            "repo_root": repo_root.display().to_string(),
            "branch": branch,
            "modified_count": modified_count,
            "staged_count": staged_count,
            "untracked_count": untracked_count,
            "deleted_count": deleted_count,
            "ahead": ahead,
            "behind": behind,
        })),
        PackageEvent::SyncDriftSummary {
            drifted_targets,
            total_deployed,
            ..
        } => Some(serde_json::json!({
            "type": "sync_drift_summary",
            "drifted_targets": drifted_targets,
            "total_deployed": total_deployed,
        })),
        PackageEvent::SyncCommitCreated {
            package_name,
            message,
            ..
        } => Some(serde_json::json!({
            "type": "sync_commit_created",
            "package": package_name,
            "message": message,
        })),
        _ => None,
    }
}

fn git_status_label(status: Option<&selfie::package::git::GitFileStatus>) -> Value {
    use selfie::package::git::GitFileStatus;
    match status {
        Some(GitFileStatus::Clean) => Value::String("clean".to_string()),
        Some(GitFileStatus::Modified) => Value::String("modified".to_string()),
        Some(GitFileStatus::Staged) => Value::String("staged".to_string()),
        Some(GitFileStatus::StagedAndModified) => Value::String("staged_and_modified".to_string()),
        Some(GitFileStatus::Untracked) => Value::String("untracked".to_string()),
        Some(GitFileStatus::NotInRepo) => Value::String("not_in_repo".to_string()),
        None => Value::Null,
    }
}

/// Label a validation issue's severity for an assistant reading the JSON.
///
/// Without this the three levels are indistinguishable, and an informational
/// notice — which never makes a package invalid — reads as a defect alongside a
/// `status` that says the package validated successfully.
fn validation_level_label(level: &selfie::package::event::ValidationLevel) -> &'static str {
    use selfie::package::event::ValidationLevel;

    match level {
        ValidationLevel::Error => "error",
        ValidationLevel::Warning => "warning",
        ValidationLevel::Info => "info",
    }
}

fn check_status_label(result: &CheckResult) -> &'static str {
    match result {
        CheckResult::Success { .. } => "installed",
        CheckResult::Failed { .. } => "not installed",
        CheckResult::CommandNotFound => "check command not found",
        CheckResult::NoCheckCommand => "no check command defined",
        CheckResult::Error(_) => "error",
    }
}

fn dep_statuses_to_json(statuses: &[selfie::package::event::DependencyStatus]) -> Vec<Value> {
    statuses
        .iter()
        .map(|dep| {
            let (status, reason) = match &dep.status {
                selfie::package::event::EnvironmentStatus::Installed => ("installed", None),
                selfie::package::event::EnvironmentStatus::NotInstalled => ("not installed", None),
                selfie::package::event::EnvironmentStatus::Unknown(reason) => {
                    ("unknown", Some(reason.as_str()))
                }
            };
            serde_json::json!({
                "name": &dep.name,
                "status": status,
                "reason": reason,
            })
        })
        .collect()
}

fn audit_details(result: &AuditResult) -> Value {
    match result {
        AuditResult::Clean { sources } => serde_json::json!({ "sources": sources }),
        AuditResult::Conflicts { sources, expected } => {
            serde_json::json!({ "sources": sources, "expected": expected })
        }
        AuditResult::NotInstalled | AuditResult::NoAuditCommand => Value::Null,
        AuditResult::Error(e) => serde_json::json!({ "error": e }),
    }
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

    /// An apply that refused an entry comes back as an error result, and the
    /// payload agrees with the envelope.
    ///
    /// `tool_result` turns `success: false` into `CallToolResult::error`, so an
    /// assistant is told the call did not do what was asked. The payload's
    /// `status` has to move with it: `"status": "success"` inside a result
    /// flagged as an error is the same two-meanings-in-one-field defect as the
    /// counter selfie-c28 is about.
    #[tokio::test]
    async fn an_apply_that_refused_an_entry_is_an_error_result() {
        use selfie::package::event::{OperationSuccess, StepCount};

        let events = vec![PackageEvent::Completed {
            operation_info: test_op_info(),
            result: OperationResult::Success(OperationSuccess::DotfilesApplied {
                deployed_count: 0,
                skipped_count: 0,
                conflict_count: 0,
                refused_count: 1,
                environment: "test".to_string(),
                steps_completed: StepCount::new(1, 1),
            }),
        }];

        let result = collect_events(Box::pin(stream::iter(events))).await;

        assert!(!result.success);
        assert_eq!(result.data["result"]["status"], "refused");
        // The count is a field, because the tool description promises one. An
        // assistant looking for `refused` must not have to read `message`.
        assert_eq!(result.data["result"]["refused"], 1);
        assert!(
            result.data["result"]["message"]
                .as_str()
                .unwrap()
                .contains("1 refused"),
            "the count belongs in the message an assistant reads: {:?}",
            result.data["result"]["message"]
        );
    }

    /// Control: an apply with nothing refused is still a success result.
    #[tokio::test]
    async fn an_apply_that_refused_nothing_is_a_success_result() {
        use selfie::package::event::{OperationSuccess, StepCount};

        let events = vec![PackageEvent::Completed {
            operation_info: test_op_info(),
            result: OperationResult::Success(OperationSuccess::DotfilesApplied {
                deployed_count: 1,
                skipped_count: 0,
                conflict_count: 0,
                refused_count: 0,
                environment: "test".to_string(),
                steps_completed: StepCount::new(1, 1),
            }),
        }];

        let result = collect_events(Box::pin(stream::iter(events))).await;

        assert!(result.success);
        assert_eq!(result.data["result"]["status"], "success");
    }

    /// A failed command's output must not reach the JSON an assistant reads.
    ///
    /// This passes today and is a lock, not a fix: failures are rendered with
    /// `Display`, which names the command and its exit code and nothing else. The
    /// secret is placed on **stderr** deliberately — `CommandFailure::ExecutionFailed`
    /// has no `stdout` field, so a secret on stdout could not reach this code by
    /// any route and the test would pass without observing anything.
    #[tokio::test]
    async fn a_failed_command_leaks_no_output_into_the_json() {
        const SECRET: &str = "s3cr3t-v4lue-DO-NOT-LEAK";

        let events = vec![PackageEvent::Completed {
            operation_info: test_op_info(),
            result: OperationResult::Failure(OperationFailure::command_failed(
                "install-with-token".to_string(),
                Some(1),
                &format!("error: vault sealed, TOKEN={SECRET}"),
            )),
        }];

        let result = collect_events(Box::pin(stream::iter(events))).await;
        let json = serde_json::to_string(&result.data).unwrap();

        test_common::assert_secret_free(&json, SECRET, "the MCP JSON payload");
        // Control: the failure was rendered at all, and stays diagnosable.
        assert!(!result.success);
        assert!(
            json.contains("install-with-token"),
            "the failing command must still be named:\n{json}"
        );
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
        assert_eq!(result.data["data"][0]["status"], "installed");
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
    async fn test_collect_package_list_with_status() {
        use selfie::package::event::PackageListItem;

        let events = vec![
            PackageEvent::PackageListItemCompleted {
                operation_info: test_op_info(),
                package_item: PackageListItem {
                    name: "ripgrep".to_string(),

                    environments: vec!["macos".to_string()],
                    status: Some(CheckResult::Success {
                        stdout: "/opt/homebrew/bin/rg".to_string(),
                        stderr: String::new(),
                    }),
                },
            },
            PackageEvent::PackageListItemCompleted {
                operation_info: test_op_info(),
                package_item: PackageListItem {
                    name: "missing-pkg".to_string(),

                    environments: vec!["macos".to_string()],
                    status: Some(CheckResult::Failed {
                        stdout: String::new(),
                        stderr: "not found".to_string(),
                        exit_code: Some(1),
                    }),
                },
            },
            PackageEvent::Completed {
                operation_info: test_op_info(),
                result: OperationResult::Success(OperationSuccess::Generic("listed".to_string())),
            },
        ];

        let stream: EventStream = Box::pin(stream::iter(events));
        let result = collect_events(stream).await;

        assert!(result.success);
        assert_eq!(result.data["data"].as_array().unwrap().len(), 2);
        assert_eq!(result.data["data"][0]["type"], "package_list_item");
        assert_eq!(result.data["data"][0]["name"], "ripgrep");
        assert_eq!(result.data["data"][0]["status"], "installed");
        assert_eq!(result.data["data"][1]["name"], "missing-pkg");
        assert_eq!(result.data["data"][1]["status"], "not installed");
    }

    #[tokio::test]
    async fn test_collect_spec_list_items() {
        use selfie::package::event::SpecListItem;

        let events = vec![
            PackageEvent::SpecListItemCompleted {
                operation_info: test_op_info(),
                spec_item: SpecListItem {
                    name: "ripgrep".to_string(),

                    description: Some("Fast search tool".to_string()),
                    environments: vec!["macos".to_string(), "ubuntu".to_string()],
                    git_status: None,
                },
            },
            PackageEvent::SpecListLoaded {
                operation_info: test_op_info(),
                spec_list: selfie::package::event::SpecListData {
                    specs: vec![],
                    invalid_packages: vec![],
                    current_environment: "macos".to_string(),
                    package_directory: "/tmp/packages".to_string(),
                    environment_stats: Default::default(),
                    show_all: false,
                },
            },
            PackageEvent::Completed {
                operation_info: test_op_info(),
                result: OperationResult::Success(OperationSuccess::spec_list_generated(
                    1,
                    0,
                    "macos".to_string(),
                    StepCount::new(2, 2),
                )),
            },
        ];

        let stream: EventStream = Box::pin(stream::iter(events));
        let result = collect_events(stream).await;

        assert!(result.success);
        assert_eq!(result.data["data"].as_array().unwrap().len(), 2);
        assert_eq!(result.data["data"][0]["type"], "spec_list_item");
        assert_eq!(result.data["data"][0]["name"], "ripgrep");
        assert_eq!(result.data["data"][0]["description"], "Fast search tool");
        assert_eq!(result.data["data"][0]["environments"][0], "macos");
        assert_eq!(result.data["data"][1]["type"], "spec_list_summary");
        assert_eq!(result.data["data"][1]["environment"], "macos");
        assert_eq!(result.data["data"][1]["total_specs"], 0);
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

    #[tokio::test]
    async fn test_collect_environment_status_with_dependency_statuses() {
        use selfie::package::event::{DependencyStatus, EnvironmentStatus, EnvironmentStatusData};

        let events = vec![
            PackageEvent::EnvironmentStatusChecked {
                operation_info: test_op_info(),
                environment_status: EnvironmentStatusData {
                    environment_name: "macos".to_string(),
                    is_current: true,
                    install_command: "brew install git".to_string(),
                    check_command: Some("which git".to_string()),
                    dependencies: vec!["curl".to_string(), "wget".to_string()],
                    dependency_statuses: vec![
                        DependencyStatus {
                            name: "curl".to_string(),
                            status: EnvironmentStatus::Installed,
                        },
                        DependencyStatus {
                            name: "wget".to_string(),
                            status: EnvironmentStatus::NotInstalled,
                        },
                    ],
                    recommends: vec![],
                    recommend_statuses: vec![],
                    status: Some(EnvironmentStatus::Installed),
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
        let env_data = &result.data["data"][0];
        assert_eq!(env_data["type"], "environment_status");
        assert_eq!(env_data["environment"], "macos");
        assert_eq!(env_data["status"], "installed");

        // dependencies remains a stable Vec<String>
        assert_eq!(env_data["dependencies"][0], "curl");
        assert_eq!(env_data["dependencies"][1], "wget");

        // dependency_statuses has the rich status objects
        assert_eq!(env_data["dependency_statuses"][0]["name"], "curl");
        assert_eq!(env_data["dependency_statuses"][0]["status"], "installed");
        assert!(env_data["dependency_statuses"][0]["reason"].is_null());
        assert_eq!(env_data["dependency_statuses"][1]["name"], "wget");
        assert_eq!(
            env_data["dependency_statuses"][1]["status"],
            "not installed"
        );
        assert!(env_data["dependency_statuses"][1]["reason"].is_null());
    }
}
