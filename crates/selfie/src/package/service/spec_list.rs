//!
//! Handles the `spec list` operation — lists package definitions without runtime checks.
//!

use std::collections::HashMap;

use crate::{
    config::SelfieConfig,
    package::{
        event::{
            EventSender, InvalidPackageInfo, OperationResult, OperationSuccess, SpecListData,
            SpecListItem,
        },
        port::PackageRepository,
        service::ProgressTracker,
    },
};

pub(super) async fn handle_spec_list<PR>(
    repo: &PR,
    config: &SelfieConfig,
    sender: &EventSender,
    progress: &mut ProgressTracker,
    show_all: bool,
) -> OperationResult
where
    PR: PackageRepository,
{
    // Step 1: Load and process packages
    progress.next(sender, "Loading package definitions").await;

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

    // Sort alphabetically
    let mut sorted_packages: Vec<_> = valid_packages.into_iter().collect();
    sorted_packages.sort_by(|a, b| a.name().cmp(b.name()));

    // Calculate environment statistics from all valid packages (before filtering)
    let mut environment_stats: HashMap<String, usize> = HashMap::new();
    for package in &sorted_packages {
        for env_name in package.environments().keys() {
            *environment_stats.entry(env_name.clone()).or_insert(0) += 1;
        }
    }

    // Filter by current environment unless show_all
    let packages_to_show: Vec<_> = sorted_packages
        .into_iter()
        .filter(|package| show_all || package.environments().contains_key(config.environment()))
        .collect();

    // Step 2: Emit individual items and summary
    progress.next(sender, "Emitting spec definitions").await;

    let mut spec_items = Vec::new();
    for package in &packages_to_show {
        let item = SpecListItem {
            name: package.name().to_string(),
            version: package.version().to_string(),
            description: package.description().map(String::from),
            environments: package.environments().keys().cloned().collect(),
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
        show_all,
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
        package::{PackageBuilder, event::PackageEvent, port::MockPackageRepository},
    };
    use tokio::sync::mpsc;

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
                .version("1.0.0")
                .environment("macos", |b| b.install("brew install ripgrep"))
                .path(temp_dir.path().join("ripgrep.yml"))
                .build(),
            PackageBuilder::default()
                .name("apt-tool")
                .version("1.0.0")
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

        let result = handle_spec_list(&mock_repo, &config, &sender, &mut progress, false).await;

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
                .version("1.0.0")
                .environment("macos", |b| b.install("brew install ripgrep"))
                .path(temp_dir.path().join("ripgrep.yml"))
                .build(),
            PackageBuilder::default()
                .name("apt-tool")
                .version("1.0.0")
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

        let result = handle_spec_list(&mock_repo, &config, &sender, &mut progress, true).await;

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
                .version("20.0.0")
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

        let result = handle_spec_list(&mock_repo, &config, &sender, &mut progress, false).await;

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
