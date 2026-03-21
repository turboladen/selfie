//! ConfigService implementation
//!
//! This module provides the concrete implementation of the [`ConfigService`] trait.
//! It coordinates between the package repository (for loading package configs),
//! the file system (for reading/writing config files), and the application config
//! to perform config deployment operations.

use std::path::{Path, PathBuf};

use tokio::sync::mpsc;

use crate::{
    config::SelfieConfig,
    config_service::{
        deploy::{DeployDecision, compute_checksum, deploy_decision, resolve_source_path},
        diff::unified_diff,
        state::{DeployState, DriftType},
    },
    fs::filesystem::{FileSystem, FileSystemError},
    package::{
        event::{
            EventSender, EventStream, OperationContext, OperationResult, OperationSuccess,
            PackageEvent, StepCount, metadata::OperationType,
        },
        port::PackageRepository,
    },
};

use super::port::{ApplyOptions, ConfigService};

/// Default deploy state filename
const DEPLOY_STATE_FILENAME: &str = "deploy-state.yml";

/// Concrete implementation of the [`ConfigService`] trait
///
/// Coordinates between the package repository, file system, and application
/// configuration to deploy config files and check for drift.
#[derive(Debug, Clone)]
pub struct ConfigServiceImpl<R, F> {
    package_repository: R,
    filesystem: F,
    config: SelfieConfig,
}

impl<R, F> ConfigServiceImpl<R, F>
where
    R: PackageRepository + Clone + Send + Sync + 'static,
    F: FileSystem + Clone + Send + Sync + 'static,
{
    /// Create a new config service instance
    pub fn new(package_repository: R, filesystem: F, config: SelfieConfig) -> Self {
        Self {
            package_repository,
            filesystem,
            config,
        }
    }

    /// Create an event stream from an async operation
    fn create_event_stream<Func, Fut>(f: Func) -> EventStream
    where
        Func: FnOnce(mpsc::Sender<PackageEvent>) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = ()> + Send,
    {
        let (tx, rx) = mpsc::channel(32);

        tokio::spawn(async move {
            f(tx).await;
        });

        Box::pin(futures::stream::unfold(rx, |mut rx| async move {
            rx.recv().await.map(|event| (event, rx))
        }))
    }
}

/// Resolve the deploy state file path.
///
/// Uses the configured `state_directory` if available, otherwise resolves
/// `~/.config/selfie/deploy-state.yml` via the filesystem abstraction.
///
/// # Errors
///
/// Returns `FileSystemError` if the home directory cannot be resolved.
fn deploy_state_path<F: FileSystem>(
    filesystem: &F,
    config: &SelfieConfig,
) -> Result<PathBuf, FileSystemError> {
    // Use configured state directory if available
    if let Some(state_dir) = config.state_directory() {
        return Ok(state_dir.join(DEPLOY_STATE_FILENAME));
    }

    // expand_path canonicalizes, which fails if the directory doesn't exist yet.
    // To avoid requiring ~/.config/selfie to already exist on first run, expand
    // just "~" (which should always exist) and join the rest.
    let home = filesystem.expand_path(&PathBuf::from("~")).map_err(|_| {
        FileSystemError::IoError(std::sync::Arc::new(std::io::Error::other(
            "Cannot determine home directory for deploy state file",
        )))
    })?;
    let parent = home.join(".config").join("selfie");
    Ok(parent.join(DEPLOY_STATE_FILENAME))
}

/// Load the deploy state from disk, or return an empty state
fn load_deploy_state<F: FileSystem>(filesystem: &F, config: &SelfieConfig) -> DeployState {
    let path = match deploy_state_path(filesystem, config) {
        Ok(p) => p,
        Err(_) => return DeployState::empty(),
    };
    if !filesystem.path_exists(&path) {
        return DeployState::empty();
    }
    match filesystem.read_file(&path) {
        Ok(content) => serde_yaml::from_str(&content).unwrap_or_else(|_| DeployState::empty()),
        Err(_) => DeployState::empty(),
    }
}

/// Save the deploy state to disk
fn save_deploy_state<F: FileSystem>(
    filesystem: &F,
    config: &SelfieConfig,
    state: &DeployState,
) -> Result<(), FileSystemError> {
    let path = deploy_state_path(filesystem, config)?;
    let yaml = serde_yaml::to_string(state).map_err(|e| {
        FileSystemError::IoError(std::sync::Arc::new(std::io::Error::other(e.to_string())))
    })?;
    filesystem.write_file(&path, yaml.as_bytes())
}

