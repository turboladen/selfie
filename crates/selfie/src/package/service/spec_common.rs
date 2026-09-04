//!
//! Shared logic for spec listing and searching operations.
//!

use std::collections::HashMap;

use crate::{
    config::SelfieConfig,
    package::{
        Package,
        event::{EventSender, OperationResult, OperationSuccess, SpecListData, SpecListItem},
        git::GitStatusProvider,
        port::PackageRepository,
        service::ProgressTracker,
    },
};

/// Options controlling how packages are loaded, filtered, and emitted.
pub(super) struct SpecQueryOptions<'a, F> {
    /// Human-readable label for step 1 progress (e.g., "Loading specs")
    pub load_step_label: &'a str,
    /// Human-readable label for step 2 progress (e.g., "Emitting spec definitions")
    pub emit_step_label: &'a str,
    /// Predicate that decides which valid packages to include in results
    pub filter: F,
    /// Whether to include invalid packages in the summary event
    pub include_invalid: bool,
    /// Value for `show_all` in the emitted `SpecListData`
    pub show_all: bool,
}

/// Load packages, filter them, emit events, and return the operation result.
///
/// This is the shared core of `spec list` and `spec search`. The caller provides
/// a filter predicate and display options; this function handles everything else.
pub(super) async fn load_filter_emit<PR, G, F>(
    repo: &PR,
    config: &SelfieConfig,
    git: &G,
    sender: &EventSender,
    progress: &mut ProgressTracker,
    opts: SpecQueryOptions<'_, F>,
) -> OperationResult
where
    PR: PackageRepository,
    G: GitStatusProvider,
    F: Fn(&Package) -> bool,
{
    // Step 1: Load and process packages
    progress.next(sender, opts.load_step_label).await;

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

    // Apply the caller's filter
    let packages_to_show: Vec<_> = sorted_packages
        .into_iter()
        .filter(|pkg| (opts.filter)(pkg))
        .collect();

    // Look up git status for the package directory (once for all files),
    // but only if there are packages to annotate.
    let git_dir_status = if packages_to_show.is_empty() {
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

    // Step 2: Emit individual items and summary
    progress.next(sender, opts.emit_step_label).await;

    let mut spec_items = Vec::new();
    for package in &packages_to_show {
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

    let invalid_package_items: Vec<crate::package::port::PackageParseError> =
        if opts.include_invalid {
            invalid_packages.iter().map(|ip| (*ip).clone()).collect()
        } else {
            Vec::new()
        };

    let valid_count = spec_items.len();
    let invalid_count = invalid_package_items.len();

    let spec_list_data = SpecListData {
        specs: spec_items,
        invalid_packages: invalid_package_items,
        current_environment: config.environment().to_string(),
        package_directory: config.package_directory().display().to_string(),
        environment_stats,
        show_all: opts.show_all,
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
            PackageBuilder,
            event::{OperationSuccess, PackageEvent},
            git::MockGitStatusProvider,
            port::{MockPackageRepository, PackageListError},
        },
    };
    use std::path::PathBuf;
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

    fn make_opts<F: Fn(&Package) -> bool>(filter: F) -> SpecQueryOptions<'static, F> {
        SpecQueryOptions {
            load_step_label: "Loading",
            emit_step_label: "Emitting",
            filter,
            include_invalid: true,
            show_all: false,
        }
    }

    fn make_invalid_package(path: PathBuf) -> crate::package::port::PackageParseError {
        crate::package::port::PackageParseError::new(
            path,
            crate::package::port::PackageParseKind::Io {
                source: std::sync::Arc::new(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "file not found",
                )),
            },
        )
    }

    #[tokio::test]
    async fn test_env_stats_computed_before_filtering() {
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

        // Filter to only macos packages
        let result = load_filter_emit(
            &mock_repo,
            &config,
            &mock_git,
            &sender,
            &mut progress,
            make_opts(|pkg: &Package| pkg.environments().contains_key("macos")),
        )
        .await;

        assert!(matches!(result, OperationResult::Success(_)));

        drop(sender);
        while let Some(event) = rx.recv().await {
            if let PackageEvent::SpecListLoaded { spec_list, .. } = event {
                // Only 1 package shown, but stats reflect both environments
                assert_eq!(spec_list.specs.len(), 1);
                assert_eq!(spec_list.environment_stats.len(), 2);
                assert_eq!(spec_list.environment_stats["macos"], 1);
                assert_eq!(spec_list.environment_stats["ubuntu"], 1);
                return;
            }
        }
        panic!("Expected SpecListLoaded event");
    }

    #[tokio::test]
    async fn test_include_invalid_false_omits_invalid_packages() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let config = SelfieConfigBuilder::default()
            .environment("macos")
            .package_directory(temp_dir.path())
            .build();

        let valid_pkg = PackageBuilder::default()
            .name("ripgrep")
            .environment("macos", |b| b.install("brew install ripgrep"))
            .path(temp_dir.path().join("ripgrep.yml"))
            .build();

        let invalid_path = temp_dir.path().join("broken.yml");
        let mut mock_repo = MockPackageRepository::new();
        mock_repo.expect_list_packages().returning(move || {
            Ok(crate::package::port::ListPackagesOutput(vec![
                Ok(valid_pkg.clone()),
                Err(make_invalid_package(invalid_path.clone())),
            ]))
        });

        let (sender, mut rx) = test_sender();
        let mut progress = ProgressTracker::new(2);
        let mock_git = mock_git_not_in_repo();

        let result = load_filter_emit(
            &mock_repo,
            &config,
            &mock_git,
            &sender,
            &mut progress,
            SpecQueryOptions {
                load_step_label: "Loading",
                emit_step_label: "Emitting",
                filter: |_: &Package| true,
                include_invalid: false,
                show_all: false,
            },
        )
        .await;

        assert!(matches!(
            result,
            OperationResult::Success(OperationSuccess::SpecListGenerated {
                invalid_count: 0,
                ..
            })
        ));

        drop(sender);
        while let Some(event) = rx.recv().await {
            if let PackageEvent::SpecListLoaded { spec_list, .. } = event {
                assert!(spec_list.invalid_packages.is_empty());
                return;
            }
        }
        panic!("Expected SpecListLoaded event");
    }

    #[tokio::test]
    async fn test_include_invalid_true_includes_invalid_packages() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let config = SelfieConfigBuilder::default()
            .environment("macos")
            .package_directory(temp_dir.path())
            .build();

        let invalid_path = temp_dir.path().join("broken.yml");
        let mut mock_repo = MockPackageRepository::new();
        mock_repo.expect_list_packages().returning(move || {
            Ok(crate::package::port::ListPackagesOutput(vec![Err(
                make_invalid_package(invalid_path.clone()),
            )]))
        });

        let (sender, mut rx) = test_sender();
        let mut progress = ProgressTracker::new(2);
        let mock_git = mock_git_not_in_repo();

        let result = load_filter_emit(
            &mock_repo,
            &config,
            &mock_git,
            &sender,
            &mut progress,
            make_opts(|_: &Package| true),
        )
        .await;

        assert!(matches!(
            result,
            OperationResult::Success(OperationSuccess::SpecListGenerated {
                invalid_count: 1,
                ..
            })
        ));

        drop(sender);
        while let Some(event) = rx.recv().await {
            if let PackageEvent::SpecListLoaded { spec_list, .. } = event {
                assert_eq!(spec_list.invalid_packages.len(), 1);
                return;
            }
        }
        panic!("Expected SpecListLoaded event");
    }

    #[tokio::test]
    async fn test_results_sorted_alphabetically() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let config = SelfieConfigBuilder::default()
            .environment("macos")
            .package_directory(temp_dir.path())
            .build();

        let packages = vec![
            PackageBuilder::default()
                .name("zsh")
                .environment("macos", |b| b.install("brew install zsh"))
                .path(temp_dir.path().join("zsh.yml"))
                .build(),
            PackageBuilder::default()
                .name("awk")
                .environment("macos", |b| b.install("brew install awk"))
                .path(temp_dir.path().join("awk.yml"))
                .build(),
            PackageBuilder::default()
                .name("make")
                .environment("macos", |b| b.install("brew install make"))
                .path(temp_dir.path().join("make.yml"))
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

        let _ = load_filter_emit(
            &mock_repo,
            &config,
            &mock_git,
            &sender,
            &mut progress,
            make_opts(|_: &Package| true),
        )
        .await;

        drop(sender);
        let mut items = Vec::new();
        while let Some(event) = rx.recv().await {
            if let PackageEvent::SpecListItemCompleted { spec_item, .. } = event {
                items.push(spec_item.name);
            }
        }

        assert_eq!(items, vec!["awk", "make", "zsh"]);
    }

    #[tokio::test]
    async fn test_repo_error_returns_failure() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let config = SelfieConfigBuilder::default()
            .environment("macos")
            .package_directory(temp_dir.path())
            .build();

        let mut mock_repo = MockPackageRepository::new();
        mock_repo.expect_list_packages().returning(|| {
            Err(PackageListError::PackageDirectoryNotFound(PathBuf::from(
                "/nonexistent",
            )))
        });

        let (sender, _rx) = test_sender();
        let mut progress = ProgressTracker::new(2);
        let mock_git = mock_git_not_in_repo();

        let result = load_filter_emit(
            &mock_repo,
            &config,
            &mock_git,
            &sender,
            &mut progress,
            make_opts(|_: &Package| true),
        )
        .await;

        assert!(matches!(result, OperationResult::Failure(_)));
    }

    #[tokio::test]
    async fn test_show_all_propagated_to_summary() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let config = SelfieConfigBuilder::default()
            .environment("macos")
            .package_directory(temp_dir.path())
            .build();

        let mut mock_repo = MockPackageRepository::new();
        mock_repo
            .expect_list_packages()
            .returning(|| Ok(crate::package::port::ListPackagesOutput(vec![])));

        let (sender, mut rx) = test_sender();
        let mut progress = ProgressTracker::new(2);
        let mock_git = mock_git_not_in_repo();

        let _ = load_filter_emit(
            &mock_repo,
            &config,
            &mock_git,
            &sender,
            &mut progress,
            SpecQueryOptions {
                load_step_label: "Loading",
                emit_step_label: "Emitting",
                filter: |_: &Package| true,
                include_invalid: true,
                show_all: true,
            },
        )
        .await;

        drop(sender);
        while let Some(event) = rx.recv().await {
            if let PackageEvent::SpecListLoaded { spec_list, .. } = event {
                assert!(spec_list.show_all);
                return;
            }
        }
        panic!("Expected SpecListLoaded event");
    }

    #[tokio::test]
    async fn test_filter_excludes_non_matching_packages() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let config = SelfieConfigBuilder::default()
            .environment("macos")
            .package_directory(temp_dir.path())
            .build();

        let packages = vec![
            PackageBuilder::default()
                .name("include-me")
                .environment("macos", |b| b.install("brew install include-me"))
                .path(temp_dir.path().join("include-me.yml"))
                .build(),
            PackageBuilder::default()
                .name("skip-me")
                .environment("macos", |b| b.install("brew install skip-me"))
                .path(temp_dir.path().join("skip-me.yml"))
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

        let _ = load_filter_emit(
            &mock_repo,
            &config,
            &mock_git,
            &sender,
            &mut progress,
            make_opts(|pkg: &Package| pkg.name().starts_with("include")),
        )
        .await;

        drop(sender);
        let mut items = Vec::new();
        while let Some(event) = rx.recv().await {
            if let PackageEvent::SpecListItemCompleted { spec_item, .. } = event {
                items.push(spec_item.name);
            }
        }

        assert_eq!(items, vec!["include-me"]);
    }

    #[tokio::test]
    async fn test_git_status_not_queried_when_no_packages_match() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let config = SelfieConfigBuilder::default()
            .environment("macos")
            .package_directory(temp_dir.path())
            .build();

        let packages = vec![
            PackageBuilder::default()
                .name("something")
                .environment("macos", |b| b.install("brew install something"))
                .path(temp_dir.path().join("something.yml"))
                .build(),
        ];

        let mut mock_repo = MockPackageRepository::new();
        let packages_clone = packages.clone();
        mock_repo.expect_list_packages().returning(move || {
            Ok(crate::package::port::ListPackagesOutput(
                packages_clone.iter().cloned().map(Ok).collect(),
            ))
        });

        let (sender, _rx) = test_sender();
        let mut progress = ProgressTracker::new(2);

        // Git mock expects NO calls to status_for_directory
        let mock_git = MockGitStatusProvider::new();

        let result = load_filter_emit(
            &mock_repo,
            &config,
            &mock_git,
            &sender,
            &mut progress,
            make_opts(|_: &Package| false), // filter rejects everything
        )
        .await;

        assert!(matches!(result, OperationResult::Success(_)));
        // If git was called, MockGitStatusProvider would panic (no expectation set)
    }
}
