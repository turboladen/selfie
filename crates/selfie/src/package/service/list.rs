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
            EnvironmentStatus, EventSender, Gap, OperationResult, OperationSuccess,
            PackageListData, PackageListItem, RefusedSpec, StepEnding, UnknownStatus,
        },
        port::PackageRepository,
        service::ProgressTracker,
    },
};

/// A package's identity, kept beside its check task's handle so a task that
/// panics is still listed under its own name.
struct PackageMetadata {
    name: String,
    environments: Vec<String>,
    check_command: Option<String>,
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

    // A name several files claim is no package: each loaded file of it is a
    // refused row, and one that did not parse keeps its own failure row.
    let names = list_output.names();
    let valid_packages = names.usable;
    let invalid_packages: Vec<_> = list_output.invalid_packages().collect();

    // Sort packages alphabetically by name before processing
    let mut sorted_packages: Vec<_> = valid_packages.into_iter().collect();
    sorted_packages.sort_by(|a, b| a.name().cmp(b.name()));

    // Refused specs are separated before anything reads the environment mapping.
    // A key shadowing `environments:` makes the filter below miss, and the spec
    // would vanish from the listing with nothing said. The rule follows the
    // environments the listing shows: apply's question in this one, and any
    // environment's with `--all`.
    let shown = if show_all {
        crate::package::refusal::Shown::Every
    } else {
        crate::package::refusal::Shown::Current(config.environment())
    };
    let (sorted_packages, refused) =
        crate::package::refusal::separate_refused(sorted_packages, shown);
    let refused: Vec<RefusedSpec> = names
        .rows
        .into_iter()
        .chain(refused.into_iter().map(|(_, spec)| spec))
        .collect();

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

    // Name every package to be checked before any check begins.
    let ready_names: Vec<String> = packages_to_process
        .iter()
        .map(|package| package.name().to_string())
        .collect();
    sender.send_package_list_ready(ready_names).await;

    // Step 2: Check package status in parallel. Waiting when some package's
    // check command will run.
    let with_checks = packages_to_process
        .iter()
        .filter(|package| {
            package
                .environments()
                .get(config.environment())
                .is_some_and(|env| env.check().is_some())
        })
        .count();
    let step = if with_checks > 0 {
        let step = progress
            .next_waiting(
                sender,
                format!(
                    "Checking {with_checks} {}",
                    crate::pluralize(with_checks, "package", "packages")
                ),
            )
            .await;
        Some(step)
    } else {
        progress.next(sender, "Checking package status").await;
        None
    };

    // Limit concurrent subprocess spawns to avoid exhausting file descriptors.
    let semaphore = Arc::new(Semaphore::new(config.max_concurrency().get()));
    let package_dir: Arc<std::path::Path> = Arc::from(config.package_directory().as_path());

