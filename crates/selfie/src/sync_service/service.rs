//! SyncService implementation.
//!
//! Orchestrates git operations for syncing selfie package specs and dotfiles.
//! Uses [`GitSyncProvider`] for git operations and [`DotfileService`] for
//! drift checking during `sync status`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use tokio::sync::mpsc;

use crate::{
    config::SelfieConfig,
    dotfile_service::port::DotfileService,
    git::sync_provider::{ChangeType, GitSyncError, GitSyncProvider, RepoInfo},
    package::event::{
        EventSender, EventStream, OperationContext, OperationFailure, OperationResult,
        OperationSuccess, PackageEvent, StepCount, metadata::OperationType,
    },
};

use super::port::{
    ConfirmedCommit, PendingCommit, PrepareResult, PushOptions, SyncError, SyncService,
};

/// Concrete implementation of [`SyncService`].
///
/// Generic over:
/// - `G`: [`GitSyncProvider`] — for testable git mocking
/// - `D`: [`DotfileService`] — for drift checking in `sync status`
#[derive(Debug, Clone)]
pub struct SyncServiceImpl<G, D> {
    git: G,
    dotfile_service: D,
    config: SelfieConfig,
}

impl<G, D> SyncServiceImpl<G, D>
where
    G: GitSyncProvider + Clone + Send + Sync + 'static,
    D: DotfileService + Clone + Send + Sync + 'static,
{
    /// Create a new sync service instance.
    pub fn new(git: G, dotfile_service: D, config: SelfieConfig) -> Self {
        Self {
            git,
            dotfile_service,
            config,
        }
    }

    /// Discover the git repository containing the package directory.
    fn discover_repo(&self) -> Result<RepoInfo, GitSyncError> {
        self.git.discover_repo(self.config.package_directory())
    }

    /// Create an event stream from an async operation.
    ///
    /// Delegates to the shared [`create_event_stream`] utility.
    fn create_event_stream<Func, Fut>(f: Func) -> EventStream
    where
        Func: FnOnce(mpsc::Sender<PackageEvent>) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = ()> + Send,
    {
        crate::package::event::create_event_stream(f)
    }
}

