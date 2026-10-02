//!
//! Helpers for spec info and package status operations.
//!
//! `handle_spec_info` — loads the package definition and emits `PackageInfoLoaded` (no runtime
//! commands).
//!
//! `handle_status` — loads the package and checks installation status for the current environment.
//!

use std::path::Path;

use tokio_util::sync::CancellationToken;

use crate::{
    commands::runner::CommandRunner,
    config::SelfieConfig,
    package::{
        event::{
            CheckResult, DependencyStatus, EnvironmentStatus, EnvironmentStatusData, EventSender,
            OperationResult, OperationSuccess, PackageInfoData, ScopedDotfile, StepEnding,
        },
        git::GitStatusProvider,
        port::PackageRepository,
        service::ProgressTracker,
    },
};

/// Spec-only info: load package definition and emit `PackageInfoLoaded`.
/// Does NOT execute any commands or check installation status.
pub(super) async fn handle_spec_info<PR, G>(
    package_name: &str,
    repo: &PR,
    config: &SelfieConfig,
    git: &G,
    sender: &EventSender,
    progress: &mut ProgressTracker,
) -> OperationResult
where
    PR: PackageRepository,
    G: GitStatusProvider,
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

    // Look up git status for the package file
    let file_git_status = match git.status_for_directory(config.package_directory()) {
        Ok(dir_status) => Some(dir_status.status_for_file(package_blob.package.path())),
        Err(e) => {
            sender
                .send_warning(format!("Git status unavailable: {e}"))
                .await;
            None
        }
    };

    // The rule every other command asks: what apply would say about this spec
    // here. A refused spec is still described, since its name and description
    // came through, but what the key may be hiding is left out rather than shown
    // as the file's, and apply would run none of its commands.
    let package = &package_blob.package;
    let refusal = package.spec_refusal(config.environment());
    let (environments, dotfiles, apply_commands, refusal_elsewhere) = if refusal.is_some() {
        (Vec::new(), Vec::new(), 0, None)
    } else {
        (
            package.environments().keys().cloned().collect(),
            package
                .dotfiles_with_scope()
                .into_iter()
                .map(|(scope, entry)| ScopedDotfile {
                    environment: scope.map(str::to_string),
                    entry: entry.clone(),
                    refused: scope.is_some_and(|environment| package.is_refused(environment)),
                })
                .collect(),
            // The current environment's effective set, overrides replacing the
            // shared entries they target, since that is what apply deploys.
            package
                .dotfiles_for_environment(config.environment())
                .iter()
                .map(crate::package::DotfileEntry::command_count)
                .sum(),
            package.listing_refusal().map(|reason| reason.to_string()),
        )
    };

    let package_info = PackageInfoData {
        name: package_blob.package.name().to_string(),
        description: package_blob
            .package
            .description()
            .map(std::string::ToString::to_string),
        homepage: package_blob
            .package
            .homepage()
            .map(std::string::ToString::to_string),
        environments,
        current_environment: config.environment().to_string(),
        git_status: file_git_status,
        refusal: refusal.map(|reason| reason.to_string()),
        refusal_elsewhere,
        dotfiles,
        apply_commands,
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

    let current_env = config.environment();
    if let Some(failure) =
        super::steps::refuse_unreadable_spec(package_name, &package_blob, current_env)
    {
        return failure;
    }

    // Step 2: Check installation status for the current environment. The
    // dependencies and recommends are looked up first, so the step is marked
    // waiting only when a check command will actually run, and names whose.
    let env_config = package_blob.package.environments().get(current_env);
    let looked_up = env_config.map(|env| {
        (
            look_up_dependencies(env.dependencies(), current_env, repo),
            look_up_dependencies(env.recommends(), current_env, repo),
        )
    });
    let mut checked: Vec<&str> = Vec::new();
    if env_config.is_some_and(|env| env.check().is_some()) {
        checked.push(package_name);
    }
    if let Some((dependencies, recommends)) = &looked_up {
        for (name, lookup) in dependencies.iter().chain(recommends) {
            if matches!(lookup, DependencyLookup::Check(_)) && !checked.contains(&name.as_str()) {
                checked.push(name);
            }
        }
    }
    let step = if checked.is_empty() {
        progress.next(sender, "Checking installation status").await;
        None
    } else {
        Some(progress.next_waiting(sender, checks_label(&checked)).await)
    };

    if let (Some(env_config), Some((dependencies, recommends))) = (env_config, looked_up) {
        let package_dir = config.package_directory();
        let status = get_installation_status(
            package_name,
            current_env,
            env_config,
            package_dir,
            command_runner,
            token,
        )
        .await;
        let max_concurrent = config.max_concurrency().get();
        let dependency_statuses = check_dependency_statuses(
            dependencies,
            current_env,
            package_dir,
            command_runner,
            token,
            max_concurrent,
        )
        .await;
        let recommend_statuses = check_dependency_statuses(
            recommends,
            current_env,
            package_dir,
            command_runner,
            token,
            max_concurrent,
        )
        .await;

        // Ended before the status is sent, so a consumer closes the step
        // before it shows the answer.
        if let Some(step) = step {
            let ending = if token.is_cancelled() {
                StepEnding::Cancelled
            } else {
                StepEnding::Succeeded
            };
            sender.send_step_ended(step, ending).await;
        }

        let environment_status = EnvironmentStatusData {
            environment_name: current_env.to_string(),
            is_current: true,
            install_command: env_config.install().map(str::to_string),
            check_command: env_config.check().map(std::string::ToString::to_string),
            dependencies: env_config.dependencies().to_vec(),
            dependency_statuses,
            recommends: env_config.recommends().to_vec(),
            recommend_statuses,
            status,
        };

        sender.send_environment_status(environment_status).await;
    } else {
        sender
            .send_warning(format!(
                "Package '{package_name}' has no configuration for environment '{current_env}'"
            ))
            .await;
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

/// The status step's label: the packages whose checks run, the first few by
/// name and the rest counted, so the line stays one terminal line long.
fn checks_label(checked: &[&str]) -> String {
    const NAMED: usize = 3;
    let commands = if checked.len() == 1 {
        "command"
    } else {
        "commands"
    };
    let mut names = checked
        .iter()
        .take(NAMED)
        .copied()
        .collect::<Vec<_>>()
        .join(", ");
    if checked.len() > NAMED {
        names.push_str(&format!(" and {} more", checked.len() - NAMED));
    }
    format!("Running the check {commands} for {names}")
}

/// What looking a dependency up found, before any command runs.
enum DependencyLookup {
    /// The dependency has a check command, still to run.
    Check(String),
    /// The dependency's status is known without running anything.
    Known(EnvironmentStatus),
}

/// Look each of `dependencies` up, in order, running nothing.
fn look_up_dependencies<PR: PackageRepository>(
    dependencies: &[String],
    current_env: &str,
    repo: &PR,
) -> Vec<(String, DependencyLookup)> {
    dependencies
        .iter()
        .map(|name| (name.clone(), look_up_dependency(name, current_env, repo)))
        .collect()
}

fn look_up_dependency<PR: PackageRepository>(
    dep_name: &str,
    current_env: &str,
    repo: &PR,
) -> DependencyLookup {
    let dep_package = match repo.get_package(dep_name) {
        Ok(pkg) => pkg,
        Err(err) => return DependencyLookup::Known(EnvironmentStatus::Unknown(format!("{err}"))),
    };

    // The refusal is asked before the lookup below, which a shadowing key makes
    // miss or find a decoy.
    if let Some(reason) = dep_package.package.spec_refusal(current_env) {
        return DependencyLookup::Known(EnvironmentStatus::Unknown(format!(
            "is refused: {reason}"
        )));
    }

    let Some(env_config) = dep_package.package.environments().get(current_env) else {
        return DependencyLookup::Known(EnvironmentStatus::Unknown(
            "not in current environment".to_string(),
        ));
    };

    match env_config.check() {
        Some(command) => DependencyLookup::Check(command.to_string()),
        None => DependencyLookup::Known(check_result_to_status(CheckResult::NoCheckCommand)),
    }
}

/// The status of each looked-up dependency, in the order given, running the
/// check commands at most `max_concurrent` at a time.
async fn check_dependency_statuses<CR: CommandRunner>(
    looked_up: Vec<(String, DependencyLookup)>,
    current_env: &str,
    package_dir: &Path,
    command_runner: &CR,
    token: &CancellationToken,
    max_concurrent: usize,
) -> Vec<DependencyStatus> {
    // Only the checks are chunked, so a known status never takes a slot a
    // command could use. `join_all` answers in the order it was given.
    let checks: Vec<(&str, &str)> = looked_up
        .iter()
        .filter_map(|(name, lookup)| match lookup {
            DependencyLookup::Check(command) => Some((name.as_str(), command.as_str())),
            DependencyLookup::Known(_) => None,
        })
        .collect();
    let mut checked = Vec::with_capacity(checks.len());
    for chunk in checks.chunks(max_concurrent.max(1)) {
        let futures: Vec<_> = chunk
            .iter()
            .map(|(name, command)| async move {
                let result = super::check::execute_check_command_quiet(
                    name,
                    current_env,
                    Some(command),
                    package_dir,
                    command_runner,
                    token,
                )
                .await;
                check_result_to_status(result.result)
            })
            .collect();
        checked.extend(futures::future::join_all(futures).await);
    }

    let mut checked = checked.into_iter();
    looked_up
        .into_iter()
        .map(|(name, lookup)| DependencyStatus {
            status: match lookup {
                DependencyLookup::Known(status) => status,
                // One status per check, taken in the same order the checks ran.
                DependencyLookup::Check(_) => checked
                    .next()
                    .unwrap_or_else(|| EnvironmentStatus::Unknown("not checked".to_string())),
            },
            name,
        })
        .collect()
}

fn check_result_to_status(result: CheckResult) -> EnvironmentStatus {
    match result {
        CheckResult::Success { .. } => EnvironmentStatus::Installed,
        CheckResult::Failed { .. } => EnvironmentStatus::NotInstalled,
        CheckResult::NoCheckCommand => EnvironmentStatus::Unknown("no check command".to_string()),
        CheckResult::CommandNotFound => {
            EnvironmentStatus::Unknown("check command not found".to_string())
        }
        CheckResult::Error(e) => EnvironmentStatus::Unknown(e),
        CheckResult::TimedOut(timed_out) => EnvironmentStatus::Unknown(timed_out.to_string()),
    }
}

async fn get_installation_status(
    package_name: &str,
    environment: &str,
    env_config: &crate::package::EnvironmentConfig,
    package_dir: &Path,
    command_runner: &impl CommandRunner,
    token: &CancellationToken,
) -> Option<EnvironmentStatus> {
    let check_cmd = env_config.check().map(str::to_string);
    let result = super::check::execute_check_command_quiet(
        package_name,
        environment,
        check_cmd.as_deref(),
        package_dir,
        command_runner,
        token,
    )
    .await;
    Some(check_result_to_status(result.result))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        commands::runner::{CommandOutput, MockCommandRunner},
        config::SelfieConfigBuilder,
        package::{
            GetPackage, PackageBuilder,
            event::{PackageEvent, metadata::OperationType},
            git::{GitDirectoryStatus, GitFileStatus, GitStatusError, MockGitStatusProvider},
            port::{MockPackageRepository, PackageError},
        },
    };
    use std::collections::HashMap;
    use std::os::unix::process::ExitStatusExt;
    use std::process::Output;
    use tokio::sync::mpsc;

    // The step's label names the first three packages and counts the rest, so it
    // stays one line however many dependencies a package has.
    #[test]
    fn the_status_step_names_three_checks_and_counts_the_rest() {
        assert_eq!(checks_label(&["a"]), "Running the check command for a");
        assert_eq!(
            checks_label(&["a", "b", "c"]),
            "Running the check commands for a, b, c"
        );
        assert_eq!(
            checks_label(&["a", "b", "c", "d", "e"]),
            "Running the check commands for a, b, c and 2 more"
        );
    }

    fn test_sender() -> (EventSender, mpsc::Receiver<PackageEvent>) {
        let (tx, rx) = mpsc::channel(256);
        let sender = EventSender::new_with_context(
            tx,
            OperationType::SpecInfo,
            "test-pkg".to_string(),
            "test".to_string(),
            crate::package::event::OperationContext::default(),
        );
        (sender, rx)
    }

    #[tokio::test]
    async fn test_spec_info_includes_git_status() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let pkg_path = temp_dir.path().join("test-pkg.yml");
        let config = SelfieConfigBuilder::default()
            .environment("test")
            .package_directory(temp_dir.path())
            .build();

        let package = PackageBuilder::default()
            .name("test-pkg")
            .environment("test", |b| b.install("echo install"))
            .path(&pkg_path)
            .build();

        let mut mock_repo = MockPackageRepository::new();
        let package_clone = package.clone();
        mock_repo.expect_get_package().returning(move |_| {
            Ok(crate::package::GetPackage::from_existing(
                package_clone.clone(),
                pkg_path.clone(),
            ))
        });

        let mut mock_git = MockGitStatusProvider::new();
        let mut files = HashMap::new();
        files.insert(package.path().clone(), GitFileStatus::Modified);
        mock_git.expect_status_for_directory().returning(move |_| {
            Ok(GitDirectoryStatus {
                in_repo: true,
                files: files.clone(),
            })
        });

        let (sender, mut rx) = test_sender();
        let mut progress = ProgressTracker::new(2);

        let result = handle_spec_info(
            "test-pkg",
            &mock_repo,
            &config,
            &mock_git,
            &sender,
            &mut progress,
        )
        .await;

        assert!(matches!(result, OperationResult::Success(_)));

        drop(sender);
        let mut found_info = false;
        while let Some(event) = rx.recv().await {
            if let PackageEvent::PackageInfoLoaded { package_info, .. } = event {
                assert_eq!(package_info.git_status, Some(GitFileStatus::Modified));
                found_info = true;
            }
        }
        assert!(found_info, "Expected PackageInfoLoaded event");
    }

    #[tokio::test]
    async fn test_spec_info_git_error_emits_warning_and_none() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let pkg_path = temp_dir.path().join("test-pkg.yml");
        let config = SelfieConfigBuilder::default()
            .environment("test")
            .package_directory(temp_dir.path())
            .build();

        let package = PackageBuilder::default()
            .name("test-pkg")
            .environment("test", |b| b.install("echo install"))
            .path(&pkg_path)
            .build();

        let mut mock_repo = MockPackageRepository::new();
        let package_clone = package.clone();
        mock_repo.expect_get_package().returning(move |_| {
            Ok(crate::package::GetPackage::from_existing(
                package_clone.clone(),
                pkg_path.clone(),
            ))
        });

        let mut mock_git = MockGitStatusProvider::new();
        mock_git.expect_status_for_directory().returning(|_| {
            Err(GitStatusError::StatusError(crate::git::GitMessage::new(
                "simulated failure",
            )))
        });

        let (sender, mut rx) = test_sender();
        let mut progress = ProgressTracker::new(2);

        let result = handle_spec_info(
            "test-pkg",
            &mock_repo,
            &config,
            &mock_git,
            &sender,
            &mut progress,
        )
        .await;

        assert!(matches!(result, OperationResult::Success(_)));

        drop(sender);
        let mut found_info = false;
        let mut found_warning = false;
        while let Some(event) = rx.recv().await {
            match event {
                PackageEvent::PackageInfoLoaded { package_info, .. } => {
                    assert_eq!(package_info.git_status, None);
                    found_info = true;
                }
                PackageEvent::Warning { message, .. } => {
                    assert!(message.contains("Git status unavailable"));
                    found_warning = true;
                }
                _ => {}
            }
        }
        assert!(found_info, "Expected PackageInfoLoaded event");
        assert!(found_warning, "Expected Warning event for git failure");
    }

    fn mock_command_output(success: bool) -> CommandOutput {
        let exit_code = if success { 0 } else { 1 };
        CommandOutput {
            output: Output {
                status: std::process::ExitStatus::from_raw(exit_code * 256),
                stdout: Vec::new(),
                stderr: Vec::new(),
            },
            duration: std::time::Duration::from_millis(10),
        }
    }

    fn status_test_sender() -> (EventSender, mpsc::Receiver<PackageEvent>) {
        let (tx, rx) = mpsc::channel(256);
        let sender = EventSender::new_with_context(
            tx,
            OperationType::PackageStatus,
            "test-pkg".to_string(),
            "test".to_string(),
            crate::package::event::OperationContext::default(),
        );
        (sender, rx)
    }

    #[tokio::test]
    async fn test_status_checks_dependency_statuses() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let pkg_path = temp_dir.path().join("test-pkg.yml");
        let dep_path = temp_dir.path().join("dep-pkg.yml");
        let config = SelfieConfigBuilder::default()
            .environment("test")
            .package_directory(temp_dir.path())
            .build();

        let package = PackageBuilder::default()
            .name("test-pkg")
            .environment("test", |b| {
                b.install("echo install")
                    .check_some("echo check")
                    .dependencies(vec!["dep-pkg"])
            })
            .path(&pkg_path)
            .build();

        let dep_package = PackageBuilder::default()
            .name("dep-pkg")
            .environment("test", |b| {
                b.install("echo install-dep").check_some("echo check-dep")
            })
            .path(&dep_path)
            .build();

        let mut mock_repo = MockPackageRepository::new();
        let pkg_clone = package.clone();
        let dep_clone = dep_package.clone();
        mock_repo.expect_get_package().returning(move |name: &str| {
            if name == "test-pkg" {
                Ok(GetPackage::from_existing(
                    pkg_clone.clone(),
                    pkg_path.clone(),
                ))
            } else if name == "dep-pkg" {
                Ok(GetPackage::from_existing(
                    dep_clone.clone(),
                    dep_path.clone(),
                ))
            } else {
                Err(PackageError::PackageNotFound {
                    name: name.to_string(),
                    packages_path: temp_dir.path().to_path_buf(),
                    files_examined: 0,
                    search_patterns: vec![],
                }
                .into())
            }
        });

        let mut mock_runner = MockCommandRunner::new();
        mock_runner
            .expect_execute()
            .returning(|_, _, _| Box::pin(async { Ok(mock_command_output(true)) }));

        let (sender, mut rx) = status_test_sender();
        let mut progress = ProgressTracker::new(2);
        let token = CancellationToken::new();

        let result = handle_status(
            "test-pkg",
            &mock_repo,
            &config,
            &mock_runner,
            &sender,
            &mut progress,
            &token,
        )
        .await;

        assert!(matches!(result, OperationResult::Success(_)));

        drop(sender);
        let mut found_env_status = false;
        while let Some(event) = rx.recv().await {
            if let PackageEvent::EnvironmentStatusChecked {
                environment_status, ..
            } = event
                && environment_status.is_current
            {
                assert_eq!(environment_status.dependency_statuses.len(), 1);
                assert_eq!(environment_status.dependency_statuses[0].name, "dep-pkg");
                assert!(matches!(
                    environment_status.dependency_statuses[0].status,
                    EnvironmentStatus::Installed
                ));
                found_env_status = true;
            }
        }
        assert!(found_env_status, "Expected EnvironmentStatusChecked event");
    }

    #[tokio::test]
    async fn test_status_dep_not_found() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let pkg_path = temp_dir.path().join("test-pkg.yml");
        let config = SelfieConfigBuilder::default()
            .environment("test")
            .package_directory(temp_dir.path())
            .build();

        let package = PackageBuilder::default()
            .name("test-pkg")
            .environment("test", |b| {
                b.install("echo install")
                    .check_some("echo check")
                    .dependencies(vec!["missing-dep"])
            })
            .path(&pkg_path)
            .build();

        let mut mock_repo = MockPackageRepository::new();
        let pkg_clone = package.clone();
        mock_repo.expect_get_package().returning(move |name: &str| {
            if name == "test-pkg" {
                Ok(GetPackage::from_existing(
                    pkg_clone.clone(),
                    pkg_path.clone(),
                ))
            } else {
                Err(PackageError::PackageNotFound {
                    name: name.to_string(),
                    packages_path: temp_dir.path().to_path_buf(),
                    files_examined: 0,
                    search_patterns: vec![],
                }
                .into())
            }
        });

        let mut mock_runner = MockCommandRunner::new();
        mock_runner
            .expect_execute()
            .returning(|_, _, _| Box::pin(async { Ok(mock_command_output(true)) }));

        let (sender, mut rx) = status_test_sender();
        let mut progress = ProgressTracker::new(2);
        let token = CancellationToken::new();

        let result = handle_status(
            "test-pkg",
            &mock_repo,
            &config,
            &mock_runner,
            &sender,
            &mut progress,
            &token,
        )
        .await;

        assert!(matches!(result, OperationResult::Success(_)));

        drop(sender);
        let mut found = false;
        while let Some(event) = rx.recv().await {
            if let PackageEvent::EnvironmentStatusChecked {
                environment_status, ..
            } = event
                && environment_status.is_current
            {
                assert_eq!(environment_status.dependency_statuses.len(), 1);
                assert_eq!(
                    environment_status.dependency_statuses[0].name,
                    "missing-dep"
                );
                assert!(matches!(
                    &environment_status.dependency_statuses[0].status,
                    EnvironmentStatus::Unknown(reason) if reason.contains("not found")
                ));
                found = true;
            }
        }
        assert!(found, "Expected EnvironmentStatusChecked event");
    }

    #[tokio::test]
    async fn test_status_dep_not_in_current_env() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let pkg_path = temp_dir.path().join("test-pkg.yml");
        let dep_path = temp_dir.path().join("dep-pkg.yml");
        let config = SelfieConfigBuilder::default()
            .environment("test")
            .package_directory(temp_dir.path())
            .build();

        let package = PackageBuilder::default()
            .name("test-pkg")
            .environment("test", |b| {
                b.install("echo install")
                    .check_some("echo check")
                    .dependencies(vec!["dep-pkg"])
            })
            .path(&pkg_path)
            .build();

        // dep-pkg only has "other-env", not "test"
        let dep_package = PackageBuilder::default()
            .name("dep-pkg")
            .environment("other-env", |b| b.install("echo install-dep"))
            .path(&dep_path)
            .build();

        let mut mock_repo = MockPackageRepository::new();
        let pkg_clone = package.clone();
        let dep_clone = dep_package.clone();
        mock_repo.expect_get_package().returning(move |name: &str| {
            if name == "test-pkg" {
                Ok(GetPackage::from_existing(
                    pkg_clone.clone(),
                    pkg_path.clone(),
                ))
            } else if name == "dep-pkg" {
                Ok(GetPackage::from_existing(
                    dep_clone.clone(),
                    dep_path.clone(),
                ))
            } else {
                Err(PackageError::PackageNotFound {
                    name: name.to_string(),
                    packages_path: temp_dir.path().to_path_buf(),
                    files_examined: 0,
                    search_patterns: vec![],
                }
                .into())
            }
        });

        let mut mock_runner = MockCommandRunner::new();
        mock_runner
            .expect_execute()
            .returning(|_, _, _| Box::pin(async { Ok(mock_command_output(true)) }));

        let (sender, mut rx) = status_test_sender();
        let mut progress = ProgressTracker::new(2);
        let token = CancellationToken::new();

        let result = handle_status(
            "test-pkg",
            &mock_repo,
            &config,
            &mock_runner,
            &sender,
            &mut progress,
            &token,
        )
        .await;

        assert!(matches!(result, OperationResult::Success(_)));

        drop(sender);
        let mut found = false;
        while let Some(event) = rx.recv().await {
            if let PackageEvent::EnvironmentStatusChecked {
                environment_status, ..
            } = event
                && environment_status.is_current
            {
                assert_eq!(environment_status.dependency_statuses.len(), 1);
                assert!(matches!(
                    &environment_status.dependency_statuses[0].status,
                    EnvironmentStatus::Unknown(reason) if reason.contains("not in current environment")
                ));
                found = true;
            }
        }
        assert!(found, "Expected EnvironmentStatusChecked event");
    }

    #[tokio::test]
    async fn test_status_dep_not_installed() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let pkg_path = temp_dir.path().join("test-pkg.yml");
        let dep_path = temp_dir.path().join("dep-pkg.yml");
        let config = SelfieConfigBuilder::default()
            .environment("test")
            .package_directory(temp_dir.path())
            .build();

        let package = PackageBuilder::default()
            .name("test-pkg")
            .environment("test", |b| {
                b.install("echo install")
                    .check_some("echo check")
                    .dependencies(vec!["dep-pkg"])
            })
            .path(&pkg_path)
            .build();

        let dep_package = PackageBuilder::default()
            .name("dep-pkg")
            .environment("test", |b| {
                b.install("echo install-dep").check_some("false")
            })
            .path(&dep_path)
            .build();

        let mut mock_repo = MockPackageRepository::new();
        let pkg_clone = package.clone();
        let dep_clone = dep_package.clone();
        mock_repo.expect_get_package().returning(move |name: &str| {
            if name == "test-pkg" {
                Ok(GetPackage::from_existing(
                    pkg_clone.clone(),
                    pkg_path.clone(),
                ))
            } else if name == "dep-pkg" {
                Ok(GetPackage::from_existing(
                    dep_clone.clone(),
                    dep_path.clone(),
                ))
            } else {
                Err(PackageError::PackageNotFound {
                    name: name.to_string(),
                    packages_path: temp_dir.path().to_path_buf(),
                    files_examined: 0,
                    search_patterns: vec![],
                }
                .into())
            }
        });

        let mut mock_runner = MockCommandRunner::new();
        // Main package check succeeds, dep check fails
        mock_runner.expect_execute().returning(|cmd, _, _| {
            let success = cmd != "false";
            Box::pin(async move { Ok(mock_command_output(success)) })
        });

        let (sender, mut rx) = status_test_sender();
        let mut progress = ProgressTracker::new(2);
        let token = CancellationToken::new();

        let result = handle_status(
            "test-pkg",
            &mock_repo,
            &config,
            &mock_runner,
            &sender,
            &mut progress,
            &token,
        )
        .await;

        assert!(matches!(result, OperationResult::Success(_)));

        drop(sender);
        let mut found = false;
        while let Some(event) = rx.recv().await {
            if let PackageEvent::EnvironmentStatusChecked {
                environment_status, ..
            } = event
                && environment_status.is_current
            {
                assert_eq!(environment_status.dependency_statuses.len(), 1);
                assert!(matches!(
                    environment_status.dependency_statuses[0].status,
                    EnvironmentStatus::NotInstalled
                ));
                found = true;
            }
        }
        assert!(found, "Expected EnvironmentStatusChecked event");
    }

    #[tokio::test]
    async fn test_status_no_deps_empty_statuses() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let pkg_path = temp_dir.path().join("test-pkg.yml");
        let config = SelfieConfigBuilder::default()
            .environment("test")
            .package_directory(temp_dir.path())
            .build();

        let package = PackageBuilder::default()
            .name("test-pkg")
            .environment("test", |b| {
                b.install("echo install").check_some("echo check")
            })
            .path(&pkg_path)
            .build();

        let mut mock_repo = MockPackageRepository::new();
        let pkg_clone = package.clone();
        mock_repo.expect_get_package().returning(move |_| {
            Ok(GetPackage::from_existing(
                pkg_clone.clone(),
                pkg_path.clone(),
            ))
        });

        let mut mock_runner = MockCommandRunner::new();
        mock_runner
            .expect_execute()
            .returning(|_, _, _| Box::pin(async { Ok(mock_command_output(true)) }));

        let (sender, mut rx) = status_test_sender();
        let mut progress = ProgressTracker::new(2);
        let token = CancellationToken::new();

        let result = handle_status(
            "test-pkg",
            &mock_repo,
            &config,
            &mock_runner,
            &sender,
            &mut progress,
            &token,
        )
        .await;

        assert!(matches!(result, OperationResult::Success(_)));

        drop(sender);
        let mut found = false;
        while let Some(event) = rx.recv().await {
            if let PackageEvent::EnvironmentStatusChecked {
                environment_status, ..
            } = event
                && environment_status.is_current
            {
                assert!(environment_status.dependency_statuses.is_empty());
                found = true;
            }
        }
        assert!(found, "Expected EnvironmentStatusChecked event");
    }
}
