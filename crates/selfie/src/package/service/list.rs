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
    CR: CommandRunner + Clone + 'static,
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

    // Step 3: Sort packages alphabetically first for streaming
    progress
        .next(sender, "Sorting packages for streaming")
        .await;

    // Sort packages alphabetically by name before processing
    let mut sorted_packages: Vec<_> = valid_packages.into_iter().collect();
    sorted_packages.sort_by(|a, b| a.name().cmp(b.name()));

    // Calculate environment statistics from all valid packages (before filtering)
    let mut environment_stats: HashMap<String, usize> = HashMap::new();
    for package in &sorted_packages {
        for env_name in package.environments().keys() {
            *environment_stats.entry(env_name.clone()).or_insert(0) += 1;
        }
    }

    // Filter packages based on show_all flag
    let packages_to_process: Vec<_> = sorted_packages
        .into_iter()
        .filter(|package| show_all || package.environments().contains_key(config.environment()))
        .collect();

    progress
        .next(sender, "Checking package status in parallel")
        .await;

    // Create parallel tasks for status checking with order preservation
    let check_futures: Vec<_> = packages_to_process
        .iter()
        .enumerate()
        .map(|(index, package)| {
            let package_name = package.name().to_string();
            let package_version = package.version().to_string();
            let package_environments: Vec<String> =
                package.environments().keys().cloned().collect();
            let current_env = config.environment().to_string();

            // Determine status based on environment support and check command
            let env_config = package.environments().get(config.environment());
            let check_command = env_config
                .and_then(|ec| ec.check.as_ref())
                .cloned();
            let supports_current_env = env_config.is_some();

            let command_runner = command_runner.clone();
            let sender = sender.clone();

            tokio::spawn(async move {
                let status = if let Some(ref cmd) = check_command {
                    let check_result = super::check::execute_check_command_quiet(
                        &package_name,
                        &current_env,
                        Some(cmd.as_str()),
                        &command_runner,
                    )
                    .await;
                    Some(check_result.result)
                } else if supports_current_env {
                    Some(crate::package::event::CheckResult::NoCheckCommand)
                } else {
                    None
                };

                // Create the package list item
                let package_item = PackageListItem {
                    name: package_name,
                    version: package_version,
                    environments: package_environments,
                    status,
                };

                // Stream the individual result immediately
                sender.send_package_list_item(package_item.clone()).await;

                (index, package_item)
            })
        })
        .collect();

    // Wait for all tasks and collect results in original order
    let mut results: Vec<(usize, PackageListItem)> = Vec::new();
    for handle in check_futures {
        if let Ok(result) = handle.await {
            results.push(result);
        }
    }

    // Sort by original index to maintain alphabetical order for final summary
    results.sort_by_key(|(index, _)| *index);
    let valid_package_items: Vec<PackageListItem> =
        results.into_iter().map(|(_, item)| item).collect();

    let invalid_package_items: Vec<InvalidPackageInfo> = invalid_packages
        .iter()
        .map(|invalid_package| InvalidPackageInfo {
            path: invalid_package.package_path().display().to_string(),
            error: invalid_package.to_string(),
        })
        .collect();

    // Calculate the count before moving the vector
    let valid_count = valid_package_items.len();

    let package_list_data = PackageListData {
        valid_packages: valid_package_items,
        invalid_packages: invalid_package_items,
        current_environment: config.environment().to_string(),
        package_directory: config.package_directory().display().to_string(),
        environment_stats,
    };

    // Send final summary data event (CLI can use this for invalid packages and stats)
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
        (progress.current_step(), progress.total_steps()).into(),
    ))
}