impl<G, D> SyncService for SyncServiceImpl<G, D>
where
    G: GitSyncProvider + Clone + Send + Sync + 'static,
    D: DotfileService + Clone + Send + Sync + 'static,
{
    async fn status(&self) -> EventStream {
        let git = self.git.clone();
        let dotfile_service = self.dotfile_service.clone();
        let config = self.config.clone();

        Self::create_event_stream(move |tx| async move {
            let sender = EventSender::new_with_context(
                tx,
                OperationType::SyncStatus,
                String::new(),
                config.environment().to_string(),
                OperationContext::default(),
            );

            sender.send_started().await;

            // Step 1: Get repo status
            let repo_info = match git.discover_repo(config.package_directory()) {
                Ok(info) => info,
                Err(e) => {
                    sender
                        .send_completed(OperationResult::Failure(OperationFailure::Generic(
                            format!("Failed to discover repository: {e}"),
                        )))
                        .await;
                    return;
                }
            };

            let repo_status = match git.repo_status(&repo_info.root) {
                Ok(status) => status,
                Err(e) => {
                    sender
                        .send_completed(OperationResult::Failure(OperationFailure::Generic(
                            format!("Failed to get repo status: {e}"),
                        )))
                        .await;
                    return;
                }
            };

            sender
                .send(PackageEvent::SyncRepoStatus {
                    operation_info: sender.operation_info(),
                    repo_root: repo_info.root,
                    branch: repo_info.branch,
                    modified_count: repo_status.modified.len(),
                    staged_count: repo_status.staged.len(),
                    untracked_count: repo_status.untracked.len(),
                    deleted_count: repo_status.deleted.len(),
                    ahead: repo_status.ahead,
                    behind: repo_status.behind,
                })
                .await;

            // Step 2: Check dotfile drift
            let drift_stream = dotfile_service.check_drift().await;
            let (drifted_targets, total_deployed) = collect_drift_summary(drift_stream).await;

            sender
                .send(PackageEvent::SyncDriftSummary {
                    operation_info: sender.operation_info(),
                    drifted_targets,
                    total_deployed,
                })
                .await;

            sender
                .send_completed(OperationResult::Success(OperationSuccess::Generic(
                    "Sync status complete".to_string(),
                )))
                .await;
        })
    }

    async fn prepare_push(&self, options: &PushOptions) -> Result<PrepareResult, SyncError> {
        let repo_info = self.discover_repo().map_err(|e| match e {
            GitSyncError::NotARepo { path } => SyncError::NotARepo { path },
            other => SyncError::GitError(other.to_string()),
        })?;

        let status = self
            .git
            .repo_status(&repo_info.root)
            .map_err(|e| SyncError::GitError(e.to_string()))?;

        let ahead = status.ahead;

        if status.is_clean() && ahead == 0 {
            return Ok(PrepareResult {
                pending_commits: vec![],
                ahead: 0,
                warnings: vec![],
            });
        }

        let all_changed = collect_changed_files(&status);

        if all_changed.is_empty() {
            return Ok(PrepareResult {
                pending_commits: vec![],
                ahead,
                warnings: vec![],
            });
        }

        // Validate changed YAML files before proposing commits.
        let environment = self.config.environment();
        validate_changed_packages(&repo_info.root, &all_changed, environment)?;

        if options.batch {
            let files: Vec<PathBuf> = all_changed.iter().map(|(p, _)| p.clone()).collect();
            let message = options
                .message
                .clone()
                .unwrap_or_else(|| generate_batch_message(&all_changed));
            return Ok(PrepareResult {
                pending_commits: vec![PendingCommit {
                    name: "all".to_string(),
                    message,
                    files,
                }],
                ahead,
                warnings: vec![],
            });
        }

        let (commits, warnings) = group_changes_by_package(all_changed, options, &repo_info.root);

        Ok(PrepareResult {
            pending_commits: commits,
            ahead,
            warnings,
        })
    }

    async fn execute_push(&self, commits: Vec<ConfirmedCommit>) -> EventStream {
        let git = self.git.clone();
        let config = self.config.clone();

        Self::create_event_stream(move |tx| async move {
            let sender = EventSender::new_with_context(
                tx,
                OperationType::SyncPush,
                String::new(),
                config.environment().to_string(),
                OperationContext::default(),
            );

            sender.send_started().await;

            let repo_info = match git.discover_repo(config.package_directory()) {
                Ok(info) => info,
                Err(e) => {
                    sender
                        .send_completed(OperationResult::Failure(OperationFailure::Generic(
                            format!("Failed to discover repository: {e}"),
                        )))
                        .await;
                    return;
                }
            };

            let total_steps = commits.len() + 1; // +1 for the push step
            let mut commits_created = 0;

            for (i, commit) in commits.iter().enumerate() {
                sender
                    .send_progress(
                        i + 1,
                        total_steps,
                        format!("Committing: {}", commit.message),
                    )
                    .await;

                // Stage files
                if let Err(e) = git.stage_files(&repo_info.root, &commit.files) {
                    sender
                        .send_completed(OperationResult::Failure(OperationFailure::Generic(
                            format!("Failed to stage files: {e}"),
                        )))
                        .await;
                    return;
                }

                // Commit
                match git.commit(&repo_info.root, &commit.message) {
                    Ok(commit_id) => {
                        // Extract package name from the commit message for the event
                        let package_name = extract_package_name_from_message(&commit.message);
                        sender
                            .send(PackageEvent::SyncCommitCreated {
                                operation_info: sender.operation_info(),
                                package_name,
                                message: format!("{commit_id} {}", commit.message),
                            })
                            .await;
                        commits_created += 1;
                    }
                    Err(e) => {
                        sender
                            .send_completed(OperationResult::Failure(OperationFailure::Generic(
                                format!("Failed to commit: {e}"),
                            )))
                            .await;
                        return;
                    }
                }
            }

            // Snapshot ahead count before push to include pre-existing unpushed commits.
            let ahead_before_push = git
                .repo_status(&repo_info.root)
                .map(|s| s.ahead)
                .unwrap_or(commits_created);

            // Push all commits
            sender
                .send_progress(total_steps, total_steps, "Pushing to remote")
                .await;

            if let Err(e) = git.push(&repo_info.root) {
                sender
                    .send_completed(OperationResult::Failure(OperationFailure::Generic(
                        format!("Push failed: {e}. Your commits are preserved locally — run 'selfie sync pull' first, then try again."),
                    )))
                    .await;
                return;
            }

            sender
                .send_completed(OperationResult::Success(
                    OperationSuccess::SyncPushComplete {
                        commits_pushed: ahead_before_push,
                        steps_completed: StepCount::new(total_steps, total_steps),
                    },
                ))
                .await;
        })
    }

    async fn pull(&self) -> EventStream {
        let git = self.git.clone();
        let config = self.config.clone();

        Self::create_event_stream(move |tx| async move {
            let sender = EventSender::new_with_context(
                tx,
                OperationType::SyncPull,
                String::new(),
                config.environment().to_string(),
                OperationContext::default(),
            );

            sender.send_started().await;

            // Step 1: Discover repo
            let repo_info = match git.discover_repo(config.package_directory()) {
                Ok(info) => info,
                Err(e) => {
                    sender
                        .send_completed(OperationResult::Failure(OperationFailure::Generic(
                            format!("Failed to discover repository: {e}"),
                        )))
                        .await;
                    return;
                }
            };

            // Step 2: Check for staged changes that would conflict with merge.
            // Modified and untracked files are fine — git merge --ff-only handles
            // them safely (fails if they'd conflict). Only staged files indicate
            // an in-progress commit that shouldn't be disrupted.
            match git.repo_status(&repo_info.root) {
                Ok(status) if !status.staged.is_empty() => {
                    sender
                        .send_completed(OperationResult::Failure(OperationFailure::Generic(
                            "Staged changes detected. Commit or unstage them before pulling."
                                .to_string(),
                        )))
                        .await;
                    return;
                }
                Err(e) => {
                    sender
                        .send_completed(OperationResult::Failure(OperationFailure::Generic(
                            format!("Failed to check repo status: {e}"),
                        )))
                        .await;
                    return;
                }
                Ok(_) => {} // Clean — proceed
            }

            // Step 3: Fetch
            sender.send_progress(1, 3, "Fetching from remote").await;
            if let Err(e) = git.fetch(&repo_info.root) {
                sender
                    .send_completed(OperationResult::Failure(OperationFailure::Generic(
                        format!("Fetch failed: {e}"),
                    )))
                    .await;
                return;
            }

            // Step 4: Fast-forward merge
            sender.send_progress(2, 3, "Merging remote changes").await;
            match git.fast_forward(&repo_info.root) {
                Ok(crate::git::FastForwardResult::AlreadyUpToDate) => {
                    sender
                        .send_completed(OperationResult::Success(
                            OperationSuccess::SyncPullUpToDate {
                                steps_completed: StepCount::new(3, 3),
                            },
                        ))
                        .await;
                }
                Ok(crate::git::FastForwardResult::Advanced {
                    from,
                    to,
                    commit_count,
                }) => {
                    // Diff old HEAD vs new HEAD to see what changed
                    let changed_files = match git.diff_commits(&repo_info.root, &from, &to) {
                        Ok(files) => files,
                        Err(e) => {
                            sender
                                .send_log(
                                    crate::package::event::LogLevel::Warning,
                                    format!(
                                        "Failed to compute diff: {e}. Change summary may be incomplete."
                                    ),
                                )
                                .await;
                            Vec::new()
                        }
                    };

                    let (packages_updated, packages_added, packages_removed) =
                        categorize_pull_changes(&changed_files, &repo_info.root);

                    if has_dotfile_changes(&changed_files, &repo_info.root) {
                        sender
                            .send_log(
                                crate::package::event::LogLevel::Warning,
                                "Dotfile sources changed — run 'selfie apply' to deploy updates",
                            )
                            .await;
                    }

                    sender
                        .send_completed(OperationResult::Success(
                            OperationSuccess::SyncPullComplete {
                                commits_pulled: commit_count,
                                packages_updated,
                                packages_added,
                                packages_removed,
                                steps_completed: StepCount::new(3, 3),
                            },
                        ))
                        .await;
                }
                Ok(crate::git::FastForwardResult::Diverged) => {
                    sender
                        .send_completed(OperationResult::Failure(OperationFailure::Generic(
                            "Remote has diverged. Resolve manually with git.".to_string(),
                        )))
                        .await;
                }
                Err(e) => {
                    sender
                        .send_completed(OperationResult::Failure(OperationFailure::Generic(
                            format!("Fast-forward failed: {e}"),
                        )))
                        .await;
                }
            }
        })
    }
}

