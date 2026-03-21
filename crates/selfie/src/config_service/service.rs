//! ConfigService implementation
//!
//! This module provides the concrete implementation of the [`ConfigService`] trait.
//! It coordinates between the package repository (for loading package configs),
//! the file system (for reading/writing config files), and the application config
//! to perform config deployment operations.

use std::path::PathBuf;

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

/// Default path for deploy state file (relative to home)
const DEPLOY_STATE_RELATIVE_PATH: &str = ".config/selfie/deploy-state.yml";

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

/// Resolve the deploy state file path
fn deploy_state_path<F: FileSystem>(filesystem: &F, config: &SelfieConfig) -> PathBuf {
    // Use configured state directory if available
    if let Some(state_dir) = config.state_directory() {
        return state_dir.join(DEPLOY_STATE_FILENAME);
    }

    let tilde_path = PathBuf::from("~").join(DEPLOY_STATE_RELATIVE_PATH);
    filesystem
        .expand_path(&tilde_path)
        .unwrap_or_else(|_| PathBuf::from("/tmp/selfie-deploy-state.yml"))
}

/// Load the deploy state from disk, or return an empty state
fn load_deploy_state<F: FileSystem>(filesystem: &F, config: &SelfieConfig) -> DeployState {
    let path = deploy_state_path(filesystem, config);
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
    let path = deploy_state_path(filesystem, config);
    let yaml = serde_yaml::to_string(state).map_err(|e| {
        FileSystemError::IoError(std::sync::Arc::new(std::io::Error::other(e.to_string())))
    })?;
    filesystem.write_file(&path, yaml.as_bytes())
}

/// Expand a target path, handling tilde expansion.
///
/// Tries `expand_path` (which canonicalizes) first. If that fails (e.g., the target
/// doesn't exist yet), falls back to canonicalizing the parent directory and appending
/// the filename. If even that fails, does simple tilde replacement.
fn expand_target_path<F: FileSystem>(filesystem: &F, target: &str) -> PathBuf {
    let raw = PathBuf::from(target);

    // Try full canonicalization first (works if path exists)
    if let Ok(p) = filesystem.expand_path(&raw) {
        return p;
    }

    // Try canonicalizing just the parent (works if parent dir exists)
    if let (Some(parent), Some(filename)) = (raw.parent(), raw.file_name()) {
        // Expand tilde in parent first
        let expanded_parent = if target.starts_with("~/") {
            if let Ok(home) = std::env::var("HOME") {
                PathBuf::from(format!("{}{}", home, &parent.to_string_lossy()[1..]))
            } else {
                parent.to_path_buf()
            }
        } else {
            parent.to_path_buf()
        };

        if let Ok(canonical_parent) = filesystem.canonicalize(&expanded_parent) {
            return canonical_parent.join(filename);
        }

        // Parent doesn't exist either — return the tilde-expanded path
        return expanded_parent.join(filename);
    }

    // Last resort: simple tilde replacement
    if target.starts_with("~/")
        && let Ok(home) = std::env::var("HOME")
    {
        return PathBuf::from(format!("{}{}", home, &target[1..]));
    }
    raw
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

            match decision {
                DeployDecision::Deploy => {
                    sender
                        .send_config_deploying(source_path.display(), target_path.display())
                        .await;

                    if !options.dry_run {
                        if let Err(e) =
                            filesystem.write_file(&target_path, source_content.as_bytes())
                        {
                            sender
                                .send_warning(format!(
                                    "Failed to write '{}': {e}",
                                    target_path.display()
                                ))
                                .await;
                            skipped_count += 1;
                            continue;
                        }
                        deploy_state.record_deployment(
                            entry.source(),
                            &target_path.to_string_lossy(),
                            &source_checksum,
                        );
                    }

                    sender
                        .send_config_deployed(source_path.display(), target_path.display())
                        .await;
                    deployed_count += 1;
                }
                DeployDecision::Skip(reason) => {
                    sender
                        .send_config_skipped(source_path.display(), target_path.display(), &reason)
                        .await;
                    skipped_count += 1;
                }
                DeployDecision::Conflict => {
                    if options.auto_accept {
                        // Deploy anyway when --yes is used
                        sender
                            .send_config_deploying(source_path.display(), target_path.display())
                            .await;

                        if !options.dry_run {
                            if let Err(e) =
                                filesystem.write_file(&target_path, source_content.as_bytes())
                            {
                                sender
                                    .send_warning(format!(
                                        "Failed to write '{}': {e}",
                                        target_path.display()
                                    ))
                                    .await;
                                skipped_count += 1;
                                continue;
                            }
                            deploy_state.record_deployment(
                                entry.source(),
                                &target_path.to_string_lossy(),
                                &source_checksum,
                            );
                        }

                        sender
                            .send_config_deployed(source_path.display(), target_path.display())
                            .await;
                        deployed_count += 1;
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

            // Read source
            let source_content = match filesystem.read_file(&source_path) {
                Ok(content) => content,
                Err(_) => continue,
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
