//!
//! Helps break down the pieces of running the `package install` command.
//!

use crate::{
    commands::runner::CommandRunner,
    config::SelfieConfig,
    package::{
        EnvironmentConfig,
        event::{
            CheckResult, CheckResultData, EventSender, OperationFailure, OperationResult,
            OperationSuccess,
        },
        port::PackageRepository,
        service::{InstallOptions, ProgressTracker},
    },
};

use tokio_util::sync::CancellationToken;

use super::{check, deps, steps};

#[allow(clippy::too_many_arguments)]
pub(super) async fn handle_install<PR, CR>(
    package_name: &str,
    repo: &PR,
    config: &SelfieConfig,
    command_runner: &CR,
    sender: &EventSender,
    progress: &mut ProgressTracker,
    token: &CancellationToken,
    options: &InstallOptions,
) -> OperationResult
where
    PR: PackageRepository + Sync,
    CR: CommandRunner,
{
    // Step 1: Resolve dependencies (includes cycle detection)
    progress
        .next(sender, "Resolving package dependencies")
        .await;

    let dep_graph =
        match deps::resolve_dependencies(package_name, repo, config.environment(), sender).await {
            Ok(graph) => graph,
            Err(failure) => return OperationResult::Failure(failure),
        };

    // Update total steps: 1 (resolve) + 7 per package (fetch, env, check, get_cmd, execute, verify, complete)
    let num_packages = dep_graph.install_order.len();
    let total_steps = 1 + (7 * num_packages);
    progress.set_total_steps(total_steps);

    // Install each package in dependency order.
    // The last package is always the root (the one the user requested).
    let mut last_result = None;
    for pkg_name in &dep_graph.install_order {
        // Check for cancellation between packages
        if token.is_cancelled() {
            return OperationResult::Failure("Installation cancelled".into());
        }

        let result = install_single_package(
            pkg_name,
            repo,
            config,
            command_runner,
            sender,
            progress,
            token,
        )
        .await;

        match result {
            OperationResult::Success(_) => {
                last_result = Some(result);
            }
            OperationResult::Failure(_) => return result,
        }
    }

    // After the main install succeeds, handle recommends (soft dependencies)
    if !options.skip_recommends {
        install_recommends(package_name, repo, config, command_runner, sender, token).await;
    }

    last_result.unwrap_or_else(|| {
        OperationResult::Success(OperationSuccess::package_installed(
            package_name.to_string(),
            config.environment().to_string(),
            false,
            None,
            (progress.current_step(), progress.total_steps()).into(),
        ))
    })
}

/// Install a single package (without dependency resolution).
async fn install_single_package<PR, CR>(
    package_name: &str,
    repo: &PR,
    config: &SelfieConfig,
    command_runner: &CR,
    sender: &EventSender,
    progress: &mut ProgressTracker,
    token: &CancellationToken,
) -> OperationResult
where
    PR: PackageRepository,
    CR: CommandRunner,
{
    // Fetch package
    let package_blob = match steps::fetch_package(repo, package_name, sender, progress).await {
        Ok(pkg) => pkg,
        Err(err) => {
            return OperationResult::Failure(err.into());
        }
    };

    // Find environment configuration
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

    // Check if package is already installed
    let pre_install_check = check::execute_check_command(
        package_name,
        config.environment(),
        env_config.check.as_deref(),
        command_runner,
        sender,
        progress,
        &format!("Checking if '{package_name}' is already installed"),
        token,
    )
    .await;

    // If package is already installed, exit early
    if let Some(result) = handle_already_installed_package(
        package_name,
        &pre_install_check,
        command_runner,
        sender,
        progress,
        config,
        token,
    )
    .await
    {
        return result;
    }

    // Log that we're proceeding with installation
    log_proceeding_with_installation(package_name, &pre_install_check, sender).await;

    // Get install command
    let Ok(install_cmd) = steps::get_command(
        env_config,
        "install",
        |ec| Some(ec.install()),
        sender,
        progress,
    )
    .await
    else {
        let other_envs_with_install = package_blob
            .package
            .environments()
            .keys()
            .filter_map(|env_name| {
                if package_blob
                    .package
                    .environments()
                    .get(env_name)?
                    .install()
                    .is_empty()
                {
                    None
                } else {
                    Some(env_name.clone())
                }
            })
            .collect();

        return OperationResult::Failure(OperationFailure::no_install_command(
            package_name.to_string(),
            config.environment().to_string(),
            package_blob.package.path().clone(),
            other_envs_with_install,
        ));
    };

    // Execute installation and verification
    let context = InstallationContext {
        package_name,
        install_cmd,
        env_config,
        config,
        pre_install_check: &pre_install_check,
    };

    execute_installation_and_verification(context, command_runner, sender, progress, token).await
}