// ─── File grouping and commit message generation ─────────────────────────────

/// The kind of change for a file in the working tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FileChangeKind {
    Added,
    Modified,
    Deleted,
}

/// Validate all changed YAML package files, returning [`SyncError::ValidationFailed`]
/// if any have errors.
///
/// Only non-deleted YAML files are validated — deleted files are obviously not
/// parseable, and non-YAML files (dotfile sources) don't have a schema to validate.
fn validate_changed_packages(
    repo_root: &Path,
    changes: &[(PathBuf, FileChangeKind)],
    environment: &str,
) -> Result<(), SyncError> {
    use super::port::{PackageValidationFailure, PackageValidationIssue};

    let mut failures: Vec<PackageValidationFailure> = Vec::new();

    for (path, kind) in changes {
        if *kind == FileChangeKind::Deleted || !is_yaml_file(path) {
            continue;
        }

        let abs_path = repo_root.join(path);
        let path_str = path.display().to_string();

        let content = match std::fs::read_to_string(&abs_path) {
            Ok(c) => c,
            Err(e) => {
                failures.push(PackageValidationFailure {
                    path: path_str,
                    issues: vec![PackageValidationIssue {
                        level: "ERROR".to_string(),
                        category: "FileError".to_string(),
                        field: "-".to_string(),
                        message: format!("failed to read: {e}"),
                        location: None,
                    }],
                });
                continue;
            }
        };

        let mut package: crate::package::Package = match serde_saphyr::from_str(&content) {
            Ok(p) => p,
            Err(e) => {
                let location = e
                    .location()
                    .map(|loc| format!("line {} column {}", loc.line(), loc.column()));
                // Strip the " at line N column N" suffix since we capture it separately.
                let msg = e.to_string();
                let message = msg
                    .rfind(" at line ")
                    .map_or_else(|| msg.clone(), |idx| msg[..idx].to_string());

                failures.push(PackageValidationFailure {
                    path: path_str,
                    issues: vec![PackageValidationIssue {
                        level: "ERROR".to_string(),
                        category: "ParseError".to_string(),
                        field: "-".to_string(),
                        message,
                        location,
                    }],
                });
                continue;
            }
        };
        package.path = abs_path;
        package.raw_yaml = content;

        let result = package.validate(environment);
        let issues: Vec<PackageValidationIssue> = result
            .issues()
            .all_issues()
            .iter()
            .map(|i| PackageValidationIssue {
                level: match i.level() {
                    crate::validation::ValidationLevel::Error => "ERROR".to_string(),
                    crate::validation::ValidationLevel::Warning => "WARN".to_string(),
                },
                category: format!("{:?}", i.category()),
                field: i.field().to_string(),
                message: i.message().to_string(),
                location: None,
            })
            .collect();

        if !issues.is_empty() {
            failures.push(PackageValidationFailure {
                path: path_str,
                issues,
            });
        }
    }

    if failures.is_empty() {
        Ok(())
    } else {
        Err(SyncError::ValidationFailed { failures })
    }
}

