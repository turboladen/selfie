//!
//! Handles the `spec search` operation — searches specs by keyword.
//!

use std::collections::HashMap;

use crate::{
    config::SelfieConfig,
    package::{
        event::{
            EventSender, InvalidPackageInfo, OperationResult, OperationSuccess, SpecListData,
            SpecListItem,
        },
        git::GitStatusProvider,
        port::PackageRepository,
        service::ProgressTracker,
    },
};

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
    // Step 1: Load packages
    progress.next(sender, "Loading specs").await;

    let list_output = match repo.list_packages() {
        Ok(output) => output,
        Err(err) => {
            return OperationResult::Failure(err.into());
        }
    };

    let valid_packages: Vec<_> = list_output.valid_packages().collect();
    let invalid_packages: Vec<_> = list_output.invalid_packages().collect();

    // Step 2: Filter by search pattern (case-insensitive substring)
    progress.next(sender, "Searching specs").await;

    let pattern_lower = pattern.to_lowercase();
    let mut matching: Vec<_> = valid_packages
        .into_iter()
        .filter(|pkg| {
            let name_match = pkg.name().to_lowercase().contains(&pattern_lower);
            let desc_match = pkg
                .description()
                .is_some_and(|d| d.to_lowercase().contains(&pattern_lower));
            name_match || desc_match
        })
        .collect();

    matching.sort_by(|a, b| a.name().cmp(b.name()));

    // Calculate environment statistics from matches
    let mut environment_stats: HashMap<String, usize> = HashMap::new();
    for package in &matching {
        for env_name in package.environments().keys() {
            *environment_stats.entry(env_name.clone()).or_insert(0) += 1;
        }
    }

    // Git status lookup
    let git_dir_status = if matching.is_empty() {
        None
    } else {
        match git.status_for_directory(config.package_directory()) {
            Ok(status) => Some(status),
            Err(e) => {
                sender
                    .send_warning(format!("Git status unavailable: {e}"))
                    .await;
                None
            }
        }
    };

    // Emit results
    let mut spec_items = Vec::new();
    for package in &matching {
        let file_git_status = git_dir_status
            .as_ref()
            .map(|s| s.status_for_file(package.path()));
        let item = SpecListItem {
            name: package.name().to_string(),
            description: package.description().map(String::from),
            environments: package.environments().keys().cloned().collect(),
            git_status: file_git_status,
        };
        sender.send_spec_list_item(item.clone()).await;
        spec_items.push(item);
    }

    let invalid_package_items: Vec<InvalidPackageInfo> = invalid_packages
        .iter()
        .map(|ip| InvalidPackageInfo {
            path: ip.package_path().display().to_string(),
            error: ip.to_string(),
        })
        .collect();

    let valid_count = spec_items.len();
    let invalid_count = invalid_package_items.len();

    let spec_list_data = SpecListData {
        specs: spec_items,
        invalid_packages: invalid_package_items,
        current_environment: config.environment().to_string(),
        package_directory: config.package_directory().display().to_string(),
        environment_stats,
        show_all: true, // search results span all environments
    };

    sender.send_spec_list(spec_list_data).await;

    OperationResult::Success(OperationSuccess::spec_list_generated(
        valid_count,
        invalid_count,
        config.environment().to_string(),
        (progress.current_step(), progress.total_steps()).into(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::SelfieConfigBuilder,
        package::{
            PackageBuilder, event::PackageEvent, git::MockGitStatusProvider,
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
}
