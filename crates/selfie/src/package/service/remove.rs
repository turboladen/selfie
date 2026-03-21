//!
//! Helps break down the pieces of running the `package remove` command.
//!

use crate::{
    config::SelfieConfig,
    package::{
        event::{EventSender, OperationResult, OperationSuccess},
        port::PackageRepository,
        service::ProgressTracker,
    },
};

pub(super) async fn handle_remove<PR>(
    package_name: &str,
    repo: &PR,
    config: &SelfieConfig,
    sender: &EventSender,
    progress: &mut ProgressTracker,
) -> OperationResult
where
    PR: PackageRepository,
{
    // Step 1: Load the package (to verify it exists and get file path)
    progress.next(sender, "Loading package").await;

    let get_package = match repo.get_package(package_name) {
        Ok(pkg) => pkg,
        Err(err) => return OperationResult::Failure(err.into()),
    };

    let file_path = get_package.file_path().to_path_buf();

    // Step 2: Check for dependent packages
    progress
        .next(sender, "Checking for dependent packages")
        .await;

    let dependent_packages = match repo.find_dependent_packages(package_name) {
        Ok(deps) => deps,
        Err(err) => {
            sender
                .send_warning(format!("Failed to check for dependent packages: {err}"))
                .await;
            vec![]
        }
    };

    let dependent_names: Vec<String> = dependent_packages
        .iter()
        .map(|p| p.name().to_string())
        .collect();

    if !dependent_names.is_empty() {
        sender
            .send_removal_dependency_info(package_name.to_string(), dependent_names.clone())
            .await;
    }

    // Step 2b: Check for config files that may need cleanup
    let configs = get_package.package().configs();
    if !configs.is_empty() {
        let config_targets: Vec<String> = configs.iter().map(|c| c.target().to_string()).collect();
        sender
            .send_config_cleanup_info(package_name.to_string(), config_targets)
            .await;
    }

    // Step 3: Remove the package
    progress.next(sender, "Removing package file").await;

    if let Err(err) = repo.remove_package(package_name) {
        return OperationResult::Failure(err.into());
    }

    sender
        .send_debug(format!(
            "Package '{}' removed from {}",
            package_name,
            file_path.display()
        ))
        .await;

    OperationResult::Success(OperationSuccess::package_removed(
        package_name.to_string(),
        file_path,
        config.environment().to_string(),
        dependent_names,
        (progress.current_step(), progress.total_steps()).into(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::SelfieConfigBuilder,
        package::{
            ConfigEntry, GetPackage, PackageBuilder,
            event::{OperationContext, OperationResult},
            port::MockPackageRepository,
            service::ProgressTracker,
        },
    };
    use std::path::PathBuf;
    use tokio::sync::mpsc;

    fn test_config() -> crate::config::SelfieConfig {
        SelfieConfigBuilder::default()
            .environment("test-env")
            .package_directory("/test/packages")
            .build()
    }

    fn test_sender() -> (
        EventSender,
        mpsc::Receiver<crate::package::event::PackageEvent>,
    ) {
        let (tx, rx) = mpsc::channel(32);
        let sender = EventSender::new_with_context(
            tx,
            crate::package::event::metadata::OperationType::PackageRemove,
            "test-pkg".to_string(),
            "test-env".to_string(),
            OperationContext::default(),
        );
        (sender, rx)
    }

    #[tokio::test]
    async fn test_remove_success() {
        let mut mock_repo = MockPackageRepository::new();
        let config = test_config();
        let (sender, _rx) = test_sender();
        let mut progress = ProgressTracker::new(3);

        let package = PackageBuilder::default()
            .name("test-pkg")
            .version("1.0.0")
            .environment("test-env", |b| b.install("brew install test"))
            .path("/test/packages/test-pkg.yml")
            .build();

        let get_package =
            GetPackage::from_existing(package, PathBuf::from("/test/packages/test-pkg.yml"));

        mock_repo
            .expect_get_package()
            .return_once(move |_| Ok(get_package));

        mock_repo
            .expect_find_dependent_packages()
            .return_once(|_| Ok(vec![]));

        mock_repo.expect_remove_package().return_once(|_| Ok(()));

        let result = handle_remove("test-pkg", &mock_repo, &config, &sender, &mut progress).await;

        assert!(matches!(result, OperationResult::Success(_)));
    }

    #[tokio::test]
    async fn test_remove_with_dependents() {
        let mut mock_repo = MockPackageRepository::new();
        let config = test_config();
        let (sender, mut rx) = test_sender();
        let mut progress = ProgressTracker::new(3);

        let package = PackageBuilder::default()
            .name("target-pkg")
            .version("1.0.0")
            .environment("test-env", |b| b.install("brew install target"))
            .path("/test/packages/target-pkg.yml")
            .build();

        let get_package =
            GetPackage::from_existing(package, PathBuf::from("/test/packages/target-pkg.yml"));

        let dependent = PackageBuilder::default()
            .name("dependent-pkg")
            .version("1.0.0")
            .environment("test-env", |b| {
                b.install("brew install dependent")
                    .dependencies(vec!["target-pkg"])
            })
            .path("/test/packages/dependent-pkg.yml")
            .build();

        mock_repo
            .expect_get_package()
            .return_once(move |_| Ok(get_package));

        mock_repo
            .expect_find_dependent_packages()
            .return_once(move |_| Ok(vec![dependent]));

        mock_repo.expect_remove_package().return_once(|_| Ok(()));

        let result = handle_remove("target-pkg", &mock_repo, &config, &sender, &mut progress).await;

        // Should still succeed
        assert!(matches!(result, OperationResult::Success(_)));

        // Check that RemovalDependencyInfo was sent
        let mut found_dep_info = false;
        // Drain events from the channel
        while let Ok(event) = rx.try_recv() {
            if let crate::package::event::PackageEvent::RemovalDependencyInfo {
                dependent_packages,
                ..
            } = event
            {
                assert_eq!(dependent_packages, vec!["dependent-pkg".to_string()]);
                found_dep_info = true;
            }
        }
        assert!(found_dep_info, "Expected RemovalDependencyInfo event");
    }

    #[tokio::test]
    async fn test_remove_not_found() {
        let mut mock_repo = MockPackageRepository::new();
        let config = test_config();
        let (sender, _rx) = test_sender();
        let mut progress = ProgressTracker::new(3);

        mock_repo.expect_get_package().return_once(|_| {
            Err(crate::package::port::PackageRepoError::from(
                crate::package::port::PackageError::PackageNotFound {
                    name: "nonexistent".to_string(),
                    packages_path: PathBuf::from("/test/packages"),
                    files_examined: 0,
                    search_patterns: vec!["nonexistent.yml".to_string()],
                },
            ))
        });

        let result =
            handle_remove("nonexistent", &mock_repo, &config, &sender, &mut progress).await;

        assert!(matches!(result, OperationResult::Failure(_)));
    }

    #[tokio::test]
    async fn test_remove_with_configs_emits_cleanup_info() {
        let mut mock_repo = MockPackageRepository::new();
        let config = test_config();
        let (sender, mut rx) = test_sender();
        let mut progress = ProgressTracker::new(3);

        let package = PackageBuilder::default()
            .name("cfg-pkg")
            .version("1.0.0")
            .environment("test-env", |b| b.install("brew install cfg-pkg"))
            .configs(vec![
                ConfigEntry::new("vimrc", "~/.vimrc"),
                ConfigEntry::new("gitconfig", "~/.gitconfig"),
            ])
            .path("/test/packages/cfg-pkg.yml")
            .build();

        let get_package =
            GetPackage::from_existing(package, PathBuf::from("/test/packages/cfg-pkg.yml"));

        mock_repo
            .expect_get_package()
            .return_once(move |_| Ok(get_package));

        mock_repo
            .expect_find_dependent_packages()
            .return_once(|_| Ok(vec![]));

        mock_repo.expect_remove_package().return_once(|_| Ok(()));

        let result = handle_remove("cfg-pkg", &mock_repo, &config, &sender, &mut progress).await;

        assert!(matches!(result, OperationResult::Success(_)));

        // Check that ConfigCleanupInfo was emitted
        let mut found_cleanup_info = false;
        while let Ok(event) = rx.try_recv() {
            if let crate::package::event::PackageEvent::ConfigCleanupInfo {
                package_name,
                config_targets,
                ..
            } = event
            {
                assert_eq!(package_name, "cfg-pkg");
                assert_eq!(config_targets, vec!["~/.vimrc", "~/.gitconfig"]);
                found_cleanup_info = true;
            }
        }
        assert!(found_cleanup_info, "Expected ConfigCleanupInfo event");
    }
}
