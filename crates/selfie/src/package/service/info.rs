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
            CheckResult, DependencyStatus, EnvironmentStatus, EnvironmentStatusData, EventSender,
            OperationResult, OperationSuccess, PackageInfoData,
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
        environments: package_blob
            .package
            .environments()
            .keys()
            .cloned()
            .collect(),
        current_environment: config.environment().to_string(),
        git_status: file_git_status,
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

    // Step 2: Check installation status for the current environment
    progress.next(sender, "Checking installation status").await;

    let current_env = config.environment();
    if let Some(env_config) = package_blob.package.environments().get(current_env) {
        let status =
            get_installation_status(package_name, current_env, env_config, command_runner, token)
                .await;
        let max_concurrent = config.max_parallel_installations().get();
        let dependency_statuses = check_dependency_statuses(
            env_config.dependencies(),
            current_env,
            repo,
            command_runner,
            token,
            max_concurrent,
        )
        .await;
        let recommend_statuses = check_dependency_statuses(
            env_config.recommends(),
            current_env,
            repo,
            command_runner,
            token,
            max_concurrent,
        )
        .await;

        let environment_status = EnvironmentStatusData {
            environment_name: current_env.to_string(),
            is_current: true,
            install_command: env_config.install().to_string(),
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

async fn check_dependency_statuses<PR, CR>(
    dependencies: &[String],
    current_env: &str,
    repo: &PR,
    command_runner: &CR,
    token: &CancellationToken,
    max_concurrent: usize,
) -> Vec<DependencyStatus>
where
    PR: PackageRepository,
    CR: CommandRunner,
{
    if dependencies.is_empty() {
        return vec![];
    }

    let mut results = Vec::with_capacity(dependencies.len());
    for chunk in dependencies.chunks(max_concurrent) {
        let futures: Vec<_> = chunk
            .iter()
            .map(|dep_name| {
                check_single_dependency(dep_name, current_env, repo, command_runner, token)
            })
            .collect();
        results.extend(futures::future::join_all(futures).await);
    }
    results
}

async fn check_single_dependency<PR, CR>(
    dep_name: &str,
    current_env: &str,
    repo: &PR,
    command_runner: &CR,
    token: &CancellationToken,
) -> DependencyStatus
where
    PR: PackageRepository,
    CR: CommandRunner,
{
    let dep_package = match repo.get_package(dep_name) {
        Ok(pkg) => pkg,
        Err(err) => {
            return DependencyStatus {
                name: dep_name.to_string(),
                status: EnvironmentStatus::Unknown(format!("{err}")),
            };
        }
    };

    let Some(env_config) = dep_package.package.environments().get(current_env) else {
        return DependencyStatus {
            name: dep_name.to_string(),
            status: EnvironmentStatus::Unknown("not in current environment".to_string()),
        };
    };

    let check_cmd = env_config.check().map(str::to_string);
    let result = super::check::execute_check_command_quiet(
        dep_name,
        current_env,
        check_cmd.as_deref(),
        command_runner,
        token,
    )
    .await;

    DependencyStatus {
        name: dep_name.to_string(),
        status: check_result_to_status(result.result),
    }
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
    }
}

async fn get_installation_status(
    package_name: &str,
    environment: &str,
    env_config: &crate::package::EnvironmentConfig,
    command_runner: &impl CommandRunner,
    token: &CancellationToken,
) -> Option<EnvironmentStatus> {
    let check_cmd = env_config.check().map(str::to_string);
    let result = super::check::execute_check_command_quiet(
        package_name,
        environment,
        check_cmd.as_deref(),
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
        mock_git
            .expect_status_for_directory()
            .returning(|_| Err(GitStatusError::StatusError("simulated failure".to_string())));

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
            .returning(|_, _| Box::pin(async { Ok(mock_command_output(true)) }));

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
            .returning(|_, _| Box::pin(async { Ok(mock_command_output(true)) }));

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
            .returning(|_, _| Box::pin(async { Ok(mock_command_output(true)) }));

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
        mock_runner.expect_execute().returning(|cmd, _| {
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
            .returning(|_, _| Box::pin(async { Ok(mock_command_output(true)) }));

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