    // Create parallel tasks for status checking with order preservation. Each
    // handle travels with its package's identity, so a task that panics is
    // still listed under its own name.
    let check_futures: Vec<_> = packages_to_process
        .iter()
        .enumerate()
        .map(|(index, package)| {
            let package_name = package.name().to_string();
            let package_environments: Vec<String> =
                package.environments().keys().cloned().collect();
            let current_env = config.environment().to_string();

            // Determine status based on environment support and check command
            let env_config = package.environments().get(config.environment());
            let check_command = env_config.and_then(|ec| ec.check()).map(str::to_string);
            let supports_current_env = env_config.is_some();

            let command_runner = command_runner.clone();
            let package_dir = Arc::clone(&package_dir);
            let sender = sender.clone();
            let token = token.clone();
            let semaphore = semaphore.clone();
            let metadata = PackageMetadata {
                name: package_name.clone(),
                environments: package_environments.clone(),
                check_command: check_command.clone(),
            };

            let handle = tokio::spawn(async move {
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
                        &package_dir,
                        &command_runner,
                        &token,
                    )
                    .await;
                    EnvironmentStatus::from(&check_result.result)
                } else if supports_current_env {
                    EnvironmentStatus::Unknown(UnknownStatus::NoCheckCommand)
                } else {
                    // Shown only under `--all`: the spec decides there is
                    // nothing to check here.
                    EnvironmentStatus::Unknown(UnknownStatus::NotInEnvironment)
                };

                // Create the package list item
                let package_item = PackageListItem {
                    check_command,
                    name: package_name,
                    environments: package_environments,
                    status,
                };

                // Stream the individual result immediately
                sender.send_package_list_item(package_item.clone()).await;

                (index, package_item)
            });
            (metadata, handle)
        })
        .collect();

    // Wait for all tasks and collect results in original order
    let mut results: Vec<(usize, PackageListItem)> = Vec::new();
    for (i, (metadata, handle)) in check_futures.into_iter().enumerate() {
        match handle.await {
            Ok(result) => results.push(result),
            Err(e) => {
                // Named from the metadata that travels with the handle, so the CLI
                // resolves the right spinner.
                let package_item = PackageListItem {
                    check_command: metadata.check_command,
                    name: metadata.name,
                    environments: metadata.environments,
                    status: EnvironmentStatus::Unknown(UnknownStatus::CheckFailed(
                        crate::package::event::CheckFailure::Internal {
                            message: crate::commands::BoundedText::of(&format!("Task failed: {e}")),
                        },
                    )),
                };
                sender.send_package_list_item(package_item.clone()).await;
                results.push((i, package_item));
            }
        }
    }

    // Every check has answered, each in its own item; one that errored is that
    // item's answer, not a failure of the step.
    if let Some(step) = step {
        let ending = if token.is_cancelled() {
            StepEnding::Canceled
        } else {
            StepEnding::Succeeded
        };
        sender.send_step_ended(step, ending).await;
    }

    // Sort by original index to maintain alphabetical order for final summary
    results.sort_by_key(|(index, _)| *index);
    let valid_package_items: Vec<PackageListItem> =
        results.into_iter().map(|(_, item)| item).collect();

    // The failures themselves, not a path and a rendered sentence beside them:
    // both of those are derived from this value, so carrying them separately keeps
    // a duplicate nothing enforces.
    let invalid_package_items: Vec<crate::package::port::PackageParseError> = invalid_packages
        .iter()
        .map(|invalid| (*invalid).clone())
        .collect();

    // Every row the listing prints is covered, and each one selfie did not
    // answer for is a gap, classified by the one rule every command uses.
    let gaps = listing_gaps(
        &valid_package_items,
        &invalid_package_items,
        &refused,
        config.package_directory(),
    );

    // Calculate the counts before moving the vectors
    let valid_count = valid_package_items.len();
    let refused_count = refused.len();

    let package_list_data = PackageListData {
        valid_packages: valid_package_items,
        invalid_packages: invalid_package_items,
        refused,
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
        refused_count,
        gaps,
        config.environment().to_string(),
        (progress.current_step(), progress.total_steps()).into(),
    ))
}