/// Expand a target path, handling tilde expansion via the filesystem abstraction.
///
/// Tries `expand_path` (which does tilde expansion + canonicalize) first. If that
/// fails (e.g., target doesn't exist yet), expands just the tilde prefix using the
/// filesystem and constructs the rest of the path without requiring it to exist.
fn expand_target_path<F: FileSystem>(filesystem: &F, target: &str) -> PathBuf {
    let raw = PathBuf::from(target);

    // Try full canonicalization first (works if path exists)
    if let Ok(p) = filesystem.expand_path(&raw) {
        return p;
    }

    // For tilde paths, expand just "~" to get the home directory via filesystem
    if let Some(rest) = target.strip_prefix("~/") {
        let tilde_only = PathBuf::from("~");
        if let Ok(home) = filesystem.expand_path(&tilde_only) {
            return home.join(rest);
        }
    }

    // Try canonicalizing just the parent (works if parent dir exists)
    if let (Some(parent), Some(filename)) = (raw.parent(), raw.file_name())
        && let Ok(canonical_parent) = filesystem.expand_path(parent)
    {
        return canonical_parent.join(filename);
    }

    raw
}

/// Validate that a resolved source path doesn't escape the configs directory.
///
/// Prevents path traversal attacks where a malicious package YAML could use
/// `../` sequences to read files outside the configs directory.
///
/// Uses a component-level normalization that resolves `..` without requiring
/// the path to exist on disk (unlike `canonicalize`).
fn validate_source_path(source_path: &Path, configs_dir: &Path) -> bool {
    match (
        std::path::absolute(source_path),
        std::path::absolute(configs_dir),
    ) {
        (Ok(abs_source), Ok(abs_configs)) => {
            normalize_path(&abs_source).starts_with(&normalize_path(&abs_configs))
        }
        _ => false,
    }
}

/// Normalize a path by resolving `.` and `..` components without touching the filesystem.
///
/// Unlike `canonicalize`, this works on paths that don't exist yet. It processes
/// components left-to-right, popping on `..` and skipping `.`.
fn normalize_path(path: &Path) -> PathBuf {
    use std::path::Component;
    let mut parts: Vec<Component<'_>> = Vec::new();
    for component in path.components() {
        match component {
            Component::ParentDir => {
                // Only pop Normal components; don't pop past root/prefix
                if matches!(parts.last(), Some(Component::Normal(_))) {
                    parts.pop();
                }
            }
            Component::CurDir => {} // skip "."
            other => parts.push(other),
        }
    }
    parts.iter().collect()
}

impl<R, F> ConfigService for ConfigServiceImpl<R, F>
where
    R: PackageRepository + Clone + std::fmt::Debug + Send + Sync + 'static,
    F: FileSystem + Clone + std::fmt::Debug + Send + Sync + 'static,
{
    async fn apply_all(&self, options: ApplyOptions) -> EventStream {
        let repo = self.package_repository.clone();
        let fs = self.filesystem.clone();
        let config = self.config.clone();

        Self::create_event_stream(move |tx| async move {
            let sender = EventSender::new_with_context(
                tx,
                OperationType::ConfigApply,
                String::new(),
                config.environment().to_string(),
                OperationContext::default(),
            );

            sender.send_started().await;

            let result = handle_apply(&repo, &fs, &config, &sender, &options, None).await;

            sender.send_completed(result).await;
        })
    }

    async fn apply(&self, name: &str, options: ApplyOptions) -> EventStream {
        let repo = self.package_repository.clone();
        let fs = self.filesystem.clone();
        let config = self.config.clone();
        let name = name.to_string();

        Self::create_event_stream(move |tx| async move {
            let sender = EventSender::new_with_context(
                tx,
                OperationType::ConfigApply,
                name.clone(),
                config.environment().to_string(),
                OperationContext::default(),
            );

            sender.send_started().await;

            let result = handle_apply(&repo, &fs, &config, &sender, &options, Some(&name)).await;

            sender.send_completed(result).await;
        })
    }

    async fn check_drift(&self) -> EventStream {
        let repo = self.package_repository.clone();
        let fs = self.filesystem.clone();
        let config = self.config.clone();

        Self::create_event_stream(move |tx| async move {
            let sender = EventSender::new_with_context(
                tx,
                OperationType::ConfigDrift,
                String::new(),
                config.environment().to_string(),
                OperationContext::default(),
            );

            sender.send_started().await;

            let result = handle_check_drift(&repo, &fs, &config, &sender).await;

            sender.send_completed(result).await;
        })
    }
}

/// Describes a single config file deployment operation
struct DeployUnit<'a> {
    source_path: &'a Path,
    target_path: &'a Path,
    source_content: &'a str,
    source_checksum: &'a str,
    source_key: &'a str,
}