/// Collect all changed files from a [`RepoStatus`] into a unified list.
///
/// Merges modified, staged (deduplicated against modified), untracked, and
/// deleted files into `(path, kind)` pairs suitable for grouping.
fn collect_changed_files(
    status: &crate::git::sync_provider::RepoStatus,
) -> Vec<(PathBuf, FileChangeKind)> {
    let mut changes = Vec::new();

    for path in &status.modified {
        changes.push((path.clone(), FileChangeKind::Modified));
    }
    for path in &status.staged {
        // Staged files may overlap with modified — deduplicate
        if !status.modified.contains(path) {
            changes.push((path.clone(), FileChangeKind::Modified));
        }
    }
    for path in &status.untracked {
        changes.push((path.clone(), FileChangeKind::Added));
    }
    for path in &status.deleted {
        changes.push((path.clone(), FileChangeKind::Deleted));
    }

    changes
}

/// Group changed files by package name and generate per-package commits.
///
/// Files that can't be associated with a package are either included as a
/// "housekeeping" commit (when `include_ungrouped` is set) or reported as
/// warnings.
///
/// Returns `(commits, warnings)`.
fn group_changes_by_package(
    changes: Vec<(PathBuf, FileChangeKind)>,
    options: &PushOptions,
    repo_root: &Path,
) -> (Vec<PendingCommit>, Vec<String>) {
    let mut groups: HashMap<String, Vec<(PathBuf, FileChangeKind)>> = HashMap::new();
    let mut ungrouped: Vec<(PathBuf, FileChangeKind)> = Vec::new();

    for (path, kind) in &changes {
        match infer_package_name(path, repo_root) {
            Some(name) => groups.entry(name).or_default().push((path.clone(), *kind)),
            None => ungrouped.push((path.clone(), *kind)),
        }
    }

    let mut commits = Vec::new();

    // Sort package names for deterministic ordering
    let mut package_names: Vec<String> = groups.keys().cloned().collect();
    package_names.sort();

    for name in package_names {
        let entries = groups.remove(&name).unwrap();
        let message = generate_commit_message(&name, &entries);
        let files = entries.into_iter().map(|(p, _)| p).collect();
        commits.push(PendingCommit {
            name,
            message,
            files,
        });
    }

    let mut warnings = Vec::new();
    if !ungrouped.is_empty() && options.include_ungrouped {
        let files = ungrouped.into_iter().map(|(p, _)| p).collect();
        commits.push(PendingCommit {
            name: "housekeeping".to_string(),
            message: "chore: update miscellaneous files".to_string(),
            files,
        });
    } else if !ungrouped.is_empty() {
        let count = ungrouped.len();
        let label = crate::pluralize(count, "file", "files");
        warnings.push(format!(
            "{count} {label} not associated with any package — use --include-ungrouped to commit them"
        ));
        tracing::warn!(count, "Ungrouped files excluded from push");
    }

    (commits, warnings)
}

/// Check whether any changed files are dotfile sources (non-YAML files
/// inside a package subdirectory, e.g., `starship/starship.toml`).
///
/// Root-level non-YAML files (README.md, .gitignore) are **not** considered
/// dotfile sources. A subdirectory file is only considered a dotfile source if
/// a YAML spec with the same name as its parent directory exists in the repo
/// (e.g., `starship/starship.toml` is a dotfile source if `starship.yml` exists
/// on disk, even if `starship.yml` itself didn't change).
fn has_dotfile_changes(changed_files: &[crate::git::ChangedFile], repo_root: &Path) -> bool {
    changed_files.iter().any(|f| {
        !is_yaml_file(&f.path)
            && f.path
                .parent()
                .and_then(|p| p.file_name())
                .is_some_and(|dir| has_spec_on_disk(repo_root, &dir.to_string_lossy()))
    })
}

