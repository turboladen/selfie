//! Helps break down the pieces of running the `package check` command.

use crate::{
    commands::runner::CommandRunner,
    config::AppConfig,
    package::{
        event::{CheckResult, CheckResultData, EventSender, OperationResult},
        port::{PackageError, PackageRepoError, PackageRepository},
    },
};

pub(super) async fn handle_check<PR, CR>(
    package_name: &str,
    repo: &PR,
    config: &AppConfig,
    command_runner: &CR,
    sender: &EventSender,
    progress: &mut crate::package::service::ProgressTracker,
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

    // Return appropriate operation result
    create_operation_result(&check_result, package_name, progress)
}

async fn load_package<PR>(
    package_name: &str,
    repo: &PR,
    sender: &EventSender,
    progress: &mut crate::package::service::ProgressTracker,
) -> Result<crate::package::GetPackage, OperationResult>
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
        Err(err) => {
            let error_msg = format!("Failed to load package '{package_name}': {err}");
            sender.send_error(err, &error_msg).await;
            Err(OperationResult::Failure(error_msg))
        }
    }
}

async fn get_check_command(
    package_name: &str,
    package_blob: &crate::package::GetPackage,
    current_env: &str,
    sender: &EventSender,
    progress: &mut crate::package::service::ProgressTracker,
) -> Result<Option<String>, OperationResult> {
    progress.next(sender, "Checking package environment").await;

    // Get environment configuration
    let Some(env_config) = package_blob.package.environments().get(current_env) else {
        return handle_missing_environment(package_name, package_blob, current_env, sender).await;
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

async fn handle_missing_environment(
    package_name: &str,
    package_blob: &crate::package::GetPackage,
    current_env: &str,
    sender: &EventSender,
) -> Result<Option<String>, OperationResult> {
    let err = Box::new(PackageError::EnvironmentNotFound {
        package_name: package_name.to_string(),
        environment: current_env.to_string(),
        available_environments: package_blob
            .package
            .environments()
            .keys()
            .cloned()
            .collect(),
        package_file: package_blob.package.path().clone(),
    });
    let error_msg = format!("Environment configuration error: {err}");
    sender
        .send_error(PackageRepoError::PackageError(err), &error_msg)
        .await;
    Err(OperationResult::Failure(error_msg))
}

async fn handle_missing_check_command(
    package_name: &str,
    package_blob: &crate::package::GetPackage,
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

    let err = Box::new(PackageError::NoCheckCommand {
        package_name: package_name.to_string(),
        environment: current_env.to_string(),
        package_file: package_blob.package.path().clone(),
        other_envs_with_check,
    });

    // Send structured result for no check command
    let check_result = CheckResultData {
        package_name: package_name.to_string(),
        environment: current_env.to_string(),
        check_command: None,
        result: CheckResult::NoCheckCommand,
    };
    sender.send_check_result(check_result).await;

    let error_msg = format!("Check command configuration error: {err}");
    sender
        .send_error(PackageRepoError::PackageError(err), &error_msg)
        .await;
    Err(OperationResult::Failure(error_msg))
}

fn create_operation_result(
    check_result: &CheckResultData,
    package_name: &str,
    progress: &crate::package::service::ProgressTracker,
) -> OperationResult {
    match &check_result.result {
        CheckResult::Success => {
            let success_msg = format!(
                "Package '{}' check completed successfully ({}/{} steps)",
                package_name,
                progress.current_step(),
                progress.total_steps()
            );
            OperationResult::Success(success_msg)
        }
        CheckResult::Failed { .. } => {
            let error_msg = format!(
                "Package '{}' check failed at step {}/{}",
                package_name,
                progress.current_step(),
                progress.total_steps()
            );
            OperationResult::Failure(error_msg)
        }
        CheckResult::Error(_) => {
            let error_msg = format!(
                "Failed to execute check command for package '{}' at step {}/{}",
                package_name,
                progress.current_step(),
                progress.total_steps()
            );
            OperationResult::Failure(error_msg)
        }
        _ => {
            // This case is already handled above, but included for completeness
            OperationResult::Failure("Unexpected check result".to_string())
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
    progress: &mut crate::package::service::ProgressTracker,
    step_description: &str,
) -> CheckResultData
where
    CR: CommandRunner,
{
    progress.next(sender, step_description).await;

    execute_check_command_quiet(
        package_name,
        environment,
        check_command,
        command_runner,
        sender,
    )
    .await
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
    _sender: &EventSender,
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
                        result: CheckResult::Success,
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
