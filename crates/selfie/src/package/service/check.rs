//! Helps break down the pieces of running the `package check` command.

use super::steps;
use crate::{
    commands::runner::{CommandError, CommandRunner},
    config::SelfieConfig,
    package::{
        GetPackage,
        event::{
            CheckResult, CheckResultData, CheckVerdict, CommandFailure, EventSender,
            OperationFailure, OperationResult, OperationSuccess, StepEnding,
        },
        port::{PackageError, PackageRepository},
        service::ProgressTracker,
    },
};
use std::path::Path;
use tokio_util::sync::CancellationToken;

pub(super) async fn handle_check<PR, CR>(
    package_name: &str,
    repo: &PR,
    config: &SelfieConfig,
    command_runner: &CR,
    sender: &EventSender,
    progress: &mut ProgressTracker,
    token: &CancellationToken,
) -> OperationResult
where
    PR: PackageRepository + Clone,
    CR: CommandRunner + Clone,
{
    // Step 1: Load package from repository
    let package_blob = match load_package(package_name, repo, sender, progress).await {
        Ok(pkg) => pkg,
        Err(result) => return *result,
    };

    if let Some(refusal) =
        steps::refuse_unreadable_spec(package_name, &package_blob, config.environment())
    {
        return refusal;
    }

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
        Err(result) => return *result,
    };

    // Step 3: Execute the check command. A check that could not start is the
    // directory's, the shell's or the cancellation's failure, not the check
    // command's, so it is reported as such rather than as an invalid command.
    let check_result = match execute_check_command(
        package_name,
        config.environment(),
        check_command.as_deref(),
        config.package_directory(),
        command_runner,
        sender,
        progress,
        &format!("Running the check command for {package_name}"),
        token,
    )
    .await
    {
        Ok(check_result) => check_result,
        Err(err) => return OperationResult::Failure(err.into()),
    };

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
) -> Result<GetPackage, Box<OperationResult>>
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
        Err(err) => Err(Box::new(OperationResult::Failure(err.into()))),
    }
}

async fn get_check_command(
    package_name: &str,
    package_blob: &GetPackage,
    current_env: &str,
    sender: &EventSender,
    progress: &mut ProgressTracker,
) -> Result<Option<String>, Box<OperationResult>> {
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
) -> Result<Option<String>, Box<OperationResult>> {
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

    Err(Box::new(OperationResult::Failure(err.into())))
}

