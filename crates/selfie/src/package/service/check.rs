//! Helps break down the pieces of running the `package check` command.

use super::steps;
use crate::{
    commands::runner::CommandRunner,
    config::AppConfig,
    package::{
        GetPackage,
        event::{
            CheckResult, CheckResultData, CommandFailure, EventSender, OperationFailure,
            OperationResult, OperationSuccess,
        },
        port::{PackageError, PackageRepository},
        service::ProgressTracker,
    },
};

pub(super) async fn handle_check<PR, CR>(
    package_name: &str,
    repo: &PR,
    config: &AppConfig,
    command_runner: &CR,
    sender: &EventSender,
    progress: &mut ProgressTracker,
) -> OperationResult
where
    PR: PackageRepository + Clone,
    CR: CommandRunner + Clone,
{
    // Step 1: Load package from repository
    let package_blob = match load_package(package_name, repo, sender, progress).await {
        Ok(pkg) => pkg,
        Err(result) => return result,
    };

    // Step 2: Get environment-specific check command
    let check_command = match get_check_command(
        package_name,
        &package_blob,
        config.environment(),
        sender,
        progress,
    )
    .await
    {
        Ok(cmd) => cmd,
        Err(result) => return result,
    };

    // Step 3: Execute the check command
    let check_result = execute_check_command(
        package_name,
        config.environment(),
        check_command.as_deref(),
        command_runner,
        sender,
        progress,
        "Running package check command",
    )
    .await;

    // Step 4: Send the check result event
    sender.send_check_result(check_result.clone()).await;

    // Return appropriate operation result
    create_operation_result(&check_result, package_name, progress)
}

async fn load_package<PR>(
    package_name: &str,
    repo: &PR,
    sender: &EventSender,
    progress: &mut ProgressTracker,
) -> Result<GetPackage, OperationResult>
where
    PR: PackageRepository,
{
    progress.next(sender, "Loading package definition").await;

    match repo.get_package(package_name) {
        Ok(pkg) => {
            sender
                .send_debug(format!("Successfully loaded package: {package_name}"))
                .await;
            Ok(pkg)
        }
        Err(err) => Err(OperationResult::Failure(err.into())),
    }
}

async fn get_check_command(
    package_name: &str,
    package_blob: &GetPackage,
    current_env: &str,
    sender: &EventSender,
    progress: &mut ProgressTracker,
) -> Result<Option<String>, OperationResult> {
    progress.next(sender, "Checking package environment").await;

    // Get environment configuration
    let Some(env_config) = package_blob.package.environments().get(current_env) else {
        return steps::handle_missing_environment(package_name, package_blob, current_env);
    };

    // Get check command from environment
    match env_config.check.as_ref() {
        Some(check_cmd) => {
            sender
                .send_debug(format!(
                    "Found check command for environment '{current_env}': {check_cmd}"
                ))
                .await;
            Ok(Some(check_cmd.clone()))
        }
        None => handle_missing_check_command(package_name, package_blob, current_env, sender).await,
    }
}

async fn handle_missing_check_command(
    package_name: &str,
    package_blob: &GetPackage,
    current_env: &str,
    sender: &EventSender,
) -> Result<Option<String>, OperationResult> {
    // Find other environments that do have check commands
    let other_envs_with_check: Vec<String> = package_blob
        .package
        .environments()
        .iter()
        .filter_map(|(env_name, env_config)| {
            if env_config.check.is_some() {
                Some(env_name.clone())
            } else {
                None
            }
        })
        .collect();

    let err = PackageError::NoCheckCommand {
        package_name: package_name.to_string(),
        environment: current_env.to_string(),
        package_file: package_blob.package.path().clone(),
        other_envs_with_check,
    };

    // Send structured result for no check command
    let check_result = CheckResultData {
        package_name: package_name.to_string(),
        environment: current_env.to_string(),
        check_command: None,
        result: CheckResult::NoCheckCommand,
    };
    sender.send_check_result(check_result).await;

    Err(OperationResult::Failure(err.into()))
}

fn create_operation_result(
    check_result: &CheckResultData,
    package_name: &str,
    progress: &ProgressTracker,
) -> OperationResult {
    match &check_result.result {
        CheckResult::Success { .. } => OperationResult::Success(OperationSuccess::package_checked(
            package_name.to_string(),
            check_result.environment.clone(),
            check_result.result.clone(),
            (progress.current_step(), progress.total_steps()).into(),
        )),
        CheckResult::Failed {
            stdout,
            stderr,
            exit_code,
        } => {
            let command = check_result
                .check_command
                .as_deref()
                .unwrap_or("unknown command");
            OperationResult::Failure(OperationFailure::command_failed(
                command.to_string(),
                *exit_code,
                stdout.clone(),
                stderr.clone(),
            ))
        }
        CheckResult::Error(error) => {
            let command = check_result
                .check_command
                .as_deref()
                .unwrap_or("unknown command");
            OperationResult::Failure(OperationFailure::CommandError(
                CommandFailure::InvalidCommand {
                    command: command.to_string(),
                    reason: error.clone(),
                },
            ))
        }
        _ => {
            // This case is already handled above, but included for completeness
            OperationResult::Failure("Unexpected check result".into())
        }
    }
}

/// Execute a check command and return structured results
///
/// This function can be reused by other services that need to run check commands
/// without duplicating the package loading and environment validation logic.
pub(super) async fn execute_check_command<CR>(
    package_name: &str,
    environment: &str,
    check_command: Option<&str>,
    command_runner: &CR,
    sender: &EventSender,
    progress: &mut ProgressTracker,
    step_description: &str,
) -> CheckResultData
where
    CR: CommandRunner,
{
    progress.next(sender, step_description).await;

    execute_check_command_quiet(package_name, environment, check_command, command_runner).await
}

/// Execute a check command without updating progress
///
/// This is useful for bulk operations like package listing where individual
/// check progress updates would be too noisy.
pub(super) async fn execute_check_command_quiet<CR>(
    package_name: &str,
    environment: &str,
    check_command: Option<&str>,
    command_runner: &CR,
) -> CheckResultData
where
    CR: CommandRunner,
{
    if let Some(cmd) = check_command {
        match command_runner.execute(cmd).await {
            Ok(output) => {
                if output.is_success() {
                    CheckResultData {
                        package_name: package_name.to_string(),
                        environment: environment.to_string(),
                        check_command: Some(cmd.to_string()),
                        result: CheckResult::Success {
                            stdout: output.stdout_str().to_string(),
                            stderr: output.stderr_str().to_string(),
                        },
                    }
                } else {
                    CheckResultData {
                        package_name: package_name.to_string(),
                        environment: environment.to_string(),
                        check_command: Some(cmd.to_string()),
                        result: CheckResult::Failed {
                            stdout: output.stdout_str().to_string(),
                            stderr: output.stderr_str().to_string(),
                            exit_code: Some(output.exit_code()),
                        },
                    }
                }
            }
            Err(err) => CheckResultData {
                package_name: package_name.to_string(),
                environment: environment.to_string(),
                check_command: Some(cmd.to_string()),
                result: CheckResult::Error(err.to_string()),
            },
        }
    } else {
        CheckResultData {
            package_name: package_name.to_string(),
            environment: environment.to_string(),
            check_command: None,
            result: CheckResult::NoCheckCommand,
        }
    }
}
