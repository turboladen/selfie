//! DotfileService implementation
//!
//! This module provides the concrete implementation of the [`DotfileService`] trait.
//! It coordinates between the package repository (for loading package dotfiles),
//! the file system (for reading/writing dotfiles), and the application config
//! to perform dotfile deployment operations.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::sync::mpsc;

use crate::{
    config::SelfieConfig,
    dotfile_service::{
        deploy::{DeployDecision, compute_checksum, deploy_decision, resolve_source_path},
        diff::unified_diff,
        port::ConflictResolution,
        state::{DeployState, DriftType},
    },
    fs::filesystem::{FileSystem, FileSystemError},
    package::{
        DotfileEntry, Package,
        event::{
            EventSender, EventStream, OperationContext, OperationFailure, OperationResult,
            OperationSuccess, PackageEvent, StepCount, metadata::OperationType,
        },
        port::PackageRepository,
    },
};

use super::port::{ApplyOptions, DotfileService};

/// Default deploy state filename
const DEPLOY_STATE_FILENAME: &str = "deploy-state.yml";

/// Concrete implementation of the [`DotfileService`] trait
///
/// Coordinates between the package repository, file system, and application
/// configuration to deploy dotfiles and check for drift.
///
/// Supports an optional second repository for standalone dotfiles (the `dotfiles/`
/// directory). When present, both repositories are scanned during apply and drift
/// operations.
#[derive(Debug, Clone)]
pub struct DotfileServiceImpl<R, F> {
    package_repository: R,
    dotfiles_repository: Option<R>,
    filesystem: F,
    config: SelfieConfig,
}

impl<R, F> DotfileServiceImpl<R, F>
where
    R: PackageRepository + Clone + Send + Sync + 'static,
    F: FileSystem + Clone + Send + Sync + 'static,
{
    /// Create a new dotfile service instance
    pub fn new(package_repository: R, filesystem: F, config: SelfieConfig) -> Self {
        Self {
            package_repository,
            dotfiles_repository: None,
            filesystem,
            config,
        }
    }

    /// Add a standalone dotfiles repository for the `dotfiles/` directory.
    ///
    /// When set, `apply` and `check_drift` operations will scan both the main
    /// package repository and this dotfiles repository.
    #[must_use]
    pub fn with_dotfiles_repository(mut self, repo: R) -> Self {
        self.dotfiles_repository = Some(repo);
        self
    }

    /// Collect packages from both the main package repository and the optional
    /// dotfiles repository, returning a combined list and any non-fatal warnings.
    ///
    /// Warnings are returned (rather than emitted directly) because collection
    /// happens before the event channel exists — callers emit them as
    /// `PackageEvent::Warning` once the stream is set up.
    fn collect_all_packages(
        package_repo: &R,
        dotfiles_repo: Option<&R>,
    ) -> Result<(Vec<Package>, Vec<String>), String> {
        let mut warnings = Vec::new();

        let mut packages = match package_repo.list_packages() {
            Ok(output) => output.valid_packages().cloned().collect::<Vec<_>>(),
            Err(e) => return Err(format!("Failed to load packages: {e}")),
        };

        let packages_count = packages.len();

        if let Some(dotfiles) = dotfiles_repo {
            match dotfiles.list_packages() {
                Ok(output) => packages.extend(output.valid_packages().cloned()),
                Err(e) => {
                    warnings.push(format!("Failed to load standalone dotfiles: {e}"));
                }
            }
        }

        // Detect duplicate names across packages/ and dotfiles/ directories.
        // Packages from packages/ take precedence (they appear first in the vec).
        if packages.len() > packages_count {
            let mut seen = std::collections::HashSet::new();
            for pkg in &packages[..packages_count] {
                seen.insert(pkg.name().to_string());
            }

            let mut deduped_dotfiles = Vec::new();
            for pkg in packages.drain(packages_count..) {
                if seen.contains(pkg.name()) {
                    warnings.push(format!(
                        "Duplicate name '{}' found in both packages/ and dotfiles/ — \
                         using the packages/ version",
                        pkg.name()
                    ));
                } else {
                    seen.insert(pkg.name().to_string());
                    deduped_dotfiles.push(pkg);
                }
            }
            packages.extend(deduped_dotfiles);
        }

        Ok((packages, warnings))
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
/// Uses the configured `state_directory` if available, otherwise defaults to
/// `~/.local/state/selfie/deploy-state.yml` (XDG_STATE_HOME) via the filesystem abstraction.
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

    // Default to XDG_STATE_HOME/selfie (~/.local/state/selfie) per the XDG Base
    // Directory Specification. Deploy state is per-machine, non-portable data —
    // exactly what XDG_STATE_HOME is designed for.
    //
    // expand_path canonicalizes, which fails if the directory doesn't exist yet.
    // To avoid requiring ~/.local/state/selfie to already exist on first run,
    // expand just "~" (which should always exist) and join the rest.
    let home = filesystem.expand_path(&PathBuf::from("~")).map_err(|_| {
        FileSystemError::IoError(std::sync::Arc::new(std::io::Error::other(
            "Cannot determine home directory for deploy state file",
        )))
    })?;
    let parent = home.join(".local").join("state").join("selfie");
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
/// Check that a name is safe for use as a filesystem path component.
///
/// Rejects names containing path separators, `..`, or characters outside
/// the alphanumeric + hyphen + underscore set used for package names.
fn is_safe_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_alphanumeric() || c == '-' || c == '_')
}

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