/// Returns the gaps among a listing's rows. A valid row is judged by its
/// status, a spec that does not parse by its lookup failure, and a refused spec
/// by its refusal.
fn listing_gaps(
    valid: &[PackageListItem],
    invalid: &[crate::package::port::PackageParseError],
    refused: &[RefusedSpec],
    package_directory: &std::path::Path,
) -> Vec<Gap> {
    let valid_rows = valid.iter().map(|item| item.status.gap(&item.name));
    let invalid_rows = invalid
        .iter()
        .map(|failure| Gap::unparsable(&failure.display_name(), failure, package_directory));
    let refused_rows = refused.iter().map(RefusedSpec::gap);
    valid_rows
        .chain(invalid_rows)
        .chain(refused_rows)
        .flatten()
        .collect()
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

    // A cloneable command runner that tracks peak concurrency.
    #[derive(Clone)]
    struct ConcurrencyTrackingRunner {
        current: Arc<AtomicUsize>,
        peak: Arc<AtomicUsize>,
        delay: std::time::Duration,
        // Whether `execute` panics, as a bug in a check task would.
        panics: bool,
    }

    impl ConcurrencyTrackingRunner {
        fn new(delay: std::time::Duration) -> Self {
            Self {
                current: Arc::new(AtomicUsize::new(0)),
                peak: Arc::new(AtomicUsize::new(0)),
                delay,
                panics: false,
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
            _working_dir: &std::path::Path,
            _token: &CancellationToken,
        ) -> Result<CommandOutput, CommandError> {
            assert!(!self.panics, "the check task panicked");
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

        async fn execute_in_dir(
            &self,
            command: &str,
            working_dir: &std::path::Path,
            _timeout: std::time::Duration,
            token: &CancellationToken,
        ) -> Result<CommandOutput, CommandError> {
            self.execute(command, working_dir, token).await
        }

        async fn execute_streaming(
            &self,
            _command: &str,
            _working_dir: &std::path::Path,
            _timeout: std::time::Duration,
            _output_sender: tokio::sync::mpsc::Sender<crate::commands::runner::OutputChunk>,
            _token: &CancellationToken,
        ) -> Result<CommandOutput, CommandError> {
            unimplemented!("not needed for list tests")
        }

        async fn execute_for_content(
            &self,
            _command: &str,
            _working_dir: &std::path::Path,
            _timeout: std::time::Duration,
            _token: &CancellationToken,
        ) -> Result<crate::commands::runner::ContentOutput, CommandError> {
            unimplemented!("listing runs no content commands")
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

    // A check task that panics is listed, its item says the fault is selfie's,
    // and the listing fails: a selfie bug never reads as a finding.
    #[tokio::test]
    async fn a_panicked_check_task_is_listed_and_fails_the_listing() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let config = SelfieConfigBuilder::default()
            .environment("test")
            .package_directory(temp_dir.path())
            .build();
        let package = PackageBuilder::default()
            .name("pkg")
            .environment("test", |b| {
                b.install("echo install").check_some("echo check")
            })
            .path(temp_dir.path().join("pkg.yml"))
            .build();
        let mut mock_repo = MockPackageRepository::new();
        mock_repo.expect_list_packages().returning(move || {
            Ok(crate::package::port::ListPackagesOutput(vec![Ok(
                package.clone()
            )]))
        });
        let runner = ConcurrencyTrackingRunner {
            panics: true,
            ..ConcurrencyTrackingRunner::new(std::time::Duration::ZERO)
        };
        let (sender, mut rx) = test_sender();
        let mut progress = ProgressTracker::new(2);

        let result = handle_list(
            &mock_repo,
            &config,
            &runner,
            &sender,
            &mut progress,
            false,
            &CancellationToken::new(),
        )
        .await;
        drop(sender);

        assert!(
            matches!(
                &result,
                OperationResult::Success(OperationSuccess::PackageListGenerated {
                    valid_count: 1,
                    gaps,
                    ..
                }) if matches!(
                    &gaps[..],
                    [gap] if gap.package_name() == "pkg"
                        && gap.coverage() == crate::package::event::Coverage::Error
                )
            ),
            "{result:?}"
        );
        assert_eq!(result.outcome(), crate::package::event::Outcome::Failed);
        let mut listed = None;
        while let Some(event) = rx.recv().await {
            if let crate::package::event::PackageEvent::PackageListLoaded { package_list, .. } =
                event
            {
                listed = Some(package_list.valid_packages);
            }
        }
        let listed = listed.expect("the summary must be sent");
        assert!(
            matches!(
                &listed[..],
                [item] if matches!(
                    item.status,
                    EnvironmentStatus::Unknown(UnknownStatus::CheckFailed(
                        crate::package::event::CheckFailure::Internal { .. }
                    ))
                )
            ),
            "{listed:?}"
        );
    }

    #[tokio::test]
    async fn test_concurrent_checks_limited_by_semaphore() {
        let max_parallel = 2;
        let num_packages = 6;

        let temp_dir = tempfile::TempDir::new().unwrap();
        let config = SelfieConfigBuilder::default()
            .environment("test")
            .package_directory(temp_dir.path())
            .max_concurrency_unchecked(max_parallel)
            .build();

        let packages: Vec<_> = (0..num_packages)
            .map(|i| {
                PackageBuilder::default()
                    .name(&format!("pkg-{i}"))
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
