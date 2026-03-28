//!
//! Handles the `spec list` operation — lists specs without runtime checks.
//!

use crate::{
    config::SelfieConfig,
    package::{
        event::{EventSender, OperationResult},
        git::GitStatusProvider,
        port::PackageRepository,
        service::ProgressTracker,
    },
};

use super::spec_common::{SpecQueryOptions, load_filter_emit};

pub(super) async fn handle_spec_list<PR, G>(
    repo: &PR,
    config: &SelfieConfig,
    git: &G,
    sender: &EventSender,
    progress: &mut ProgressTracker,
    show_all: bool,
) -> OperationResult
where
    PR: PackageRepository,
    G: GitStatusProvider,
{
    let environment = config.environment().to_string();
    load_filter_emit(
        repo,
        config,
        git,
        sender,
        progress,
        SpecQueryOptions {
            load_step_label: "Loading specs",
            emit_step_label: "Emitting spec definitions",
            filter: move |pkg: &crate::package::Package| {
                show_all || pkg.environments().contains_key(&environment)
            },
            include_invalid: true,
            show_all,
        },
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::SelfieConfigBuilder,
        package::{
            PackageBuilder,
            event::{OperationSuccess, PackageEvent},
            git::MockGitStatusProvider,
            port::MockPackageRepository,
        },
    };
    use tokio::sync::mpsc;

    fn mock_git_not_in_repo() -> MockGitStatusProvider {
        let mut mock = MockGitStatusProvider::new();
        mock.expect_status_for_directory().returning(|_| {
            Ok(crate::package::git::GitDirectoryStatus {
                in_repo: false,
                files: std::collections::HashMap::new(),
            })
        });
        mock
    }

    fn test_sender() -> (EventSender, mpsc::Receiver<PackageEvent>) {
        let (tx, rx) = mpsc::channel(256);
        let sender = EventSender::new_with_context(
            tx,
            crate::package::event::metadata::OperationType::SpecList,
            String::new(),
            "test".to_string(),
            crate::package::event::OperationContext::default(),
        );
        (sender, rx)
    }

    #[tokio::test]
    async fn test_spec_list_filters_by_environment() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let config = SelfieConfigBuilder::default()
            .environment("macos")
            .package_directory(temp_dir.path())
            .build();

        let packages = vec![
            PackageBuilder::default()
                .name("ripgrep")
                .environment("macos", |b| b.install("brew install ripgrep"))
                .path(temp_dir.path().join("ripgrep.yml"))
                .build(),
            PackageBuilder::default()
                .name("apt-tool")
                .environment("ubuntu", |b| b.install("apt install apt-tool"))
                .path(temp_dir.path().join("apt-tool.yml"))
                .build(),
        ];

        let mut mock_repo = MockPackageRepository::new();
        let packages_clone = packages.clone();
        mock_repo.expect_list_packages().returning(move || {
            Ok(crate::package::port::ListPackagesOutput(
                packages_clone.iter().cloned().map(Ok).collect(),
            ))
        });

        let (sender, mut rx) = test_sender();
        let mut progress = ProgressTracker::new(2);

        let mock_git = mock_git_not_in_repo();
        let result = handle_spec_list(
            &mock_repo,
            &config,
            &mock_git,
            &sender,
            &mut progress,
            false,
        )
        .await;

        assert!(matches!(result, OperationResult::Success(_)));

        // Collect spec list item events
        drop(sender);
        let mut items = Vec::new();
        while let Some(event) = rx.recv().await {
            if let PackageEvent::SpecListItemCompleted { spec_item, .. } = event {
                items.push(spec_item);
            }
        }

        // Only macos package should be listed
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].name, "ripgrep");
    }

    #[tokio::test]
    async fn test_spec_list_show_all() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let config = SelfieConfigBuilder::default()
            .environment("macos")
            .package_directory(temp_dir.path())
            .build();

        let packages = vec![
            PackageBuilder::default()
                .name("ripgrep")
                .environment("macos", |b| b.install("brew install ripgrep"))
                .path(temp_dir.path().join("ripgrep.yml"))
                .build(),
            PackageBuilder::default()
                .name("apt-tool")
                .environment("ubuntu", |b| b.install("apt install apt-tool"))
                .path(temp_dir.path().join("apt-tool.yml"))
                .build(),
        ];

        let mut mock_repo = MockPackageRepository::new();
        let packages_clone = packages.clone();
        mock_repo.expect_list_packages().returning(move || {
            Ok(crate::package::port::ListPackagesOutput(
                packages_clone.iter().cloned().map(Ok).collect(),
            ))
        });

        let (sender, mut rx) = test_sender();
        let mut progress = ProgressTracker::new(2);

        let mock_git = mock_git_not_in_repo();
        let result =
            handle_spec_list(&mock_repo, &config, &mock_git, &sender, &mut progress, true).await;

        assert!(matches!(result, OperationResult::Success(_)));

        drop(sender);
        let mut items = Vec::new();
        while let Some(event) = rx.recv().await {
            if let PackageEvent::SpecListItemCompleted { spec_item, .. } = event {
                items.push(spec_item);
            }
        }

        // Both packages should be listed
        assert_eq!(items.len(), 2);
        // Alphabetical order
        assert_eq!(items[0].name, "apt-tool");
        assert_eq!(items[1].name, "ripgrep");
    }

    #[tokio::test]
    async fn test_spec_list_emits_summary() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let config = SelfieConfigBuilder::default()
            .environment("macos")
            .package_directory(temp_dir.path())
            .build();

        let packages = vec![
            PackageBuilder::default()
                .name("node")
                .environment("macos", |b| b.install("brew install node"))
                .path(temp_dir.path().join("node.yml"))
                .build(),
        ];

        let mut mock_repo = MockPackageRepository::new();
        let packages_clone = packages.clone();
        mock_repo.expect_list_packages().returning(move || {
            Ok(crate::package::port::ListPackagesOutput(
                packages_clone.iter().cloned().map(Ok).collect(),
            ))
        });

        let (sender, mut rx) = test_sender();
        let mut progress = ProgressTracker::new(2);

        let mock_git = mock_git_not_in_repo();
        let result = handle_spec_list(
            &mock_repo,
            &config,
            &mock_git,
            &sender,
            &mut progress,
            false,
        )
        .await;

        assert!(matches!(
            result,
            OperationResult::Success(OperationSuccess::SpecListGenerated { valid_count: 1, .. })
        ));

        drop(sender);
        let mut found_summary = false;
        while let Some(event) = rx.recv().await {
            if let PackageEvent::SpecListLoaded { spec_list, .. } = event {
                assert_eq!(spec_list.specs.len(), 1);
                assert_eq!(spec_list.current_environment, "macos");
                found_summary = true;
            }
        }
        assert!(found_summary, "Expected SpecListLoaded event");
    }
}
