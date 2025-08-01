//!
//! Helps break down the pieces of running the `package install` command.
//!

use crate::{
    commands::runner::CommandRunner,
    config::AppConfig,
    package::{
        event::{CheckResult, CheckResultData, EventSender, OperationResult},
        port::{PackageRepoError, PackageRepository},
    },
};

use super::{check, steps};

pub(super) async fn handle_install<PR, CR>(
    package_name: &str,
    repo: &PR,
    config: &AppConfig,
    command_runner: &CR,
    sender: &EventSender,
    progress: &mut crate::package::service::ProgressTracker,
) -> OperationResult
where
    PR: PackageRepository,
    CR: CommandRunner,
{
    // Step 1: Fetch package (reusing shared step)
    let package_blob = match steps::fetch_package(repo, package_name, sender, progress).await {
        Ok(pkg) => pkg,
        Err(err) => {
            return OperationResult::Failure(format!(
                "Failed to fetch package '{package_name}': {err}"
            ));
        }
    };

    // Step 2: Find environment configuration
    let env_config = match get_environment_config(
        package_name,
        &package_blob,
        config.environment(),
        sender,
        progress,
    )
    .await
    {
        Ok(config) => config,
        Err(result) => return result,
    };

    // Step 3: Check if package is already installed
    let pre_install_check = check::execute_check_command(
        package_name,
        config.environment(),
        env_config.check.as_deref(),
        command_runner,
        sender,
        progress,
        "Checking if package is already installed",
    )
    .await;

    // If package is already installed, exit early
    if let Some(result) =
        handle_already_installed_package(package_name, &pre_install_check, command_runner, sender)
            .await
    {
        return result;
    }

    // Log that we're proceeding with installation
    log_proceeding_with_installation(package_name, &pre_install_check, sender).await;

    // Step 4: Get install command (reusing shared step with custom getter function)
    let install_cmd = match steps::get_command(
        env_config,
        "install",
        |ec| Some(ec.install()),
        sender,
        progress,
    )
    .await
    {
        Ok(cmd) => cmd,
        Err(err) => return OperationResult::Failure(format!("Install command error: {err}")),
    };

    // Step 5: Execute installation and verification
    let context = InstallationContext {
        package_name,
        install_cmd,
        env_config,
        config,
        pre_install_check: &pre_install_check,
    };

    execute_installation_and_verification(context, command_runner, sender, progress).await
}

async fn handle_already_installed_package<CR>(
    package_name: &str,
    pre_install_check: &CheckResultData,
    command_runner: &CR,
    sender: &EventSender,
) -> Option<OperationResult>
where
    CR: CommandRunner,
{
    if matches!(pre_install_check.result, CheckResult::Success) {
        sender
            .send_debug(format!("Package '{package_name}' is already installed"))
            .await;

        let executable_path = find_executable_path(package_name, command_runner, sender).await;

        let message = if let Some(path) = executable_path {
            format!("Package '{package_name}' is already installed at: {path}")
        } else {
            format!("Package '{package_name}' is already installed, skipping installation")
        };

        return Some(OperationResult::Success(message));
    }
    None
}

async fn find_executable_path<CR>(
    package_name: &str,
    command_runner: &CR,
    sender: &EventSender,
) -> Option<String>
where
    CR: CommandRunner,
{
    let finder_command = if cfg!(target_os = "windows") {
        format!("where {package_name}")
    } else {
        format!("which {package_name}")
    };

    match command_runner.execute(&finder_command).await {
        Ok(output) if output.is_success() && !output.stdout_str().trim().is_empty() => {
            let executable_path = output.stdout_str().trim().to_string();
            sender
                .send_debug(format!("Found executable at: {executable_path}"))
                .await;
            Some(executable_path.to_string())
        }
        Ok(_) => {
            sender
                .send_debug(format!(
                    "No executable named '{package_name}' found in PATH"
                ))
                .await;
            None
        }
        Err(_) => {
            let command_name = if cfg!(target_os = "windows") {
                "where"
            } else {
                "which"
            };
            sender
                .send_debug(format!(
                    "Could not run '{command_name}' command to locate executable"
                ))
                .await;
            None
        }
    }
}