/// Deploy a single config file to its target path, updating state and emitting events.
async fn perform_deploy<F: FileSystem>(
    filesystem: &F,
    deploy_state: &mut DeployState,
    sender: &EventSender,
    unit: &DeployUnit<'_>,
    dry_run: bool,
) -> Result<(), ()> {
    if dry_run {
        sender
            .send_config_skipped(
                unit.source_path.display(),
                unit.target_path.display(),
                "dry run",
            )
            .await;
        return Ok(());
    }

    sender
        .send_config_deploying(unit.source_path.display(), unit.target_path.display())
        .await;

    if let Err(e) = filesystem.write_file(unit.target_path, unit.source_content.as_bytes()) {
        sender
            .send_warning(format!(
                "Failed to write '{}': {e}",
                unit.target_path.display()
            ))
            .await;
        return Err(());
    }
    deploy_state.record_deployment(
        unit.source_key,
        &unit.target_path.to_string_lossy(),
        unit.source_checksum,
    );

    sender
        .send_config_deployed(unit.source_path.display(), unit.target_path.display())
        .await;
    Ok(())
}

/// Core logic for applying config files
async fn handle_apply<R, F>(
    repo: &R,
    filesystem: &F,
    config: &SelfieConfig,
    sender: &EventSender,
    options: &ApplyOptions,
    filter_name: Option<&str>,
) -> OperationResult
where
    R: PackageRepository,
    F: FileSystem,
{
    // Load packages
    let packages = match repo.list_packages() {
        Ok(output) => output.valid_packages().cloned().collect::<Vec<_>>(),
        Err(e) => {
            return OperationResult::Failure(crate::package::event::OperationFailure::Generic(
                format!("Failed to load packages: {e}"),
            ));
        }
    };

    let configs_dir = config.configs_directory();
    let mut deploy_state = load_deploy_state(filesystem, config);

    let mut deployed_count: usize = 0;
    let mut skipped_count: usize = 0;
    let mut conflict_count: usize = 0;

    for package in &packages {
        // If filtering by name, skip non-matching packages
        if let Some(name) = filter_name
            && package.name() != name
        {
            continue;
        }

        let configs = package.configs();
        if configs.is_empty() {
            continue;
        }

        for entry in configs {
            let source_path = resolve_source_path(&configs_dir, entry.source());

            // Runtime path traversal guard: verify resolved path stays within configs_dir
            if !validate_source_path(&source_path, &configs_dir) {
                sender
                    .send_warning(format!(
                        "Skipping '{}': source path escapes configs directory",
                        entry.source()
                    ))
                    .await;
                skipped_count += 1;
                continue;
            }

            let target_path = expand_target_path(filesystem, entry.target());

            // Read source file
            let source_content = match filesystem.read_file(&source_path) {
                Ok(content) => content,
                Err(e) => {
                    sender
                        .send_warning(format!(
                            "Cannot read source '{}': {e}",
                            source_path.display()
                        ))
                        .await;
                    skipped_count += 1;
                    continue;
                }
            };

            let source_checksum = compute_checksum(source_content.as_bytes());
            let target_exists = filesystem.path_exists(&target_path);

            // Read target if exists
            let target_checksum = if target_exists {
                match filesystem.read_file(&target_path) {
                    Ok(content) => compute_checksum(content.as_bytes()),
                    Err(_) => String::new(),
                }
            } else {
                String::new()
            };

            // Detect drift
            let drift =
                deploy_state.detect_drift(entry.source(), &source_checksum, &target_checksum);
            let decision = deploy_decision(&drift, target_exists);

            let unit = DeployUnit {
                source_path: &source_path,
                target_path: &target_path,
                source_content: &source_content,
                source_checksum: &source_checksum,
                source_key: entry.source(),
            };

            match decision {
                DeployDecision::Deploy => {
                    if perform_deploy(
                        filesystem,
                        &mut deploy_state,
                        sender,
                        &unit,
                        options.dry_run,
                    )
                    .await
                    .is_ok()
                    {
                        deployed_count += 1;
                    } else {
                        skipped_count += 1;
                    }
                }
                DeployDecision::Skip(reason) => {
                    sender
                        .send_config_skipped(source_path.display(), target_path.display(), &reason)
                        .await;
                    skipped_count += 1;
                }
                DeployDecision::Conflict => {
                    if options.auto_accept {
                        if perform_deploy(
                            filesystem,
                            &mut deploy_state,
                            sender,
                            &unit,
                            options.dry_run,
                        )
                        .await
                        .is_ok()
                        {
                            deployed_count += 1;
                        } else {
                            skipped_count += 1;
                        }
                    } else {
                        // Emit conflict with diff
                        let target_content = filesystem.read_file(&target_path).unwrap_or_default();
                        let diff = unified_diff(
                            &target_content,
                            &source_content,
                            &target_path.to_string_lossy(),
                            &source_path.to_string_lossy(),
                        );
                        sender
                            .send_config_conflict(
                                source_path.display(),
                                target_path.display(),
                                &diff,
                            )
                            .await;
                        conflict_count += 1;
                    }
                }
            }
        }
    }

    // Save deploy state (skip in dry-run mode)
    if !options.dry_run
        && let Err(e) = save_deploy_state(filesystem, config, &deploy_state)
    {
        sender
            .send_warning(format!("Failed to save deploy state: {e}"))
            .await;
    }

    let total = deployed_count + skipped_count + conflict_count;
    OperationResult::Success(OperationSuccess::ConfigApplied {
        deployed_count,
        skipped_count,
        conflict_count,
        environment: config.environment().to_string(),
        steps_completed: StepCount::new(total, total),
    })
}