/// Validate that a resolved source path doesn't escape the YAML base directory.
///
/// Prevents path traversal attacks where a malicious package YAML could use
/// `../` sequences to read files outside the YAML file's parent directory.
///
/// Uses a component-level normalization that resolves `..` without requiring
/// the path to exist on disk (unlike `canonicalize`).
fn validate_source_path(source_path: &Path, base_dir: &Path) -> bool {
    match (
        std::path::absolute(source_path),
        std::path::absolute(base_dir),
    ) {
        (Ok(abs_source), Ok(abs_base)) => {
            normalize_path(&abs_source).starts_with(normalize_path(&abs_base))
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

impl<R, F> DotfileService for DotfileServiceImpl<R, F>
where
    R: PackageRepository + Clone + std::fmt::Debug + Send + Sync + 'static,
    F: FileSystem + Clone + std::fmt::Debug + Send + Sync + 'static,
{
    async fn apply_all(&self, options: ApplyOptions) -> EventStream {
        let collected =
            Self::collect_all_packages(&self.package_repository, self.dotfiles_repository.as_ref());
        let fs = self.filesystem.clone();
        let config = self.config.clone();

        Self::create_event_stream(move |tx| async move {
            let sender = EventSender::new_with_context(
                tx,
                OperationType::DotfileApply,
                String::new(),
                config.environment().to_string(),
                OperationContext::default(),
            );

            sender.send_started().await;

            let result = match collected {
                Ok((packages, warnings)) => {
                    for warning in warnings {
                        sender.send_warning(&warning).await;
                    }
                    handle_apply(&packages, &fs, &config, &sender, &options, None).await
                }
                Err(e) => {
                    OperationResult::Failure(crate::package::event::OperationFailure::Generic(e))
                }
            };

            sender.send_completed(result).await;
        })
    }

    async fn apply(&self, name: &str, options: ApplyOptions) -> EventStream {
        let collected =
            Self::collect_all_packages(&self.package_repository, self.dotfiles_repository.as_ref());
        let fs = self.filesystem.clone();
        let config = self.config.clone();
        let name = name.to_string();

        Self::create_event_stream(move |tx| async move {
            let sender = EventSender::new_with_context(
                tx,
                OperationType::DotfileApply,
                name.clone(),
                config.environment().to_string(),
                OperationContext::default(),
            );

            sender.send_started().await;

            let result = match collected {
                Ok((packages, warnings)) => {
                    for warning in warnings {
                        sender.send_warning(&warning).await;
                    }
                    handle_apply(&packages, &fs, &config, &sender, &options, Some(&name)).await
                }
                Err(e) => {
                    OperationResult::Failure(crate::package::event::OperationFailure::Generic(e))
                }
            };

            sender.send_completed(result).await;
        })
    }

    async fn check_drift(&self) -> EventStream {
        let collected =
            Self::collect_all_packages(&self.package_repository, self.dotfiles_repository.as_ref());
        let fs = self.filesystem.clone();
        let config = self.config.clone();

        Self::create_event_stream(move |tx| async move {
            let sender = EventSender::new_with_context(
                tx,
                OperationType::DotfileDrift,
                String::new(),
                config.environment().to_string(),
                OperationContext::default(),
            );

            sender.send_started().await;

            let result = match collected {
                Ok((packages, warnings)) => {
                    for warning in warnings {
                        sender.send_warning(&warning).await;
                    }
                    handle_check_drift(&packages, &fs, &config, &sender).await
                }
                Err(e) => {
                    OperationResult::Failure(crate::package::event::OperationFailure::Generic(e))
                }
            };

            sender.send_completed(result).await;
        })
    }

    async fn track_standalone(&self, name: &str, target_path: &str) -> EventStream {
        let dotfiles_repo = self.dotfiles_repository.clone();
        let fs = self.filesystem.clone();
        let config = self.config.clone();
        let name = name.to_string();
        let target_path = target_path.to_string();

        Self::create_event_stream(move |tx| async move {
            let sender = EventSender::new_with_context(
                tx,
                OperationType::DotfileTrack,
                name.clone(),
                config.environment().to_string(),
                OperationContext::default(),
            );
            sender.send_started().await;

            let result =
                handle_track_standalone(&name, &target_path, dotfiles_repo.as_ref(), &fs, &config);

            sender.send_completed(result).await;
        })
    }

    async fn track_for_package(&self, package_name: &str, target_path: &str) -> EventStream {
        let repo = self.package_repository.clone();
        let fs = self.filesystem.clone();
        let config = self.config.clone();
        let package_name = package_name.to_string();
        let target_path = target_path.to_string();

        Self::create_event_stream(move |tx| async move {
            let sender = EventSender::new_with_context(
                tx,
                OperationType::DotfileTrack,
                package_name.clone(),
                config.environment().to_string(),
                OperationContext::default(),
            );
            sender.send_started().await;

            let result = handle_track_for_package(&package_name, &target_path, &repo, &fs, &config);

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
            .send_dotfile_skipped(
                unit.source_path.display(),
                unit.target_path.display(),
                "dry run",
            )
            .await;
        return Ok(());
    }

    sender
        .send_dotfile_deploying(unit.source_path.display(), unit.target_path.display())
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
        .send_dotfile_deployed(unit.source_path.display(), unit.target_path.display())
        .await;
    Ok(())
}

/// Core logic for applying config files
async fn handle_apply<F>(
    packages: &[Package],
    filesystem: &F,
    config: &SelfieConfig,
    sender: &EventSender,
    options: &ApplyOptions,
    filter_name: Option<&str>,
) -> OperationResult
where
    F: FileSystem,
{
    let mut deploy_state = load_deploy_state(filesystem, config);

    let mut deployed_count: usize = 0;
    let mut skipped_count: usize = 0;
    let mut conflict_count: usize = 0;

    for package in packages {
        // If filtering by name, skip non-matching packages
        if let Some(name) = filter_name
            && package.name() != name
        {
            continue;
        }

        let dotfiles = package.dotfiles();
        if dotfiles.is_empty() {
            continue;
        }

        // Source paths resolve relative to the YAML file's parent directory,
        // so packages/fnm.yaml with source "fnm/init.fish" → packages/fnm/init.fish
        let base_dir = package
            .path()
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .to_path_buf();

        for entry in dotfiles {
            let source_path = resolve_source_path(&base_dir, entry.source());

            // Runtime path traversal guard: verify resolved path stays within base_dir
            if !validate_source_path(&source_path, &base_dir) {
                sender
                    .send_warning(format!(
                        "Skipping '{}': source path escapes YAML base directory",
                        entry.source()
                    ))
                    .await;
                skipped_count += 1;
                continue;
            }

            let target_path = expand_target_path(filesystem, entry.target());

            // Enforce documented rule: target must be absolute after expansion.
            // A relative target would write relative to CWD, which is surprising
            // and potentially dangerous.
            if !target_path.is_absolute() {
                sender
                    .send_warning(format!(
                        "Skipping '{}': target path '{}' is not absolute; targets must be absolute or start with '~/'",
                        entry.target(),
                        target_path.display()
                    ))
                    .await;
                skipped_count += 1;
                continue;
            }

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
            let decision =
                deploy_decision(&drift, target_exists, &source_checksum, &target_checksum);

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
                        if options.dry_run {
                            skipped_count += 1;
                        } else {
                            deployed_count += 1;
                        }
                    } else {
                        skipped_count += 1;
                    }
                }
                DeployDecision::Skip(reason) => {
                    // If this was an untracked file that's already in sync,
                    // record the state so future runs see DriftType::None.
                    if drift == DriftType::NotTracked && !options.dry_run {
                        deploy_state.record_deployment(
                            entry.source(),
                            &target_path.to_string_lossy(),
                            &source_checksum,
                        );
                    }
                    sender
                        .send_dotfile_skipped(source_path.display(), target_path.display(), &reason)
                        .await;
                    skipped_count += 1;
                }
                DeployDecision::Conflict => {
                    // Build the diff for display/resolution (needed by both
                    // the resolver and the fallback conflict event).
                    let target_content = filesystem.read_file(&target_path).unwrap_or_default();
                    let diff = unified_diff(
                        &target_content,
                        &source_content,
                        &target_path.to_string_lossy(),
                        &source_path.to_string_lossy(),
                    );

                    // Determine whether to accept: --yes flag, interactive
                    // resolver, or neither (skip with conflict event).
                    let accept = if options.auto_accept {
                        true
                    } else if let Some(resolver) = &options.conflict_resolver {
                        let src = source_path.display().to_string();
                        let tgt = target_path.display().to_string();
                        let d = diff.clone();
                        let r = Arc::clone(resolver);
                        tokio::task::spawn_blocking(move || r.resolve(&src, &tgt, &d))
                            .await
                            .unwrap_or(ConflictResolution::Skip)
                            == ConflictResolution::Accept
                    } else {
                        false
                    };

                    if accept {
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
                            if options.dry_run {
                                skipped_count += 1;
                            } else {
                                deployed_count += 1;
                            }
                        } else {
                            skipped_count += 1;
                        }
                    } else {
                        sender
                            .send_dotfile_conflict(
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
    OperationResult::Success(OperationSuccess::DotfilesApplied {
        deployed_count,
        skipped_count,
        conflict_count,
        environment: config.environment().to_string(),
        steps_completed: StepCount::new(total, total),
    })
}

/// Core logic for checking drift
async fn handle_check_drift<F>(
    packages: &[Package],
    filesystem: &F,
    config: &SelfieConfig,
    sender: &EventSender,
) -> OperationResult
where
    F: FileSystem,
{
    let deploy_state = load_deploy_state(filesystem, config);

    let mut drift_count: usize = 0;
    let mut total_count: usize = 0;

    for package in packages {
        // Source paths resolve relative to the YAML file's parent directory
        let base_dir = package
            .path()
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .to_path_buf();

        for entry in package.dotfiles() {
            total_count += 1;

            let source_path = resolve_source_path(&base_dir, entry.source());
            let target_path = expand_target_path(filesystem, entry.target());

            // Reject relative targets (same guard as handle_apply)
            if !target_path.is_absolute() {
                sender
                    .send_warning(format!(
                        "Skipping '{}': target path '{}' is not absolute",
                        entry.target(),
                        target_path.display()
                    ))
                    .await;
                continue;
            }

            // Runtime path traversal guard (same as handle_apply)
            if !validate_source_path(&source_path, &base_dir) {
                sender
                    .send_warning(format!(
                        "Skipping '{}': source path escapes YAML base directory",
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
                    .send_dotfile_drift_detected(target_path.display(), &drift)
                    .await;
                drift_count += 1;
            }
        }
    }

    OperationResult::Success(OperationSuccess::DotfileDriftChecked {
        drift_count,
        total_count,
        environment: config.environment().to_string(),
        steps_completed: StepCount::new(total_count, total_count),
    })
}

/// Handle `track_standalone`: copy target file into dotfiles dir, create a new
/// YAML spec, and record initial deploy state.
fn handle_track_standalone<R, F>(
    name: &str,
    target_path: &str,
    dotfiles_repo: Option<&R>,
    filesystem: &F,
    config: &SelfieConfig,
) -> OperationResult
where
    R: PackageRepository,
    F: FileSystem,
{
    let Some(dotfiles_repo) = dotfiles_repo else {
        return OperationResult::Failure(OperationFailure::Generic(
            "No dotfiles directory configured. Set `dotfiles_directory` in config.".to_string(),
        ));
    };

    // Reject names with path separators or traversal components
    if !is_safe_name(name) {
        return OperationResult::Failure(OperationFailure::Generic(format!(
            "Invalid name '{name}': must contain only alphanumeric characters, hyphens, or underscores"
        )));
    }

    let dotfiles_dir = config.dotfiles_directory();

    // Expand and validate the target path
    let expanded_target = expand_target_path(filesystem, target_path);
    if !filesystem.path_exists(&expanded_target) {
        return OperationResult::Failure(OperationFailure::Generic(format!(
            "Target file does not exist: {}",
            expanded_target.display()
        )));
    }

    // Read the target file content
    let content = match filesystem.read_file(&expanded_target) {
        Ok(c) => c,
        Err(e) => {
            return OperationResult::Failure(OperationFailure::Generic(format!(
                "Cannot read target file: {e}"
            )));
        }
    };

    // Determine source filename (just the basename of the target)
    let filename = expanded_target
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .to_string();

    // Check for existing spec (prevent silent overwrite)
    let spec_path = dotfiles_dir.join(format!("{name}.yml"));
    if filesystem.path_exists(&spec_path) {
        return OperationResult::Failure(OperationFailure::Generic(format!(
            "A dotfile spec already exists at {}. Remove it first or choose a different name.",
            spec_path.display()
        )));
    }

    // Copy the file into dotfiles_dir/name/filename
    let source_dir = dotfiles_dir.join(name);
    let source_path = source_dir.join(&filename);

    if filesystem.path_exists(&source_path) {
        return OperationResult::Failure(OperationFailure::Generic(format!(
            "Source file already exists at {}. Remove it first or choose a different name.",
            source_path.display()
        )));
    }

    if let Err(e) = filesystem.write_file(&source_path, content.as_bytes()) {
        return OperationResult::Failure(OperationFailure::Generic(format!(
            "Cannot write source file: {e}"
        )));
    }

    // Create the YAML spec (spec_path already computed above for overwrite check)
    let package = crate::package::PackageBuilder::default()
        .name(name)
        .dotfiles(vec![DotfileEntry::new(
            format!("{name}/{filename}"),
            target_path,
        )])
        .path(spec_path.clone())
        .build();

    if let Err(e) = dotfiles_repo.save_package(&package, &spec_path) {
        return OperationResult::Failure(OperationFailure::Generic(format!(
            "Cannot save spec: {e}"
        )));
    }

    // Record initial deploy state
    let checksum = compute_checksum(content.as_bytes());
    let mut deploy_state = load_deploy_state(filesystem, config);
    let source_key = format!("{name}/{filename}");
    deploy_state.record_deployment(&source_key, target_path, &checksum);
    if let Err(e) = save_deploy_state(filesystem, config, &deploy_state) {
        return OperationResult::Failure(OperationFailure::Generic(format!(
            "Cannot save deploy state: {e}"
        )));
    }

    OperationResult::Success(OperationSuccess::DotfileTracked {
        name: name.to_string(),
        source_path,
        target_path: target_path.to_string(),
        was_already_tracked: false,
        environment: config.environment().to_string(),
        steps_completed: StepCount::new(1, 1),
    })
}

/// Handle `track_for_package`: load an existing package, copy the target file
/// alongside the YAML, add a dotfiles entry, save, and record deploy state.
fn handle_track_for_package<R, F>(
    package_name: &str,
    target_path: &str,
    repo: &R,
    filesystem: &F,
    config: &SelfieConfig,
) -> OperationResult
where
    R: PackageRepository,
    F: FileSystem,
{
    // Load the existing package
    let mut package_blob = match repo.get_package(package_name) {
        Ok(blob) => blob,
        Err(e) => {
            return OperationResult::Failure(OperationFailure::Generic(format!(
                "Cannot load package '{package_name}': {e}"
            )));
        }
    };

    // Check if this target is already tracked in the package
    let expanded_target = expand_target_path(filesystem, target_path);
    if package_blob
        .package()
        .dotfiles()
        .iter()
        .any(|entry| expand_target_path(filesystem, entry.target()) == expanded_target)
    {
        return OperationResult::Success(OperationSuccess::DotfileTracked {
            name: package_name.to_string(),
            source_path: expanded_target,
            target_path: target_path.to_string(),
            was_already_tracked: true,
            environment: config.environment().to_string(),
            steps_completed: StepCount::new(1, 1),
        });
    }

    // Validate the target path
    if !filesystem.path_exists(&expanded_target) {
        return OperationResult::Failure(OperationFailure::Generic(format!(
            "Target file does not exist: {}",
            expanded_target.display()
        )));
    }

    // Read the target file content
    let content = match filesystem.read_file(&expanded_target) {
        Ok(c) => c,
        Err(e) => {
            return OperationResult::Failure(OperationFailure::Generic(format!(
                "Cannot read target file: {e}"
            )));
        }
    };

    // Determine where to copy the file — alongside the package YAML
    let package_dir = package_blob
        .file_path()
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .to_path_buf();
    let filename = expanded_target
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .to_string();
    let source_dir = package_dir.join(package_name);
    let source_path = source_dir.join(&filename);
    let relative_source = format!("{package_name}/{filename}");

    // Prevent silent overwrite of existing source files
    if filesystem.path_exists(&source_path) {
        return OperationResult::Failure(OperationFailure::Generic(format!(
            "Source file already exists at {}. Remove it first or choose a different file.",
            source_path.display()
        )));
    }

    // Copy the file
    if let Err(e) = filesystem.write_file(&source_path, content.as_bytes()) {
        return OperationResult::Failure(OperationFailure::Generic(format!(
            "Cannot write source file: {e}"
        )));
    }

    // Add dotfiles entry and save — source is relative to the YAML's parent dir
    package_blob
        .package_mut()
        .add_dotfile(DotfileEntry::new(&relative_source, target_path));

    let file_path = package_blob.file_path().to_path_buf();
    if let Err(e) = repo.save_package(package_blob.package(), &file_path) {
        return OperationResult::Failure(OperationFailure::Generic(format!(
            "Cannot save updated package: {e}"
        )));
    }

    // Record initial deploy state
    let checksum = compute_checksum(content.as_bytes());
    let mut deploy_state = load_deploy_state(filesystem, config);
    deploy_state.record_deployment(&relative_source, target_path, &checksum);
    if let Err(e) = save_deploy_state(filesystem, config, &deploy_state) {
        return OperationResult::Failure(OperationFailure::Generic(format!(
            "Cannot save deploy state: {e}"
        )));
    }

    OperationResult::Success(OperationSuccess::DotfileTracked {
        name: package_name.to_string(),
        source_path,
        target_path: target_path.to_string(),
        was_already_tracked: false,
        environment: config.environment().to_string(),
        steps_completed: StepCount::new(1, 1),
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
