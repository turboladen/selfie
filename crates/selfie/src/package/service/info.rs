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
            recommends: env_config.recommends().to_vec(),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::SelfieConfigBuilder,
        package::{
            PackageBuilder,
            event::{PackageEvent, metadata::OperationType},
            git::{GitDirectoryStatus, GitFileStatus, GitStatusError, MockGitStatusProvider},
            port::MockPackageRepository,
        },
    };
    use std::collections::HashMap;
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
}
