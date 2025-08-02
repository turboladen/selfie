//!
//! Helps break down the pieces of running the `package info` command.
//!

use crate::{
    commands::runner::CommandRunner,
    config::AppConfig,
    package::{
        event::{
            EnvironmentStatus, EnvironmentStatusData, EventSender, OperationResult,
            OperationSuccess, PackageInfoData,
        },
        port::PackageRepository,
        service::ProgressTracker,
    },
};

pub(super) async fn handle_info<PR, CR>(
    package_name: &str,
    repo: &PR,
    config: &AppConfig,
    command_runner: &CR,
    sender: &EventSender,
    progress: &mut ProgressTracker,
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
            let error_msg = format!("Failed to load package '{package_name}': {err}");
            let err_for_conversion = err.clone();
            sender.send_error(err, &error_msg).await;
            return OperationResult::Failure(err_for_conversion.into());
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

    // Step 3: Send environment status data
    progress
        .next(sender, "Checking installation status for environments")
        .await;

    // Sort environments to show current environment first
    let mut environments: Vec<_> = package_blob.package.environments().iter().collect();
    environments.sort_by(|a, b| {
        let a_is_current = a.0 == config.environment();
        let b_is_current = b.0 == config.environment();

        match (a_is_current, b_is_current) {
            (true, false) => std::cmp::Ordering::Less, // a comes first
            (false, true) => std::cmp::Ordering::Greater, // b comes first
            _ => a.0.cmp(b.0),                         // alphabetical order
        }
    });

    for (env_name, env_config) in environments {
        let is_current = env_name == config.environment();
        let status = if is_current {
            get_installation_status(env_config, command_runner).await
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

    // Send success message
    sender
        .send_debug(format!("Package information retrieved for: {package_name}"))
        .await;

    OperationResult::Success(OperationSuccess::package_info_retrieved(
        package_name.to_string(),
        config.environment().to_string(),
        (
            progress.current_step() as usize,
            progress.total_steps() as usize,
        ),
    ))
}

async fn get_installation_status(
    env_config: &crate::package::EnvironmentConfig,
    command_runner: &impl CommandRunner,
) -> Option<EnvironmentStatus> {
    // Only run check for current environment
    if let Some(check_cmd) = env_config.check() {
        // Run the check command asynchronously
        if let Ok(output) = command_runner.execute(check_cmd).await {
            if output.is_success() {
                Some(EnvironmentStatus::Installed)
            } else {
                Some(EnvironmentStatus::NotInstalled)
            }
        } else {
            // Error executing check command
            Some(EnvironmentStatus::Unknown("check failed".to_string()))
        }
    } else {
        // No check command available
        Some(EnvironmentStatus::Unknown("no check command".to_string()))
    }
}
