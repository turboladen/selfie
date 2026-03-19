//!
//! Helpers for spec info and package status operations.
//!
//! `handle_spec_info` — loads the package definition and emits `PackageInfoLoaded` (no runtime
//! commands).
//!
//! `handle_status` — loads the package and checks installation status for the current environment.
//!

use tokio_util::sync::CancellationToken;

use crate::{
    commands::runner::CommandRunner,
    config::SelfieConfig,
    package::{
        event::{
            EnvironmentStatus, EnvironmentStatusData, EventSender, OperationResult,
            OperationSuccess, PackageInfoData,
        },
        port::PackageRepository,
        service::ProgressTracker,
    },
};

/// Spec-only info: load package definition and emit `PackageInfoLoaded`.
/// Does NOT execute any commands or check installation status.
pub(super) async fn handle_spec_info<PR>(
    package_name: &str,
    repo: &PR,
    config: &SelfieConfig,
    sender: &EventSender,
    progress: &mut ProgressTracker,
) -> OperationResult
where
    PR: PackageRepository,
{
    // Step 1: Fetch package
    progress.next(sender, "Loading package definition").await;

    let package_blob = match repo.get_package(package_name) {
        Ok(pkg) => {
            sender
                .send_debug(format!("Successfully loaded package: {package_name}"))
                .await;
            pkg
        }
        Err(err) => {
            return OperationResult::Failure(err.into());
        }
    };

    // Step 2: Send package information data
    progress.next(sender, "Gathering package information").await;

    let package_info = PackageInfoData {
        name: package_blob.package.name().to_string(),
        version: package_blob.package.version().to_string(),
        description: package_blob
            .package
            .description()
            .map(std::string::ToString::to_string),
        homepage: package_blob
            .package
            .homepage()
            .map(std::string::ToString::to_string),
        environments: package_blob
            .package
            .environments()
            .keys()
            .cloned()
            .collect(),
        current_environment: config.environment().to_string(),
    };

    sender.send_package_info(package_info).await;

    sender
        .send_debug(format!("Spec info retrieved for: {package_name}"))
        .await;

    OperationResult::Success(OperationSuccess::spec_info_retrieved(
        package_name.to_string(),
        config.environment().to_string(),
        (progress.current_step(), progress.total_steps()).into(),
    ))
}

/// Runtime status: load package and check installation status for the current environment.
pub(super) async fn handle_status<PR, CR>(
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
    // Step 1: Fetch package
    progress.next(sender, "Loading package definition").await;

    let package_blob = match repo.get_package(package_name) {
        Ok(pkg) => {
            sender
                .send_debug(format!("Successfully loaded package: {package_name}"))
                .await;
            pkg
        }
        Err(err) => {
            return OperationResult::Failure(err.into());
        }
    };

    // Step 2: Check installation status for environments
    progress
        .next(sender, "Checking installation status for environments")
        .await;

    // Sort environments to show current environment first
    let mut environments: Vec<_> = package_blob.package.environments().iter().collect();
    environments.sort_by(|a, b| {
        let a_is_current = a.0 == config.environment();
        let b_is_current = b.0 == config.environment();

        match (a_is_current, b_is_current) {
            (true, false) => std::cmp::Ordering::Less,
            (false, true) => std::cmp::Ordering::Greater,
            _ => a.0.cmp(b.0),
        }
    });

    for (env_name, env_config) in environments {
        let is_current = env_name == config.environment();
        let status = if is_current {
            get_installation_status(env_config, command_runner, token).await
        } else {
            None
        };

        let environment_status = EnvironmentStatusData {
            environment_name: env_name.clone(),
            is_current,
            install_command: env_config.install().to_string(),
            check_command: env_config.check().map(std::string::ToString::to_string),
            dependencies: env_config.dependencies().to_vec(),
            status,
        };

        sender.send_environment_status(environment_status).await;
    }

    sender
        .send_debug(format!("Package status checked for: {package_name}"))
        .await;

    OperationResult::Success(OperationSuccess::package_status_checked(
        package_name.to_string(),
        config.environment().to_string(),
        (progress.current_step(), progress.total_steps()).into(),
    ))
}

async fn get_installation_status(
    env_config: &crate::package::EnvironmentConfig,
    command_runner: &impl CommandRunner,
    token: &CancellationToken,
) -> Option<EnvironmentStatus> {
    if let Some(check_cmd) = env_config.check() {
        if let Ok(output) = command_runner.execute(check_cmd, token).await {
            if output.is_success() {
                Some(EnvironmentStatus::Installed)
            } else {
                Some(EnvironmentStatus::NotInstalled)
            }
        } else {
            Some(EnvironmentStatus::Unknown("check failed".to_string()))
        }
    } else {
        Some(EnvironmentStatus::Unknown("no check command".to_string()))
    }
}