async fn handle_already_installed_package<CR>(
    package_name: &str,
    pre_install_check: &CheckResultData,
    command_runner: &CR,
    sender: &EventSender,
    progress: &mut ProgressTracker,
    config: &SelfieConfig,
    token: &CancellationToken,
) -> Option<OperationResult>
where
    CR: CommandRunner,
{
    if matches!(pre_install_check.result, CheckResult::Success { .. }) {
        sender
            .send_debug(format!("Package '{package_name}' is already installed"))
            .await;

        // Reduce total steps by 4 (get_cmd, execute, verify, complete) since
        // we're skipping the actual installation for this package.
        progress.reduce_total_steps(4);

        let executable_path =
            find_executable_path(package_name, command_runner, sender, token).await;

        return Some(OperationResult::Success(
            OperationSuccess::package_installed(
                package_name.to_string(),
                config.environment().to_string(),
                true, // was_already_installed
                executable_path,
                (progress.current_step(), progress.total_steps()).into(),
            ),
        ));
    }
    None
}

async fn find_executable_path<CR>(
    package_name: &str,
    command_runner: &CR,
    sender: &EventSender,
    token: &CancellationToken,
) -> Option<String>
where
    CR: CommandRunner,
{
    let finder_command = if cfg!(target_os = "windows") {
        format!("where {package_name}")
    } else {
        format!("which {package_name}")
    };

    match command_runner.execute(&finder_command, token).await {
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
        CheckResult::Success { .. } => {
            // Already handled in handle_already_installed_package
        }
    }
}

struct InstallationContext<'a> {
    package_name: &'a str,
    install_cmd: &'a str,
    env_config: &'a EnvironmentConfig,
    config: &'a SelfieConfig,
    pre_install_check: &'a CheckResultData,
}

async fn execute_installation_and_verification<CR>(
    context: InstallationContext<'_>,
    command_runner: &CR,
    sender: &EventSender,
    progress: &mut ProgressTracker,
    token: &CancellationToken,
) -> OperationResult
where
    CR: CommandRunner,
{
    // Execute install command with streaming output
    let install_output = match steps::execute_command_streaming(
        command_runner,
        context.install_cmd,
        "install",
        context.config,
        sender,
        progress,
        token,
    )
    .await
    {
        Ok(output) => output,
        Err(err) => return OperationResult::Failure(err.into()),
    };

    if !install_output.is_success() {
        sender
            .send_warning(format!(
                "Package '{}' installation command failed",
                context.package_name
            ))
            .await;
        return OperationResult::Failure(OperationFailure::command_failed(
            context.install_cmd.to_string(),
            Some(install_output.exit_code()),
            install_output.stdout_str().to_string(),
            install_output.stderr_str().to_string(),
        ));
    }

    // Verify installation if check command is available
    verify_installation(&context, command_runner, sender, progress, token).await;

    let executable_path =
        find_executable_path(context.package_name, command_runner, sender, token).await;

    // Final step: Report success
    progress
        .next(sender, "Package installation completed")
        .await;

    OperationResult::Success(OperationSuccess::package_installed(
        context.package_name.to_string(),
        context.config.environment().to_string(),
        false, // was_already_installed
        executable_path,
        (progress.current_step(), progress.total_steps()).into(),
    ))
}