async fn log_proceeding_with_installation(
    package_name: &str,
    pre_install_check: &CheckResultData,
    sender: &EventSender,
) {
    match pre_install_check.result {
        CheckResult::Failed { .. } => {
            sender
                .send_debug(format!(
                    "Package '{package_name}' is not installed, proceeding with installation"
                ))
                .await;
        }
        CheckResult::NoCheckCommand => {
            sender
                .send_debug("No check command defined, proceeding with installation")
                .await;
        }
        CheckResult::Error(_) | CheckResult::CommandNotFound => {
            sender
                .send_warning("Check command failed, but proceeding with installation anyway")
                .await;
        }
        CheckResult::Success => {
            // Already handled in handle_already_installed_package
        }
    }
}

struct InstallationContext<'a> {
    package_name: &'a str,
    install_cmd: &'a str,
    env_config: &'a crate::package::EnvironmentConfig,
    config: &'a AppConfig,
    pre_install_check: &'a CheckResultData,
}

async fn execute_installation_and_verification<CR>(
    context: InstallationContext<'_>,
    command_runner: &CR,
    sender: &EventSender,
    progress: &mut crate::package::service::ProgressTracker,
) -> OperationResult
where
    CR: CommandRunner,
{
    // Execute install command with streaming output
    let install_success = match steps::execute_command_streaming(
        command_runner,
        context.install_cmd,
        "install",
        context.config,
        sender,
        progress,
    )
    .await
    {
        Ok(success) => success,
        Err(err) => return OperationResult::Failure(format!("Command execution error: {err}")),
    };

    if !install_success {
        sender
            .send_warning(format!(
                "Package '{}' installation command failed",
                context.package_name
            ))
            .await;
        return OperationResult::Failure(format!(
            "Installation failed at step {}/{}",
            progress.current_step(),
            progress.total_steps()
        ));
    }

    // Verify installation if check command is available
    verify_installation(&context, command_runner, sender, progress).await;

    // Final step: Report success
    progress
        .next(sender, "Package installation completed")
        .await;

    OperationResult::Success(format!(
        "Installation completed successfully ({}/{} steps)",
        progress.current_step(),
        progress.total_steps()
    ))
}

async fn get_environment_config<'a>(
    package_name: &str,
    package_blob: &'a crate::package::GetPackage,
    current_env: &str,
    sender: &EventSender,
    progress: &mut crate::package::service::ProgressTracker,
) -> Result<&'a crate::package::EnvironmentConfig, OperationResult> {
    progress.next(sender, "Checking package environment").await;

    // Get environment configuration
    let Some(env_config) = package_blob.package.environments().get(current_env) else {
        return handle_missing_environment(package_name, package_blob, current_env, sender).await;
    };

    sender
        .send_debug(format!(
            "Found environment configuration for '{current_env}'"
        ))
        .await;
    Ok(env_config)
}

async fn handle_missing_environment(
    package_name: &str,
    package_blob: &crate::package::GetPackage,
    current_env: &str,
    sender: &EventSender,
) -> Result<&'static crate::package::EnvironmentConfig, OperationResult> {
    let err = Box::new(crate::package::port::PackageError::EnvironmentNotFound {
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

async fn verify_installation<CR>(
    context: &InstallationContext<'_>,
    command_runner: &CR,
    sender: &EventSender,
    progress: &mut crate::package::service::ProgressTracker,
) where
    CR: CommandRunner,
{
    if context.pre_install_check.check_command.is_some() {
        let post_install_check = check::execute_check_command(
            context.package_name,
            context.config.environment(),
            context.env_config.check.as_deref(),
            command_runner,
            sender,
            progress,
            "Verifying package installation",
        )
        .await;

        match post_install_check.result {
            CheckResult::Success => {
                sender
                    .send_debug(format!(
                        "Package '{}' installation verified successfully",
                        context.package_name
                    ))
                    .await;
            }
            CheckResult::Failed { .. } => {
                sender
                    .send_warning(format!(
                        "Package '{}' installation verification failed - package may not have installed correctly",
                        context.package_name
                    ))
                    .await;
            }
            CheckResult::Error(_) | CheckResult::CommandNotFound => {
                sender
                    .send_warning(
                        "Post-installation check failed, but installation command completed",
                    )
                    .await;
            }
            CheckResult::NoCheckCommand => {
                sender
                    .send_debug("Unexpected: no check command in post-install verification")
                    .await;
            }
        }
    } else {
        progress
            .next(
                sender,
                "Skipping installation verification (no check command)",
            )
            .await;
    }
}