/// Core logic for checking drift
async fn handle_check_drift<R, F>(
    repo: &R,
    filesystem: &F,
    config: &SelfieConfig,
    sender: &EventSender,
) -> OperationResult
where
    R: PackageRepository,
    F: FileSystem,
{
    let configs_dir = config.configs_directory();
    let deploy_state = load_deploy_state(filesystem, config);

    // Also scan packages to find all config entries
    let packages = match repo.list_packages() {
        Ok(output) => output.valid_packages().cloned().collect::<Vec<_>>(),
        Err(e) => {
            return OperationResult::Failure(crate::package::event::OperationFailure::Generic(
                format!("Failed to load packages: {e}"),
            ));
        }
    };

    let mut drift_count: usize = 0;
    let mut total_count: usize = 0;

    for package in &packages {
        for entry in package.configs() {
            total_count += 1;

            let source_path = resolve_source_path(&configs_dir, entry.source());
            let target_path = expand_target_path(filesystem, entry.target());

            // Runtime path traversal guard (same as handle_apply)
            if !validate_source_path(&source_path, &configs_dir) {
                sender
                    .send_warning(format!(
                        "Skipping '{}': source path escapes configs directory",
                        entry.source()
                    ))
                    .await;
                continue;
            }

            // Read source — emit warning if missing instead of silently skipping
            let source_content = match filesystem.read_file(&source_path) {
                Ok(content) => content,
                Err(e) => {
                    sender
                        .send_warning(format!(
                            "Cannot read source '{}' for drift check: {e}",
                            source_path.display()
                        ))
                        .await;
                    continue;
                }
            };
            let source_checksum = compute_checksum(source_content.as_bytes());

            // Read target
            let target_checksum = if filesystem.path_exists(&target_path) {
                match filesystem.read_file(&target_path) {
                    Ok(content) => compute_checksum(content.as_bytes()),
                    Err(_) => String::new(),
                }
            } else {
                String::new()
            };

            let drift =
                deploy_state.detect_drift(entry.source(), &source_checksum, &target_checksum);

            if drift != DriftType::None {
                sender
                    .send_config_drift_detected(target_path.display(), &drift)
                    .await;
                drift_count += 1;
            }
        }
    }

    OperationResult::Success(OperationSuccess::ConfigDriftChecked {
        drift_count,
        total_count,
        environment: config.environment().to_string(),
        steps_completed: StepCount::new(total_count, total_count),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fs::RealFileSystem;
    use std::path::Path;

    #[test]
    fn test_validate_source_path_valid() {
        assert!(validate_source_path(
            Path::new("/configs/app/file.toml"),
            Path::new("/configs")
        ));
    }

    #[test]
    fn test_validate_source_path_traversal() {
        assert!(!validate_source_path(
            Path::new("/configs/../etc/passwd"),
            Path::new("/configs")
        ));
    }

    #[test]
    fn test_validate_source_path_within_nested() {
        assert!(validate_source_path(
            Path::new("/configs/deep/nested/file"),
            Path::new("/configs")
        ));
    }

    #[test]
    fn test_expand_target_path_absolute() {
        let fs = RealFileSystem;
        let result = expand_target_path(&fs, "/tmp/some/file");
        // Should be an absolute path starting with /tmp
        assert!(result.is_absolute());
        assert!(result.starts_with("/tmp"));
    }

    #[test]
    fn test_expand_target_path_tilde() {
        let fs = RealFileSystem;
        let result = expand_target_path(&fs, "~/test-file");
        // Should start with the actual home directory, not literal "~"
        assert!(result.is_absolute());
        assert!(
            !result.starts_with("~"),
            "Tilde should be expanded to actual home directory"
        );
        assert!(
            result.to_string_lossy().contains("test-file"),
            "Should preserve the filename after tilde expansion"
        );
    }
}
