//!
//! Handles the `spec search` operation — searches specs by keyword.
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

pub(super) async fn handle_spec_search<PR, G>(
    repo: &PR,
    config: &SelfieConfig,
    git: &G,
    sender: &EventSender,
    progress: &mut ProgressTracker,
    pattern: &str,
) -> OperationResult
where
    PR: PackageRepository,
    G: GitStatusProvider,
{
    let pattern_lower = pattern.to_lowercase();
    load_filter_emit(
        repo,
        config,
        git,
        sender,
        progress,
        SpecQueryOptions {
            load_step_label: "Loading specs",
            emit_step_label: "Searching specs",
            filter: move |pkg: &crate::package::Package| {
                let name_match = pkg.name().to_lowercase().contains(&pattern_lower);
                let desc_match = pkg
                    .description()
                    .is_some_and(|d: &str| d.to_lowercase().contains(&pattern_lower));
                name_match || desc_match
            },
            include_invalid: false,
            show_all: true, // search results span all environments
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
            event::{PackageEvent, SpecListItem},
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
            crate::package::event::metadata::OperationType::SpecSearch,
            String::new(),
            "test".to_string(),
            crate::package::event::OperationContext::default(),
        );
        (sender, rx)
    }

    fn test_packages(temp_dir: &std::path::Path) -> Vec<crate::package::Package> {
        vec![
            PackageBuilder::default()
                .name("ripgrep")
                .description("Fast line-oriented search tool")
                .environment("macos", |b| b.install("brew install ripgrep"))
                .path(temp_dir.join("ripgrep.yml"))
                .build(),
            PackageBuilder::default()
                .name("node")
                .description("JavaScript runtime")
                .environment("macos", |b| b.install("brew install node"))
                .path(temp_dir.join("node.yml"))
                .build(),
            PackageBuilder::default()
                .name("fd-find")
                .description("A simple, fast alternative to find")
                .environment("ubuntu", |b| b.install("apt install fd-find"))
                .path(temp_dir.join("fd-find.yml"))
                .build(),
        ]
    }

    fn mock_repo(packages: Vec<crate::package::Package>) -> MockPackageRepository {
        let mut mock = MockPackageRepository::new();
        mock.expect_list_packages().returning(move || {
            Ok(crate::package::port::ListPackagesOutput(
                packages.iter().cloned().map(Ok).collect(),
            ))
        });
        mock
    }

    async fn collect_items(mut rx: mpsc::Receiver<PackageEvent>) -> Vec<SpecListItem> {
        let mut items = Vec::new();
        while let Some(event) = rx.recv().await {
            if let PackageEvent::SpecListItemCompleted { spec_item, .. } = event {
                items.push(spec_item);
            }
        }
        items
    }

    #[tokio::test]
    async fn test_search_by_name() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let config = SelfieConfigBuilder::default()
            .environment("macos")
            .package_directory(temp_dir.path())
            .build();

        let mock_repo = mock_repo(test_packages(temp_dir.path()));
        let (sender, rx) = test_sender();
        let mut progress = ProgressTracker::new(2);
        let mock_git = mock_git_not_in_repo();

        let result = handle_spec_search(
            &mock_repo,
            &config,
            &mock_git,
            &sender,
            &mut progress,
            "ripgrep",
        )
        .await;
        assert!(matches!(result, OperationResult::Success(_)));

        drop(sender);
        let items = collect_items(rx).await;
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].name, "ripgrep");
    }

    #[tokio::test]
    async fn test_search_by_description() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let config = SelfieConfigBuilder::default()
            .environment("macos")
            .package_directory(temp_dir.path())
            .build();

        let mock_repo = mock_repo(test_packages(temp_dir.path()));
        let (sender, rx) = test_sender();
        let mut progress = ProgressTracker::new(2);
        let mock_git = mock_git_not_in_repo();

        let result = handle_spec_search(
            &mock_repo,
            &config,
            &mock_git,
            &sender,
            &mut progress,
            "runtime",
        )
        .await;
        assert!(matches!(result, OperationResult::Success(_)));

        drop(sender);
        let items = collect_items(rx).await;
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].name, "node");
    }

    #[tokio::test]
    async fn test_search_case_insensitive() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let config = SelfieConfigBuilder::default()
            .environment("macos")
            .package_directory(temp_dir.path())
            .build();

        let mock_repo = mock_repo(test_packages(temp_dir.path()));
        let (sender, rx) = test_sender();
        let mut progress = ProgressTracker::new(2);
        let mock_git = mock_git_not_in_repo();

        let result = handle_spec_search(
            &mock_repo,
            &config,
            &mock_git,
            &sender,
            &mut progress,
            "RIPGREP",
        )
        .await;
        assert!(matches!(result, OperationResult::Success(_)));

        drop(sender);
        let items = collect_items(rx).await;
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].name, "ripgrep");
    }

    #[tokio::test]
    async fn test_search_no_matches() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let config = SelfieConfigBuilder::default()
            .environment("macos")
            .package_directory(temp_dir.path())
            .build();

        let mock_repo = mock_repo(test_packages(temp_dir.path()));
        let (sender, rx) = test_sender();
        let mut progress = ProgressTracker::new(2);
        let mock_git = mock_git_not_in_repo();

        let result = handle_spec_search(
            &mock_repo,
            &config,
            &mock_git,
            &sender,
            &mut progress,
            "nonexistent",
        )
        .await;
        assert!(matches!(result, OperationResult::Success(_)));

        drop(sender);
        let items = collect_items(rx).await;
        assert!(items.is_empty());
    }

    #[tokio::test]
    async fn test_search_matches_across_environments() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let config = SelfieConfigBuilder::default()
            .environment("macos")
            .package_directory(temp_dir.path())
            .build();

        let mock_repo = mock_repo(test_packages(temp_dir.path()));
        let (sender, rx) = test_sender();
        let mut progress = ProgressTracker::new(2);
        let mock_git = mock_git_not_in_repo();

        // "find" matches fd-find (name) and fd-find's description
        let result = handle_spec_search(
            &mock_repo,
            &config,
            &mock_git,
            &sender,
            &mut progress,
            "find",
        )
        .await;
        assert!(matches!(result, OperationResult::Success(_)));

        drop(sender);
        let items = collect_items(rx).await;
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].name, "fd-find");
    }

    #[tokio::test]
    async fn test_search_emits_summary_without_invalid_packages() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let config = SelfieConfigBuilder::default()
            .environment("macos")
            .package_directory(temp_dir.path())
            .build();

        let mock_repo = mock_repo(test_packages(temp_dir.path()));
        let (sender, mut rx) = test_sender();
        let mut progress = ProgressTracker::new(2);
        let mock_git = mock_git_not_in_repo();

        let result = handle_spec_search(
            &mock_repo,
            &config,
            &mock_git,
            &sender,
            &mut progress,
            "node",
        )
        .await;
        assert!(matches!(result, OperationResult::Success(_)));

        drop(sender);
        let mut found_summary = false;
        while let Some(event) = rx.recv().await {
            if let PackageEvent::SpecListLoaded { spec_list, .. } = event {
                assert_eq!(spec_list.specs.len(), 1);
                assert_eq!(spec_list.specs[0].name, "node");
                assert!(spec_list.show_all);
                assert!(spec_list.invalid_packages.is_empty());
                // env_stats should reflect all packages, not just matches
                assert_eq!(spec_list.environment_stats.len(), 2);
                assert_eq!(spec_list.environment_stats["macos"], 2);
                assert_eq!(spec_list.environment_stats["ubuntu"], 1);
                found_summary = true;
            }
        }
        assert!(found_summary, "Expected SpecListLoaded event");
    }
}