async fn get_environment_config<'a>(
    package_name: &str,
    package_blob: &'a crate::package::GetPackage,
    current_env: &str,
    sender: &EventSender,
    progress: &mut ProgressTracker,
) -> Result<&'a EnvironmentConfig, OperationResult> {
    progress.next(sender, "Checking package environment").await;

    // Get environment configuration
    let Some(env_config) = package_blob.package.environments().get(current_env) else {
        return steps::handle_missing_environment(package_name, package_blob, current_env);
    };

    sender
        .send_debug(format!(
            "Found environment configuration for '{current_env}'"
        ))
        .await;
    Ok(env_config)
}

async fn verify_installation<CR>(
    context: &InstallationContext<'_>,
    command_runner: &CR,
    sender: &EventSender,
    progress: &mut ProgressTracker,
    token: &CancellationToken,
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
            token,
        )
        .await;

        match post_install_check.result {
            CheckResult::Success { .. } => {
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

/// Install recommended (soft) dependencies for a package.
///
/// Recommends are one-level deep only — we do NOT follow recommends of recommends.
/// Each recommend's hard dependencies ARE resolved and installed.
/// Failures are emitted as `RecommendFailed` events but never propagate to the parent result.
async fn install_recommends<PR, CR>(
    package_name: &str,
    repo: &PR,
    config: &SelfieConfig,
    command_runner: &CR,
    sender: &EventSender,
    token: &CancellationToken,
) where
    PR: PackageRepository + Sync,
    CR: CommandRunner,
{
    // Load the root package to read its recommends for the current environment
    let package_blob = match repo.get_package(package_name) {
        Ok(pkg) => pkg,
        Err(_) => return, // Package was already installed; if we can't reload it, skip recommends
    };

    let recommends: Vec<String> = package_blob
        .package
        .environments()
        .get(config.environment())
        .map(|env| env.recommends().to_vec())
        .unwrap_or_default();

    if recommends.is_empty() {
        return;
    }

    sender
        .send_debug(format!(
            "Package '{package_name}' recommends: {recommends:?}"
        ))
        .await;

    // Recommends use a separate progress tracker so they don't overflow
    // the parent operation's step count. Progress for recommends is communicated
    // via RecommendStarted/Succeeded/Failed events instead.
    let mut rec_progress = ProgressTracker::new(0);

    for recommend_name in &recommends {
        if token.is_cancelled() {
            break;
        }

        sender.send_recommend_started(recommend_name).await;

        match install_single_recommend(
            recommend_name,
            repo,
            config,
            command_runner,
            sender,
            &mut rec_progress,
            token,
        )
        .await
        {
            Ok(()) => {
                sender.send_recommend_succeeded(recommend_name).await;
            }
            Err(error) => {
                sender.send_recommend_failed(recommend_name, &error).await;
            }
        }
    }
}

/// Try to install a single recommended package (with its hard dependencies).
///
/// Returns `Ok(())` on success, or `Err(message)` describing the failure.
async fn install_single_recommend<PR, CR>(
    recommend_name: &str,
    repo: &PR,
    config: &SelfieConfig,
    command_runner: &CR,
    sender: &EventSender,
    progress: &mut ProgressTracker,
    token: &CancellationToken,
) -> Result<(), String>
where
    PR: PackageRepository + Sync,
    CR: CommandRunner,
{
    // Resolve hard dependencies for this recommend
    let dep_graph = deps::resolve_dependencies(recommend_name, repo, config.environment(), sender)
        .await
        .map_err(|f| f.to_string())?;

    // Install each package in dependency order
    for pkg_name in &dep_graph.install_order {
        if token.is_cancelled() {
            return Err("cancelled".to_string());
        }

        let result = install_single_package(
            pkg_name,
            repo,
            config,
            command_runner,
            sender,
            progress,
            token,
        )
        .await;

        if let OperationResult::Failure(failure) = result {
            return Err(failure.to_string());
        }
    }

    Ok(())
}
