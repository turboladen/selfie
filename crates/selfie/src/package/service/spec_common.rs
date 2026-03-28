//!
//! Shared logic for spec listing and searching operations.
//!

use std::collections::HashMap;

use crate::{
    config::SelfieConfig,
    package::{
        Package,
        event::{
            EventSender, InvalidPackageInfo, OperationResult, OperationSuccess, SpecListData,
            SpecListItem,
        },
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

    let invalid_package_items: Vec<InvalidPackageInfo> = if opts.include_invalid {
        invalid_packages
            .iter()
            .map(|ip| InvalidPackageInfo {
                path: ip.package_path().display().to_string(),
                error: ip.to_string(),
            })
            .collect()
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
