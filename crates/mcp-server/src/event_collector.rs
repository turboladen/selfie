use futures::StreamExt;
use selfie::package::SpecOrigin;
use selfie::package::event::{
    AuditResult, CheckResult, EventStream, NoSuchPackageReason, OperationFailure, OperationResult,
    PackageEvent,
};
use serde_json::Value;

pub struct EventCollectorResult {
    pub success: bool,
    pub data: Value,
}

/// The result payload for a failed operation.
fn failure_json(failure: &OperationFailure) -> Value {
    let mut payload = serde_json::json!({ "status": "failure", "error": format!("{failure}") });
    // A field, so an assistant can tell a typo from a spec that failed to load
    // without matching the sentence in `error`.
    if let OperationFailure::NoSuchPackage { reason, .. } = failure {
        payload["reason"] = Value::from(match reason {
            NoSuchPackageReason::NotFound => "not_found",
            NoSuchPackageReason::MaybeInUnlistableDirectory => "maybe_in_unlistable_directory",
            NoSuchPackageReason::NotLoaded => "not_loaded",
        });
    }
    payload
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
        Some(OperationResult::Failure(f)) => (false, failure_json(&f)),
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

/// The `origin` value a dotfile row carries, derived from the package rather
/// than chosen by the caller.
///
/// The two strings are the ones this tool has always emitted; what changes is
/// where they come from. A call site pairing the dotfiles repository with
/// "packages" used to mislabel every row it produced, with nothing to catch it.
// No catch-all: an origin added later has to state which label it carries here
// rather than inherit one that was written before it existed.
pub(crate) fn origin_label(origin: SpecOrigin) -> &'static str {
    match origin {
        SpecOrigin::PackageDirectory => "packages",
        SpecOrigin::DotfilesDirectory => "dotfiles",
        SpecOrigin::Memory => "memory",
    }
}

/// Render one dotfile entry as JSON for `selfie_dotfiles_list`.
///
/// Reports where content comes from without producing any of it: var names and
/// the command string come from the package file and are references, not values.
/// Nothing here runs a command or renders a template, so enumeration cannot leak
/// a secret or trigger an authentication prompt.
pub(crate) fn dotfile_entry_json(
    package: &str,
    scope: Option<&str>,
    entry: &selfie::package::DotfileEntry,
    origin: &str,
) -> serde_json::Value {
    use selfie::package::ContentSource;

    let mut value = serde_json::json!({
        "package": package,
        "environment": scope,
        "target": entry.target(),
        "origin": origin,
    });
    let map = value.as_object_mut().expect("constructed as an object");

    match entry.content_source() {
        Ok(ContentSource::RepoFile(source)) => {
            map.insert("kind".into(), "file".into());
            map.insert("source".into(), source.into());
        }
        Ok(ContentSource::Template { source, vars }) => {
            map.insert("kind".into(), "template".into());
            map.insert("source".into(), source.into());
            map.insert(
                "vars".into(),
                vars.keys().map(String::as_str).collect::<Vec<_>>().into(),
            );
        }
        Ok(ContentSource::Provider(command)) => {
            map.insert("kind".into(), "command".into());
            map.insert("command".into(), command.into());
        }
        // The reason, not a generic string: an assistant reading this is the
        // caller least able to guess which of the possible defects applies, and
        // naming the key or the var is what lets it propose the actual fix.
        Err(invalid) => {
            map.insert("kind".into(), "invalid".into());
            map.insert("error".into(), invalid.to_string().into());
        }
    }

    value
}