fn create_operation_result(
    check_result: &CheckResultData,
    package_name: &str,
    progress: &ProgressTracker,
) -> OperationResult {
    let checked = |verdict| {
        OperationResult::Success(OperationSuccess::package_checked(
            package_name.to_string(),
            check_result.environment.clone(),
            verdict,
            (progress.current_step(), progress.total_steps()).into(),
        ))
    };
    match &check_result.result {
        CheckResult::Success { .. } => checked(CheckVerdict::Installed),
        // A check that ran and exited non-zero answered the question: the
        // package is not installed. `stdout` stays in the `CheckResult` this was
        // built from, which `selfie package check` displays deliberately, and is
        // not copied into the completion, which reaches every adapter.
        CheckResult::Failed {
            stderr, exit_code, ..
        } => checked(CheckVerdict::NotInstalled {
            command: check_result
                .check_command
                .as_deref()
                .unwrap_or("unknown command")
                .to_string(),
            exit_code: *exit_code,
            stderr: crate::commands::BoundedText::bound(stderr.as_bytes()),
        }),
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

/// Execute a check command in `package_dir`, advancing progress, and return
/// structured results.
///
/// A failure that stops every command in `package_dir` is returned as the error
/// instead of recorded as the check's result: the directory cannot be entered,
/// the shell cannot start, or the operation was cancelled.
///
/// # Errors
///
/// The [`CommandError`] that kept the check from starting, when no later command
/// in `package_dir` could start either.
#[allow(clippy::too_many_arguments)]
pub(super) async fn execute_check_command<CR>(
    package_name: &str,
    environment: &str,
    check_command: Option<&str>,
    package_dir: &Path,
    command_runner: &CR,
    sender: &EventSender,
    progress: &mut ProgressTracker,
    step_description: &str,
    token: &CancellationToken,
) -> Result<CheckResultData, CommandError>
where
    CR: CommandRunner,
{
    // Waiting only when a command will actually run.
    let step = if check_command.is_some() {
        Some(progress.next_waiting(sender, step_description).await)
    } else {
        progress.next(sender, step_description).await;
        None
    };

    let result = run_check(
        package_name,
        environment,
        check_command,
        package_dir,
        command_runner,
        token,
    )
    .await;
    if let Some(step) = step {
        let ending = match &result {
            Ok(data) => check_ending(&data.result, token),
            Err(CommandError::Cancelled { .. }) => StepEnding::Cancelled,
            Err(_) => StepEnding::Failed,
        };
        sender.send_step_ended(step, ending).await;
    }
    result
}

/// How a waiting step that ran a check command ended. Any exit status is the
/// check's answer; only a command that could not give one failed.
pub(super) fn check_ending(result: &CheckResult, token: &CancellationToken) -> StepEnding {
    match result {
        CheckResult::Error(_) if token.is_cancelled() => StepEnding::Cancelled,
        CheckResult::Error(_) | CheckResult::CommandNotFound => StepEnding::Failed,
        _ => StepEnding::Succeeded,
    }
}

/// Execute a check command in `package_dir` without updating progress
///
/// This is useful for bulk operations like package listing where individual
/// check progress updates would be too noisy.
pub(super) async fn execute_check_command_quiet<CR>(
    package_name: &str,
    environment: &str,
    check_command: Option<&str>,
    package_dir: &Path,
    command_runner: &CR,
    token: &CancellationToken,
) -> CheckResultData
where
    CR: CommandRunner,
{
    run_check(
        package_name,
        environment,
        check_command,
        package_dir,
        command_runner,
        token,
    )
    .await
    .unwrap_or_else(|err| could_not_run(package_name, environment, check_command, &err))
}

/// The result of a check that could not run, recording why.
fn could_not_run(
    package_name: &str,
    environment: &str,
    check_command: Option<&str>,
    err: &CommandError,
) -> CheckResultData {
    CheckResultData {
        package_name: package_name.to_string(),
        environment: environment.to_string(),
        check_command: check_command.map(str::to_string),
        result: CheckResult::Error(err.to_string()),
    }
}

/// Run a check, returning as an error only a failure that stops every command
/// in `package_dir`, cancellation included, and recording any other as the
/// check's result.
async fn run_check<CR>(
    package_name: &str,
    environment: &str,
    check_command: Option<&str>,
    package_dir: &Path,
    command_runner: &CR,
    token: &CancellationToken,
) -> Result<CheckResultData, CommandError>
where
    CR: CommandRunner,
{
    let Some(cmd) = check_command else {
        return Ok(CheckResultData {
            package_name: package_name.to_string(),
            environment: environment.to_string(),
            check_command: None,
            result: CheckResult::NoCheckCommand,
        });
    };

    // Any exit status is an answer, whatever produced it: the user's shell
    // reports a check killed by a signal as an ordinary non-zero status, so a
    // kill cannot be told from an exit.
    let result = match command_runner.execute(cmd, package_dir, token).await {
        Ok(output) if output.is_success() => CheckResult::Success {
            stdout: output.stdout_str().to_string(),
            stderr: output.stderr_str().to_string(),
        },
        Ok(output) => CheckResult::Failed {
            stdout: output.stdout_str().to_string(),
            stderr: output.stderr_str().to_string(),
            exit_code: Some(output.exit_code()),
        },
        Err(
            err @ (CommandError::WorkingDirectoryUnusable { .. }
            | CommandError::SpawnFailed { .. }
            | CommandError::Cancelled { .. }),
        ) => return Err(err),
        Err(err) => CheckResult::Error(err.to_string()),
    };

    Ok(CheckResultData {
        package_name: package_name.to_string(),
        environment: environment.to_string(),
        check_command: Some(cmd.to_string()),
        result,
    })
}
