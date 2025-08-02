//!
//! Helps break down the pieces of running the `package list` command.
//!

use std::collections::HashMap;

use crate::{
    commands::runner::CommandRunner,
    config::AppConfig,
    package::{
        event::{
            EventSender, InvalidPackageInfo, OperationResult, OperationSuccess, PackageListData,
            PackageListItem,
        },
        port::{PackageRepoError, PackageRepository},
        service::ProgressTracker,
    },
};

pub(super) async fn handle_list<PR, CR>(
    repo: &PR,
    config: &AppConfig,
    command_runner: &CR,
    sender: &EventSender,
    progress: &mut ProgressTracker,
    show_all: bool,
) -> OperationResult
where
    PR: PackageRepository,
    CR: CommandRunner,
{
    // Step 1: List all packages
    progress.next(sender, "Loading package list").await;

    let list_output = match repo.list_packages() {
        Ok(output) => {
            sender.send_debug("Successfully loaded package list").await;
            output
        }
        Err(err) => {
            let error_msg = format!("Failed to list packages: {err}");
            let repo_error = PackageRepoError::PackageListError(err);
            sender.send_error(repo_error, &error_msg).await;
            return OperationResult::Failure(error_msg.into());
        }
    };

    // Step 2: Process valid packages
    progress
        .next(sender, "Processing package information")
        .await;

    let valid_packages: Vec<_> = list_output.valid_packages().collect();
    let invalid_packages: Vec<_> = list_output.invalid_packages().collect();

    // Step 3: Check installation status for each package
    progress
        .next(sender, "Checking package installation status")
        .await;

    // Convert to structured data with check results
    let mut valid_package_items: Vec<PackageListItem> = Vec::new();

    for package in &valid_packages {
        // Check if package supports the current environment
        let status = if let Some(env_config) = package.environments().get(config.environment()) {
            // Package supports current environment - check for installation
            let check_command = env_config.check.as_ref();

            let check_result = super::check::execute_check_command_quiet(
                package.name(),
                config.environment(),
                check_command.map(std::string::String::as_str),
                command_runner,
                sender,
            )
            .await;

            Some(check_result.result)
        } else {
            // Package doesn't support current environment - mark as not relevant
            None
        };

        // If show_all is false, only include packages relevant to current environment
        if show_all || package.environments().contains_key(config.environment()) {
            valid_package_items.push(PackageListItem {
                name: package.name().to_string(),
                version: package.version().to_string(),
                environments: package.environments().keys().cloned().collect(),
                status,
            });
        }
    }

    // Sort packages alphabetically by name
    valid_package_items.sort_by(|a, b| a.name.cmp(&b.name));

    let invalid_package_items: Vec<InvalidPackageInfo> = invalid_packages
        .iter()
        .map(|invalid_package| InvalidPackageInfo {
            path: invalid_package.package_path().display().to_string(),
            error: invalid_package.to_string(),
        })
        .collect();

    // Calculate environment statistics from all packages before filtering
    let mut environment_stats: HashMap<String, usize> = HashMap::new();
    for package in &valid_packages {
        for env_name in package.environments().keys() {
            *environment_stats.entry(env_name.clone()).or_insert(0) += 1;
        }
    }

    // Calculate the count before moving the vector
    let valid_count = valid_package_items.len();

    let package_list_data = PackageListData {
        valid_packages: valid_package_items,
        invalid_packages: invalid_package_items,
        current_environment: config.environment().to_string(),
        package_directory: config.package_directory().display().to_string(),
        environment_stats,
    };

    // Send structured data event
    sender.send_package_list(package_list_data).await;

    // Step 4: Complete operation
    progress.next(sender, "Finalizing package list").await;

    sender
        .send_debug("Package listing completed successfully")
        .await;

    OperationResult::Success(OperationSuccess::package_list_generated(
        valid_count,
        invalid_packages.len(),
        config.environment().to_string(),
        (
            progress.current_step() as usize,
            progress.total_steps() as usize,
        ),
    ))
}
