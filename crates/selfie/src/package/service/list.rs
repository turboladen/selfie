//!
//! Helps break down the pieces of running the `package list` command.
//!

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

use crate::{
    commands::runner::CommandRunner,
    config::SelfieConfig,
    package::{
        event::{
            EventSender, InvalidPackageInfo, OperationResult, OperationSuccess, PackageListData,
            PackageListItem,
        },
        port::PackageRepository,
        service::ProgressTracker,
    },
};

/// Lightweight snapshot of package identity for the JoinError fallback path.
/// Captured before spawning tasks so we still have metadata if a task panics.
struct PackageMetadata {
    name: String,
    version: String,
    environments: Vec<String>,
}

pub(super) async fn handle_list<PR, CR>(
    repo: &PR,
    config: &SelfieConfig,
    command_runner: &CR,
    sender: &EventSender,
    progress: &mut ProgressTracker,
    show_all: bool,
    token: &CancellationToken,
) -> OperationResult
where
    PR: PackageRepository,
    CR: CommandRunner + Clone + 'static,
{
    // Step 1: Load, process, sort, filter packages and emit ready event
    progress.next(sender, "Loading packages").await;

    let list_output = match repo.list_packages() {
        Ok(output) => {
            sender.send_debug("Successfully loaded package list").await;
            output
        }
        Err(err) => {
            return OperationResult::Failure(err.into());
        }
    };

    let valid_packages: Vec<_> = list_output.valid_packages().collect();
    let invalid_packages: Vec<_> = list_output.invalid_packages().collect();

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

    // Emit PackageListReady with all packages (status: None) before checking
    let ready_items: Vec<PackageListItem> = packages_to_process
        .iter()
        .map(|package| PackageListItem {
            name: package.name().to_string(),
            version: package.version().to_string(),
            environments: package.environments().keys().cloned().collect(),
            status: None,
        })
        .collect();
    sender.send_package_list_ready(ready_items).await;

    // Step 2: Check package status in parallel
    progress.next(sender, "Checking package status").await;

    // Limit concurrent subprocess spawns to avoid exhausting file descriptors.
    let semaphore = Arc::new(Semaphore::new(config.max_parallel_installations().get()));

    // Create parallel tasks for status checking with order preservation
    // Collect package metadata for JoinError handling (values move into spawned tasks)
    let package_metadata: Vec<PackageMetadata> = packages_to_process
        .iter()
        .map(|p| PackageMetadata {
            name: p.name().to_string(),
            version: p.version().to_string(),
            environments: p.environments().keys().cloned().collect(),
        })
        .collect();

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
            let check_command = env_config.and_then(|ec| ec.check.as_ref()).cloned();
            let supports_current_env = env_config.is_some();

            let command_runner = command_runner.clone();
            let sender = sender.clone();
            let token = token.clone();
            let semaphore = semaphore.clone();

            tokio::spawn(async move {
                let status = if let Some(ref cmd) = check_command {
                    // Acquire permit before spawning a subprocess
                    let _permit = semaphore
                        .acquire()
                        .await
                        .expect("semaphore is never closed");
                    let check_result = super::check::execute_check_command_quiet(
                        &package_name,
                        &current_env,
                        Some(cmd.as_str()),
                        &command_runner,
                        &token,
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
    for (i, handle) in check_futures.into_iter().enumerate() {
        match handle.await {
            Ok(result) => results.push(result),
            Err(e) => {
                // Use the captured metadata so the CLI can resolve the correct spinner.
                // Fallback to defaults if index is somehow out of bounds — this is
                // error-recovery code and must not panic or mask the original failure.
                let (name, version, environments) = match package_metadata.get(i) {
                    Some(meta) => (
                        meta.name.clone(),
                        meta.version.clone(),
                        meta.environments.clone(),
                    ),
                    None => {
                        tracing::warn!(
                            index = i,
                            "package_metadata index out of bounds in JoinError handler"
                        );
                        (String::new(), String::new(), Vec::new())
                    }
                };
                sender
                    .send_package_list_item(PackageListItem {
                        name,
                        version,
                        environments,
                        status: Some(crate::package::event::CheckResult::Error(format!(
                            "Task failed: {e}"
                        ))),
                    })
                    .await;
            }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        commands::runner::{CommandError, CommandOutput, CommandRunner},
        config::SelfieConfigBuilder,
        package::{PackageBuilder, port::MockPackageRepository},
    };
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::sync::mpsc;

    /// A cloneable command runner that tracks peak concurrency.
    #[derive(Clone)]
    struct ConcurrencyTrackingRunner {
        current: Arc<AtomicUsize>,
        peak: Arc<AtomicUsize>,
        delay: std::time::Duration,
    }

    impl ConcurrencyTrackingRunner {
        fn new(delay: std::time::Duration) -> Self {
            Self {
                current: Arc::new(AtomicUsize::new(0)),
                peak: Arc::new(AtomicUsize::new(0)),
                delay,
            }
        }

        fn peak(&self) -> usize {
            self.peak.load(Ordering::SeqCst)
        }
    }

    impl CommandRunner for ConcurrencyTrackingRunner {
        async fn is_command_available(&self, _command: &str) -> bool {
            true
        }

        async fn execute(
            &self,
            _command: &str,
            _token: &CancellationToken,
        ) -> Result<CommandOutput, CommandError> {
            let active = self.current.fetch_add(1, Ordering::SeqCst) + 1;
            self.peak.fetch_max(active, Ordering::SeqCst);

            tokio::time::sleep(self.delay).await;

            self.current.fetch_sub(1, Ordering::SeqCst);

            Ok(CommandOutput {
                output: std::process::Output {
                    status: std::os::unix::process::ExitStatusExt::from_raw(0),
                    stdout: Vec::new(),
                    stderr: Vec::new(),
                },
                duration: self.delay,
            })
        }

        async fn execute_with_timeout(
            &self,
            command: &str,
            _timeout: std::time::Duration,
            token: &CancellationToken,
        ) -> Result<CommandOutput, CommandError> {
            self.execute(command, token).await
        }

        async fn execute_streaming(
            &self,
            _command: &str,
            _timeout: std::time::Duration,
            _output_sender: tokio::sync::mpsc::Sender<crate::commands::runner::OutputChunk>,
            _token: &CancellationToken,
        ) -> Result<CommandOutput, CommandError> {
            unimplemented!("not needed for list tests")
        }
    }

    fn test_sender() -> (
        EventSender,
        mpsc::Receiver<crate::package::event::PackageEvent>,
    ) {
        let (tx, rx) = mpsc::channel(256);
        let sender = EventSender::new_with_context(
            tx,
            crate::package::event::metadata::OperationType::PackageList,
            String::new(),
            "test".to_string(),
            crate::package::event::OperationContext::default(),
        );
        (sender, rx)
    }

    #[tokio::test]
    async fn test_concurrent_checks_limited_by_semaphore() {
        let max_parallel = 2;
        let num_packages = 6;

        let temp_dir = tempfile::TempDir::new().unwrap();
        let config = SelfieConfigBuilder::default()
            .environment("test")
            .package_directory(temp_dir.path())
            .max_parallel_unchecked(max_parallel)
            .build();

        let packages: Vec<_> = (0..num_packages)
            .map(|i| {
                PackageBuilder::default()
                    .name(&format!("pkg-{i}"))
                    .version("1.0.0")
                    .environment("test", |b| {
                        b.install("echo install").check_some("echo check")
                    })
                    .path(temp_dir.path().join(format!("pkg-{i}.yml")))
                    .build()
            })
            .collect();

        let mut mock_repo = MockPackageRepository::new();
        let packages_clone = packages.clone();
        mock_repo.expect_list_packages().returning(move || {
            Ok(crate::package::port::ListPackagesOutput(
                packages_clone.iter().cloned().map(Ok).collect(),
            ))
        });

        let runner = ConcurrencyTrackingRunner::new(std::time::Duration::from_millis(50));

        let (sender, _rx) = test_sender();
        let mut progress = ProgressTracker::new(2);
        let token = CancellationToken::new();

        let result = handle_list(
            &mock_repo,
            &config,
            &runner,
            &sender,
            &mut progress,
            false,
            &token,
        )
        .await;

        assert!(matches!(result, OperationResult::Success(_)));

        let peak_value = runner.peak();
        assert!(
            peak_value <= max_parallel,
            "Peak concurrency {peak_value} exceeded limit {max_parallel}"
        );
        assert!(
            peak_value > 1,
            "Expected some parallelism (peak was {peak_value}), \
             suggests semaphore might be too restrictive"
        );
    }
}