/// One parse failure as fields, shared by every surface that reports one.
///
/// `kind` is what a caller branches on; `reason` is prose to display. `line` and
/// `column` are null for the kinds that carry no location — absent because there
/// is none, not because nothing was wired.
fn parse_failure_json(error: &selfie::package::port::PackageParseError) -> Value {
    use selfie::package::port::PackageParseKind;

    // One match, and no catch-all: a sixth kind has to state here whether it
    // reports a location, rather than inheriting `None` from an arm that never
    // considered it.
    let (at, reason) = match error.kind() {
        PackageParseKind::Yaml { source } => (source.location(), source.reason()),
        other @ (PackageParseKind::Io { .. }
        | PackageParseKind::Unreadable { .. }
        | PackageParseKind::IrregularFile { .. }
        | PackageParseKind::Refused { .. }) => (None, other.to_string()),
    };

    serde_json::json!({
        "path": error.package_path().display().to_string(),
        "kind": error.kind().label(),
        "reason": reason,
        "line": at.map(|at| at.line()),
        "column": at.map(|at| at.column()),
    })
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
                .map(parse_failure_json)
                .collect();
            Some(serde_json::json!({
                "type": "spec_list_summary",
                "environment": &spec_list.current_environment,
                "package_directory": &spec_list.package_directory,
                "total_specs": spec_list.specs.len(),
                "invalid_packages": invalid,
            }))
        }
        PackageEvent::DotfileListLoaded { dotfile_list, .. } => {
            // One row per ENTRY, not per package: a caller asking what is
            // deployed where is asking about entries, and a package carrying
            // three of them is three answers.
            let entries: Vec<Value> = dotfile_list
                .packages
                .iter()
                .flat_map(|pkg| {
                    let origin = origin_label(pkg.origin());
                    pkg.dotfiles_with_scope()
                        .into_iter()
                        .map(move |(scope, entry)| {
                            dotfile_entry_json(pkg.name(), scope, entry, origin)
                        })
                })
                .collect();

            // A refused package is not a package with no dotfiles, and an
            // assistant reading `total` cannot tell those apart unless the
            // refusals are their own field.
            let refused: Vec<Value> = dotfile_list
                .refused
                .iter()
                .map(|r| {
                    serde_json::json!({
                        "package": &r.package_name,
                        "path": &r.path,
                        "reason": &r.reason,
                    })
                })
                .collect();

            Some(serde_json::json!({
                "type": "dotfile_list",
                "package_directory": &dotfile_list.package_directory,
                "dotfiles_directory": &dotfile_list.dotfiles_directory,
                "total": entries.len(),
                "entries": entries,
                "refused": refused,
            }))
        }
        PackageEvent::PackageListLoaded { package_list, .. } => {
            let invalid: Vec<Value> = package_list
                .invalid_packages
                .iter()
                .map(parse_failure_json)
                .collect();
            Some(serde_json::json!({
                "type": "package_list_summary",
                "environment": &package_list.current_environment,
                "package_directory": &package_list.package_directory,
                "total_packages": package_list.valid_packages.len(),
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
        // `drift_type` stays the bare classification an assistant can match on;
        // the explanation is its own field rather than appended prose.
        PackageEvent::DotfileDriftDetected {
            target,
            drift_type,
            reason,
            ..
        } => Some(serde_json::json!({
            "type": "dotfile_drift_detected",
            "target": target,
            "drift_type": drift_type,
            "reason": reason,
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
            refused_count,
            unloaded_specs,
            ..
        } => Some(serde_json::json!({
            "type": "sync_drift_summary",
            "drifted_targets": drifted_targets,
            "total_deployed": total_deployed,
            "refused_count": refused_count,
            "unloaded_specs": unloaded_specs,
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

        // The reason, the kind and the location as separate fields. A caller
        // branches on `kind` rather than reading the sentence: `reason` is prose to
        // display, and matching on it is what this whole area was rewritten to stop
        // callers having to do.
        //
        // `line` and `column` are null for the four kinds that have no location.
        // Absent because there is none, not because nobody wired it.
        PackageEvent::SpecSkipped { error, .. } => {
            let mut row = parse_failure_json(error);
            row.as_object_mut()
                .expect("constructed as an object")
                .insert("type".into(), "spec_skipped".into());
            Some(row)
        }

        // Listed rather than matched with `_`, so a variant added later is a
        // compile error here instead of vanishing from every tool's output.
        //
        // `Completed` is read by `collect_events` for the operation's result
        // rather than emitted as a data event, and the lifecycle and log variants
        // carry nothing a tool caller acts on.
        PackageEvent::Started { .. }
        | PackageEvent::Progress { .. }
        | PackageEvent::Completed { .. }
        | PackageEvent::Canceled { .. }
        | PackageEvent::Trace { .. }
        | PackageEvent::Debug { .. }
        | PackageEvent::Error { .. } => None,
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

    // An apply that refused an entry comes back as an error result, and the
    // payload agrees with the envelope.
    //
    // `tool_result` turns `success: false` into `CallToolResult::error`, so an
    // assistant is told the call did not do what was asked. The payload's
    // `status` has to move with it: `"status": "success"` inside a result
    // flagged as an error is the same two-meanings-in-one-field defect as the
    // counter this distinguishes.
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

    // Control: an apply with nothing refused is still a success result.
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

    // An assistant reading `sync_drift_summary` needs the same count the CLI
    // warns about, so it can tell a run that skipped specs from one that
    // checked everything.
    #[tokio::test]
    async fn sync_drift_summary_carries_the_unloaded_spec_count() {
        let events = vec![
            PackageEvent::SyncDriftSummary {
                operation_info: test_op_info(),
                drifted_targets: vec![],
                total_deployed: 3,
                refused_count: 0,
                unloaded_specs: 2,
            },
            PackageEvent::Completed {
                operation_info: test_op_info(),
                result: OperationResult::Success(OperationSuccess::Generic("done".to_string())),
            },
        ];

        let result = collect_events(Box::pin(stream::iter(events))).await;

        assert_eq!(result.data["data"][0]["unloaded_specs"], 2);
    }

    // The reason is a field, so telling a typo from a spec that failed to load
    // does not require reading `error`.
    #[tokio::test]
    async fn a_named_package_that_could_not_be_found_carries_its_reason() {
        let events = vec![PackageEvent::Completed {
            operation_info: test_op_info(),
            result: OperationResult::Failure(OperationFailure::NoSuchPackage {
                name: "bat".to_string(),
                reason: NoSuchPackageReason::NotLoaded,
            }),
        }];

        let result = collect_events(Box::pin(stream::iter(events))).await;

        assert!(!result.success);
        assert_eq!(result.data["result"]["status"], "failure");
        assert_eq!(result.data["result"]["reason"], "not_loaded");
    }

    // A failed command's output must not reach the JSON an assistant reads.
    //
    // This passes today and is a lock, not a fix: failures are rendered with
    // `Display`, which names the command and its exit code and nothing else. The
    // secret is placed on **stderr** deliberately — `CommandFailure::ExecutionFailed`
    // has no `stdout` field, so a secret on stdout could not reach this code by
    // any route and the test would pass without observing anything.
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

    // A fixture value, never a real credential. Not path-shaped: the scan uses a
    // twelve-character window, so a path-like value matches ordinary output and
    // passes for the wrong reason.
    const SECRET: &str = "Xq7Rm2Kz9Wp4Ns6Tv8Bh3Gd5";

    // A skipped spec reaches a tool caller as fields it can branch on, and the
    // kind is what it branches on: `reason` is prose to display, and matching on
    // prose is what the fields exist to spare a caller.
    #[tokio::test]
    async fn a_skipped_spec_carries_its_kind_and_location_as_fields() {
        let spec = format!(
            "name: creds\ndotfiles:\n  - command: op read op://vault/item/field\n    \
             vars:\n      token: {SECRET}\n    target: ~/.npmrc\nenvironments: {{oops\n"
        );
        let source = selfie::yaml::parse::<selfie::package::Package>(&spec)
            .expect_err("the fixture must not parse");
        let error = selfie::package::port::PackageParseError::new(
            "/packages/creds.yml",
            selfie::package::port::PackageParseKind::Yaml { source },
        );

        let events = vec![PackageEvent::SpecSkipped {
            operation_info: test_op_info(),
            error,
        }];
        let stream: EventStream = Box::pin(stream::iter(events.clone()));
        let result = collect_events(stream).await;

        let row = &result.data["data"][0];
        assert_eq!(row["type"], "spec_skipped");
        assert_eq!(row["path"], "/packages/creds.yml");
        assert_eq!(row["kind"], "yaml");
        assert_eq!(row["line"], 7);
        assert_eq!(row["column"], 15);
        // The location is in its own fields, so the sentence must not repeat it.
        assert_eq!(row["reason"], "unclosed bracket '{'");

        // And none of the file it was reading.
        test_common::assert_secret_free(&result.data.to_string(), SECRET, "the collected JSON");
        for event in &events {
            test_common::assert_secret_free(&format!("{event:?}"), SECRET, "an event");
        }
    }

    // The only test of this arm. Without it a regression could drop `refused`, or
    // go back to labelling every row from the loop it stands in rather than from
    // the package, and the rest of the suite would stay green -- the CLI renders
    // this event through entirely different code.
    #[tokio::test]
    async fn a_dotfile_listing_carries_both_origins_and_its_refusals() {
        use selfie::package::{DotfileEntry, PackageBuilder, SpecOrigin};

        let from_packages = PackageBuilder::default()
            .name("bat")
            .origin(SpecOrigin::PackageDirectory)
            .dotfiles(vec![DotfileEntry::new("bat.conf", "~/.config/bat/config")])
            .build();
        let from_dotfiles = PackageBuilder::default()
            .name("fish")
            .origin(SpecOrigin::DotfilesDirectory)
            .dotfiles(vec![DotfileEntry::new("config.fish", "~/.config/fish/c")])
            .build();

        let stream: EventStream = Box::pin(stream::iter(vec![PackageEvent::DotfileListLoaded {
            operation_info: test_op_info(),
            dotfile_list: selfie::package::event::DotfileListData {
                packages: vec![from_packages, from_dotfiles],
                refused: vec![selfie::package::event::RefusedSpec {
                    package_name: "shadowed".to_string(),
                    path: "/packages/shadowed.yml".to_string(),
                    reason: "unrecognized top-level key `_dotfiles`".to_string(),
                }],
                package_directory: "/packages".to_string(),
                dotfiles_directory: "/dotfiles".to_string(),
            },
        }]));
        let result = collect_events(stream).await;

        let row = &result.data["data"][0];
        assert_eq!(row["type"], "dotfile_list");
        assert_eq!(row["total"], 2);

        // Each row's origin comes from its own package, not from a literal.
        let origins: Vec<&str> = row["entries"]
            .as_array()
            .expect("entries is an array")
            .iter()
            .map(|e| e["origin"].as_str().expect("origin is a string"))
            .collect();
        assert_eq!(origins, vec!["packages", "dotfiles"]);

        let refused = &row["refused"][0];
        assert_eq!(refused["package"], "shadowed");
        assert_eq!(refused["path"], "/packages/shadowed.yml");
        assert!(
            refused["reason"]
                .as_str()
                .expect("reason is a string")
                .contains("_dotfiles"),
            "the reason must survive to the caller, got: {}",
            refused["reason"]
        );
    }

    // The listing rows carry the same shape as a skipped spec, so a caller learns
    // one contract rather than two: `reason` beside the kind and the location,
    // never one rendered sentence to match on.
    #[tokio::test]
    async fn a_spec_list_row_reports_a_parse_failure_the_same_way() {
        let source =
            selfie::yaml::parse::<selfie::package::Package>("name: x\nenvironments: {oops\n")
                .expect_err("the fixture must not parse");
        let error = selfie::package::port::PackageParseError::new(
            "/packages/creds.yml",
            selfie::package::port::PackageParseKind::Yaml { source },
        );

        let stream: EventStream = Box::pin(stream::iter(vec![PackageEvent::SpecListLoaded {
            operation_info: test_op_info(),
            spec_list: selfie::package::event::SpecListData {
                specs: vec![],
                invalid_packages: vec![error],
                current_environment: "test".to_string(),
                package_directory: "/packages".to_string(),
                environment_stats: std::collections::HashMap::new(),
                show_all: false,
            },
        }]));
        let result = collect_events(stream).await;

        let row = &result.data["data"][0]["invalid_packages"][0];
        assert_eq!(row["path"], "/packages/creds.yml");
        assert_eq!(row["kind"], "yaml");
        assert_eq!(row["reason"], "unclosed bracket '{'");
        assert_eq!(row["line"], 2);
        assert_eq!(row["column"], 15);
    }

    // The package listing answers the same question about the same directory, so
    // a caller that has to switch tools to find out what selfie could not read
    // has been told the directory holds less than it does.
    #[tokio::test]
    async fn a_package_list_summary_reports_the_spec_it_could_not_read() {
        let source =
            selfie::yaml::parse::<selfie::package::Package>("name: x\nenvironments: {oops\n")
                .expect_err("the fixture must not parse");
        let error = selfie::package::port::PackageParseError::new(
            "/packages/creds.yml",
            selfie::package::port::PackageParseKind::Yaml { source },
        );

        let stream: EventStream = Box::pin(stream::iter(vec![PackageEvent::PackageListLoaded {
            operation_info: test_op_info(),
            package_list: selfie::package::event::PackageListData {
                valid_packages: vec![],
                invalid_packages: vec![error],
                current_environment: "test".to_string(),
                package_directory: "/packages".to_string(),
                environment_stats: std::collections::HashMap::new(),
            },
        }]));
        let result = collect_events(stream).await;

        let summary = &result.data["data"][0];
        assert_eq!(summary["type"], "package_list_summary");
        assert_eq!(summary["package_directory"], "/packages");

        let row = &summary["invalid_packages"][0];
        assert_eq!(row["path"], "/packages/creds.yml");
        assert_eq!(row["kind"], "yaml");
        assert_eq!(row["reason"], "unclosed bracket '{'");
        assert_eq!(row["line"], 2);
        assert_eq!(row["column"], 15);
    }

    // A kind with no location says so, rather than inventing one.
    #[tokio::test]
    async fn a_skipped_spec_with_no_location_reports_null() {
        let error = selfie::package::port::PackageParseError::new(
            "/packages/ghost.yml",
            selfie::package::port::PackageParseKind::IrregularFile {
                kind: "named pipe (fifo)",
            },
        );

        let stream: EventStream = Box::pin(stream::iter(vec![PackageEvent::SpecSkipped {
            operation_info: test_op_info(),
            error,
        }]));
        let result = collect_events(stream).await;

        let row = &result.data["data"][0];
        assert_eq!(row["kind"], "irregular_file");
        assert!(row["line"].is_null(), "got: {row}");
        assert!(row["column"].is_null(), "got: {row}");
    }

    // The other half of the CLI's window: the terminal gets the file's text, this
    // does not. Both halves read the same failure.
    //
    // A failure that arrives on the operation's result carries the location in
    // prose rather than in fields of its own, which is what the positive
    // assertions below pin. Their other job is to prove an event was collected at
    // all -- a scan for absence passes an empty stream.
    #[tokio::test]
    async fn a_parse_failure_reaches_the_json_without_the_file_it_read() {
        let spec = format!(
            "name: creds\ndotfiles:\n  - command: op read op://vault/item/field\n    \
             vars:\n      token: {SECRET}\n    target: ~/.npmrc\nenvironments: {{oops\n"
        );
        let source = selfie::yaml::parse::<selfie::package::Package>(&spec)
            .expect_err("the fixture must not parse");

        let failure = selfie::package::port::PackageError::ParseError {
            name: "creds".to_string(),
            packages_path: std::path::PathBuf::from("/packages"),
            failed_file: std::path::PathBuf::from("/packages/creds.yml"),
            source: selfie::package::port::PackageParseError::new(
                "/packages/creds.yml",
                selfie::package::port::PackageParseKind::Yaml { source },
            ),
        };

        let events = vec![PackageEvent::Completed {
            operation_info: test_op_info(),
            result: OperationResult::Failure(OperationFailure::Package(failure)),
        }];

        let stream: EventStream = Box::pin(stream::iter(events.clone()));
        let result = collect_events(stream).await;

        // Positive control: the failure was collected and says why and where.
        // Without these the scan below passes on an empty stream.
        assert!(!result.success);
        assert_eq!(result.data["result"]["status"], "failure");
        let error = result.data["result"]["error"]
            .as_str()
            .expect("the failure must carry prose");
        assert!(error.contains("YAML parsing error"), "got: {error}");
        assert!(error.contains("at line"), "got: {error}");

        // And the file's own text is in none of it.
        test_common::assert_secret_free(&result.data.to_string(), SECRET, "the collected JSON");
        for event in &events {
            test_common::assert_secret_free(&format!("{event:?}"), SECRET, "an event");
        }
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