/// Infer the package name from a file path, using the repo filesystem.
///
/// Rules:
/// - YAML files (`*.yml`, `*.yaml`) → package name is the file stem
///   (e.g., `starship.yml` → `starship`)
/// - Non-YAML files in a subdirectory whose name matches a YAML spec on disk
///   → package name is the parent directory name
///   (e.g., `starship/starship.toml` → `starship` if `starship.yml` exists
///   in the repo, even if it wasn't modified)
/// - Everything else → `None` (ungrouped)
fn infer_package_name(path: &Path, repo_root: &Path) -> Option<String> {
    // YAML files → package name is the file stem
    if is_yaml_file(path) {
        return path.file_stem().map(|s| s.to_string_lossy().to_string());
    }

    // Non-YAML files → use parent directory name if a matching YAML spec
    // exists on disk (prevents `docs/sync.md` from being grouped as "docs").
    let dir_name = path
        .parent()
        .and_then(|p| p.file_name())
        .map(|s| s.to_string_lossy().to_string())?;

    if has_spec_on_disk(repo_root, &dir_name) {
        Some(dir_name)
    } else {
        None
    }
}

/// Generate a conventional commit message for a package's changes.
fn generate_commit_message(name: &str, entries: &[(PathBuf, FileChangeKind)]) -> String {
    let has_yaml_changes = entries.iter().any(|(p, _)| is_yaml_file(p));
    let has_non_yaml_changes = entries.iter().any(|(p, _)| !is_yaml_file(p));
    let has_new_yaml = entries
        .iter()
        .any(|(p, k)| is_yaml_file(p) && *k == FileChangeKind::Added);
    let has_deleted_yaml = entries
        .iter()
        .any(|(p, k)| is_yaml_file(p) && *k == FileChangeKind::Deleted);

    if has_deleted_yaml && !has_non_yaml_changes {
        return format!("chore({name}): remove package spec");
    }

    if has_new_yaml && !has_non_yaml_changes {
        return format!("feat({name}): add package spec");
    }

    if has_yaml_changes && has_non_yaml_changes {
        return format!("chore({name}): update spec and dotfiles");
    }

    if has_yaml_changes {
        return format!("chore({name}): update package spec");
    }

    // Only dotfile source changes
    format!("chore({name}): update dotfile")
}

/// Generate a batch commit message summarizing all changes.
fn generate_batch_message(entries: &[(PathBuf, FileChangeKind)]) -> String {
    let count = entries.len();
    let label = crate::pluralize(count, "file", "files");
    format!("chore: update {count} {label}")
}

/// Check if a path is a YAML file.
fn is_yaml_file(path: &Path) -> bool {
    path.extension()
        .is_some_and(|ext| ext == "yml" || ext == "yaml")
}

/// Check if a YAML spec file (`{name}.yml` or `{name}.yaml`) exists on disk
/// in the repo root. Used to associate subdirectory files with their package.
fn has_spec_on_disk(repo_root: &Path, name: &str) -> bool {
    repo_root.join(format!("{name}.yml")).exists()
        || repo_root.join(format!("{name}.yaml")).exists()
}

/// Extract the package name from a conventional commit message.
///
/// Parses `"feat(name): ..."` or `"chore(name): ..."` → `"name"`.
/// Falls back to the full message if parsing fails.
fn extract_package_name_from_message(message: &str) -> String {
    if let Some(start) = message.find('(')
        && let Some(end) = message.find(')')
        && end > start
    {
        return message[start + 1..end].to_string();
    }
    message.to_string()
}

// ─── Drift summary collection ────────────────────────────────────────────────

/// Collect drift information from a DotfileService event stream.
///
/// Consumes the event stream and extracts drifted target paths and total
/// deployed count from drift events.
async fn collect_drift_summary(stream: EventStream) -> (Vec<String>, usize) {
    use futures::StreamExt;

    let mut drifted_targets = Vec::new();
    let mut total_deployed = 0;

    futures::pin_mut!(stream);
    while let Some(event) = stream.next().await {
        match event {
            PackageEvent::DotfileDriftDetected { target, .. } => {
                drifted_targets.push(target);
            }
            PackageEvent::Completed {
                result:
                    OperationResult::Success(OperationSuccess::DotfileDriftChecked {
                        drift_count: _,
                        total_count,
                        ..
                    }),
                ..
            } => {
                total_deployed = total_count;
            }
            _ => {}
        }
    }

    (drifted_targets, total_deployed)
}

// ─── Pull change categorization ──────────────────────────────────────────────

/// Categorize changed files from a pull into updated, added, and removed package names.
fn categorize_pull_changes(
    changed_files: &[crate::git::ChangedFile],
    repo_root: &Path,
) -> (Vec<String>, Vec<String>, Vec<String>) {
    let mut updated = Vec::new();
    let mut added = Vec::new();
    let mut removed = Vec::new();

    let mut seen_updated = std::collections::HashSet::new();
    let mut seen_added = std::collections::HashSet::new();
    let mut seen_removed = std::collections::HashSet::new();

    for file in changed_files {
        if let Some(name) = infer_package_name(&file.path, repo_root) {
            match file.change_type {
                ChangeType::Added => {
                    if seen_added.insert(name.clone()) {
                        added.push(name);
                    }
                }
                ChangeType::Modified => {
                    if seen_updated.insert(name.clone()) {
                        updated.push(name);
                    }
                }
                ChangeType::Deleted => {
                    if seen_removed.insert(name.clone()) {
                        removed.push(name);
                    }
                }
            }
        }
    }

    (updated, added, removed)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ─── infer_package_name tests ────────────────────────────────────────────

    /// Create a temp dir with YAML spec files for testing infer_package_name.
    fn repo_with_specs(specs: &[&str]) -> (tempfile::TempDir, PathBuf) {
        let temp = tempfile::TempDir::new().unwrap();
        let root = temp.path().to_path_buf();
        for spec in specs {
            std::fs::write(root.join(spec), "name: test\n").unwrap();
        }
        (temp, root)
    }

    #[test]
    fn infer_from_yaml_file_stem() {
        let temp = tempfile::TempDir::new().unwrap();
        assert_eq!(
            infer_package_name(Path::new("starship.yml"), temp.path()),
            Some("starship".to_string())
        );
        assert_eq!(
            infer_package_name(Path::new("fnm.yaml"), temp.path()),
            Some("fnm".to_string())
        );
    }

    #[test]
    fn infer_from_nested_yaml() {
        let temp = tempfile::TempDir::new().unwrap();
        assert_eq!(
            infer_package_name(Path::new("packages/starship.yml"), temp.path()),
            Some("starship".to_string())
        );
    }

    #[test]
    fn infer_from_subdirectory_with_spec_on_disk() {
        let (_temp, root) = repo_with_specs(&["starship.yml", "fnm.yml"]);

        assert_eq!(
            infer_package_name(Path::new("starship/starship.toml"), &root),
            Some("starship".to_string())
        );
        assert_eq!(
            infer_package_name(Path::new("fnm/init.fish"), &root),
            Some("fnm".to_string())
        );
    }

    #[test]
    fn infer_dotfile_only_change_still_groups() {
        // Even when the YAML spec itself didn't change, dotfile-only edits
        // should be grouped under the package (spec exists on disk).
        let (_temp, root) = repo_with_specs(&["starship.yml"]);
        assert_eq!(
            infer_package_name(Path::new("starship/starship.toml"), &root),
            Some("starship".to_string())
        );
    }

    #[test]
    fn infer_subdirectory_without_spec_is_ungrouped() {
        let temp = tempfile::TempDir::new().unwrap();
        // No docs.yml on disk → should NOT be grouped
        assert_eq!(
            infer_package_name(Path::new("docs/sync.md"), temp.path()),
            None
        );
    }

    #[test]
    fn infer_returns_none_for_root_non_yaml() {
        let temp = tempfile::TempDir::new().unwrap();
        assert_eq!(
            infer_package_name(Path::new("README.md"), temp.path()),
            None
        );
        assert_eq!(
            infer_package_name(Path::new(".gitignore"), temp.path()),
            None
        );
    }

    // ─── generate_commit_message tests ───────────────────────────────────────

    #[test]
    fn commit_message_new_yaml_only() {
        let entries = vec![(PathBuf::from("starship.yml"), FileChangeKind::Added)];
        assert_eq!(
            generate_commit_message("starship", &entries),
            "feat(starship): add package spec"
        );
    }

    #[test]
    fn commit_message_deleted_yaml() {
        let entries = vec![(PathBuf::from("old-tool.yml"), FileChangeKind::Deleted)];
        assert_eq!(
            generate_commit_message("old-tool", &entries),
            "chore(old-tool): remove package spec"
        );
    }

    #[test]
    fn commit_message_modified_yaml() {
        let entries = vec![(PathBuf::from("starship.yml"), FileChangeKind::Modified)];
        assert_eq!(
            generate_commit_message("starship", &entries),
            "chore(starship): update package spec"
        );
    }

    #[test]
    fn commit_message_dotfile_only() {
        let entries = vec![(
            PathBuf::from("starship/starship.toml"),
            FileChangeKind::Modified,
        )];
        assert_eq!(
            generate_commit_message("starship", &entries),
            "chore(starship): update dotfile"
        );
    }

    #[test]
    fn commit_message_yaml_and_dotfile() {
        let entries = vec![
            (PathBuf::from("starship.yml"), FileChangeKind::Modified),
            (
                PathBuf::from("starship/starship.toml"),
                FileChangeKind::Modified,
            ),
        ];
        assert_eq!(
            generate_commit_message("starship", &entries),
            "chore(starship): update spec and dotfiles"
        );
    }

    // ─── generate_batch_message tests ────────────────────────────────────────

    #[test]
    fn batch_message_singular() {
        let entries = vec![(PathBuf::from("starship.yml"), FileChangeKind::Modified)];
        assert_eq!(generate_batch_message(&entries), "chore: update 1 file");
    }

    #[test]
    fn batch_message_plural() {
        let entries = vec![
            (PathBuf::from("starship.yml"), FileChangeKind::Modified),
            (PathBuf::from("fnm.yml"), FileChangeKind::Added),
        ];
        assert_eq!(generate_batch_message(&entries), "chore: update 2 files");
    }

    // ─── extract_package_name_from_message tests ─────────────────────────────

    #[test]
    fn extract_name_from_conventional_commit() {
        assert_eq!(
            extract_package_name_from_message("feat(starship): add package spec"),
            "starship"
        );
        assert_eq!(
            extract_package_name_from_message("chore(fnm): update dotfile"),
            "fnm"
        );
    }

    #[test]
    fn extract_name_fallback() {
        assert_eq!(
            extract_package_name_from_message("update stuff"),
            "update stuff"
        );
    }

    // ─── categorize_pull_changes tests ───────────────────────────────────────

    #[test]
    fn categorize_groups_by_change_type() {
        use crate::git::{ChangeType, ChangedFile};

        let changed = vec![
            ChangedFile {
                path: PathBuf::from("starship.yml"),
                change_type: ChangeType::Modified,
            },
            ChangedFile {
                path: PathBuf::from("fnm.yml"),
                change_type: ChangeType::Added,
            },
            ChangedFile {
                path: PathBuf::from("old-tool.yml"),
                change_type: ChangeType::Deleted,
            },
        ];

        let temp = tempfile::TempDir::new().unwrap();
        let (updated, added, removed) = categorize_pull_changes(&changed, temp.path());
        assert_eq!(updated, vec!["starship"]);
        assert_eq!(added, vec!["fnm"]);
        assert_eq!(removed, vec!["old-tool"]);
    }

    #[test]
    fn categorize_deduplicates_package_names() {
        use crate::git::{ChangeType, ChangedFile};

        let (_temp, root) = repo_with_specs(&["starship.yml"]);
        let changed = vec![
            ChangedFile {
                path: PathBuf::from("starship.yml"),
                change_type: ChangeType::Modified,
            },
            ChangedFile {
                path: PathBuf::from("starship/starship.toml"),
                change_type: ChangeType::Modified,
            },
        ];

        let (updated, added, removed) = categorize_pull_changes(&changed, &root);
        assert_eq!(updated, vec!["starship"]);
        assert!(added.is_empty());
        assert!(removed.is_empty());
    }

    // ─── collect_changed_files tests ────────────────────────────────────────

    #[test]
    fn collect_merges_all_change_kinds() {
        use crate::git::sync_provider::RepoStatus;

        let status = RepoStatus {
            modified: vec![PathBuf::from("a.yml")],
            staged: vec![PathBuf::from("b.yml")],
            untracked: vec![PathBuf::from("c.yml")],
            deleted: vec![PathBuf::from("d.yml")],
            ahead: 0,
            behind: 0,
        };

        let changes = collect_changed_files(&status);

        assert_eq!(changes.len(), 4);
        assert!(changes.contains(&(PathBuf::from("a.yml"), FileChangeKind::Modified)));
        assert!(changes.contains(&(PathBuf::from("b.yml"), FileChangeKind::Modified)));
        assert!(changes.contains(&(PathBuf::from("c.yml"), FileChangeKind::Added)));
        assert!(changes.contains(&(PathBuf::from("d.yml"), FileChangeKind::Deleted)));
    }

    #[test]
    fn collect_deduplicates_staged_and_modified() {
        use crate::git::sync_provider::RepoStatus;

        let status = RepoStatus {
            modified: vec![PathBuf::from("overlap.yml")],
            staged: vec![
                PathBuf::from("overlap.yml"),
                PathBuf::from("only-staged.yml"),
            ],
            untracked: vec![],
            deleted: vec![],
            ahead: 0,
            behind: 0,
        };

        let changes = collect_changed_files(&status);

        // overlap.yml should appear once (from modified), only-staged.yml once (from staged)
        assert_eq!(changes.len(), 2);
    }

    #[test]
    fn collect_empty_status_returns_empty() {
        use crate::git::sync_provider::RepoStatus;

        let status = RepoStatus {
            modified: vec![],
            staged: vec![],
            untracked: vec![],
            deleted: vec![],
            ahead: 0,
            behind: 0,
        };

        assert!(collect_changed_files(&status).is_empty());
    }

    // ─── group_changes_by_package tests ──────────────────────────────────────

    #[test]
    fn group_creates_per_package_commits() {
        let temp = tempfile::TempDir::new().unwrap();
        let changes = vec![
            (PathBuf::from("starship.yml"), FileChangeKind::Added),
            (PathBuf::from("fnm.yml"), FileChangeKind::Modified),
        ];
        let options = PushOptions::default();

        let (commits, warnings) = group_changes_by_package(changes, &options, temp.path());

        assert_eq!(commits.len(), 2);
        assert_eq!(commits[0].name, "fnm");
        assert_eq!(commits[1].name, "starship");
        assert!(warnings.is_empty());
    }

    #[test]
    fn group_ungrouped_files_warn_by_default() {
        let temp = tempfile::TempDir::new().unwrap();
        let changes = vec![
            (PathBuf::from("starship.yml"), FileChangeKind::Added),
            (PathBuf::from("README.md"), FileChangeKind::Modified),
        ];
        let options = PushOptions::default();

        let (commits, warnings) = group_changes_by_package(changes, &options, temp.path());

        assert_eq!(commits.len(), 1);
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("--include-ungrouped"));
    }

    #[test]
    fn group_ungrouped_files_included_when_requested() {
        let temp = tempfile::TempDir::new().unwrap();
        let changes = vec![
            (PathBuf::from("starship.yml"), FileChangeKind::Added),
            (PathBuf::from("README.md"), FileChangeKind::Modified),
        ];
        let options = PushOptions {
            include_ungrouped: true,
            ..Default::default()
        };

        let (commits, warnings) = group_changes_by_package(changes, &options, temp.path());

        assert_eq!(commits.len(), 2);
        assert_eq!(commits[1].name, "housekeeping");
        assert!(warnings.is_empty());
    }

    #[test]
    fn group_bundles_yaml_and_dotfiles_together() {
        let (_temp, root) = repo_with_specs(&["starship.yml"]);
        let changes = vec![
            (PathBuf::from("starship.yml"), FileChangeKind::Modified),
            (
                PathBuf::from("starship/starship.toml"),
                FileChangeKind::Modified,
            ),
        ];
        let options = PushOptions::default();

        let (commits, _) = group_changes_by_package(changes, &options, &root);

        assert_eq!(commits.len(), 1);
        assert_eq!(commits[0].name, "starship");
        assert_eq!(commits[0].files.len(), 2);
        assert_eq!(
            commits[0].message,
            "chore(starship): update spec and dotfiles"
        );
    }

    #[test]
    fn group_dotfile_only_change_groups_under_package() {
        // Dotfile-only changes should be grouped even when the YAML spec wasn't modified
        let (_temp, root) = repo_with_specs(&["starship.yml"]);
        let changes = vec![(
            PathBuf::from("starship/starship.toml"),
            FileChangeKind::Modified,
        )];
        let options = PushOptions::default();

        let (commits, warnings) = group_changes_by_package(changes, &options, &root);

        assert_eq!(commits.len(), 1);
        assert_eq!(commits[0].name, "starship");
        assert!(warnings.is_empty());
    }

    // ─── has_dotfile_changes tests ───────────────────────────────────────────

    #[test]
    fn dotfile_changes_detected_with_spec_on_disk() {
        use crate::git::{ChangeType, ChangedFile};

        let (_temp, root) = repo_with_specs(&["starship.yml"]);
        let files = vec![ChangedFile {
            path: PathBuf::from("starship/starship.toml"),
            change_type: ChangeType::Modified,
        }];
        assert!(has_dotfile_changes(&files, &root));
    }

    #[test]
    fn dotfile_only_change_detected_without_yaml_in_changeset() {
        use crate::git::{ChangeType, ChangedFile};

        // starship.yml exists on disk but is NOT in the changeset
        let (_temp, root) = repo_with_specs(&["starship.yml"]);
        let files = vec![ChangedFile {
            path: PathBuf::from("starship/starship.toml"),
            change_type: ChangeType::Modified,
        }];
        assert!(has_dotfile_changes(&files, &root));
    }

    #[test]
    fn no_dotfile_changes_for_unrelated_subdirectory() {
        use crate::git::{ChangeType, ChangedFile};

        let temp = tempfile::TempDir::new().unwrap();
        let files = vec![ChangedFile {
            path: PathBuf::from("docs/sync.md"),
            change_type: ChangeType::Modified,
        }];
        assert!(!has_dotfile_changes(&files, temp.path()));
    }

    #[test]
    fn no_dotfile_changes_for_root_non_yaml() {
        use crate::git::{ChangeType, ChangedFile};

        let temp = tempfile::TempDir::new().unwrap();
        let files = vec![
            ChangedFile {
                path: PathBuf::from("README.md"),
                change_type: ChangeType::Modified,
            },
            ChangedFile {
                path: PathBuf::from(".gitignore"),
                change_type: ChangeType::Modified,
            },
        ];
        assert!(!has_dotfile_changes(&files, temp.path()));
    }

    #[test]
    fn no_dotfile_changes_for_yaml_only() {
        use crate::git::{ChangeType, ChangedFile};

        let temp = tempfile::TempDir::new().unwrap();
        let files = vec![ChangedFile {
            path: PathBuf::from("starship.yml"),
            change_type: ChangeType::Modified,
        }];
        assert!(!has_dotfile_changes(&files, temp.path()));
    }
}
