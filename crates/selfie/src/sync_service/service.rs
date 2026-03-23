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

use super::port::{ConfirmedCommit, PendingCommit, PushOptions, SyncService};

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
    /// Spawns a task that runs the closure with a channel sender, and returns
    /// the receiving end as a pinned stream.
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
            let (drifted_packages, total_deployed) = collect_drift_summary(drift_stream).await;

            sender
                .send(PackageEvent::SyncDriftSummary {
                    operation_info: sender.operation_info(),
                    drifted_packages,
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

    async fn prepare_push(&self, options: &PushOptions) -> anyhow::Result<Vec<PendingCommit>> {
        let repo_info = self.discover_repo().map_err(|e| anyhow::anyhow!("{e}"))?;

        let status = self
            .git
            .repo_status(&repo_info.root)
            .map_err(|e| anyhow::anyhow!("{e}"))?;

        if status.is_clean() && status.ahead == 0 {
            return Ok(vec![]);
        }

        // Collect all changed files (modified + staged + untracked + deleted)
        let mut all_changed: Vec<(PathBuf, FileChangeKind)> = Vec::new();
        for path in &status.modified {
            all_changed.push((path.clone(), FileChangeKind::Modified));
        }
        for path in &status.staged {
            // Staged files may overlap with modified — deduplicate
            if !status.modified.contains(path) {
                all_changed.push((path.clone(), FileChangeKind::Modified));
            }
        }
        for path in &status.untracked {
            all_changed.push((path.clone(), FileChangeKind::Added));
        }
        for path in &status.deleted {
            all_changed.push((path.clone(), FileChangeKind::Deleted));
        }

        if all_changed.is_empty() {
            return Ok(vec![]);
        }

        if options.batch {
            // Single commit for everything
            let files: Vec<PathBuf> = all_changed.iter().map(|(p, _)| p.clone()).collect();
            let message = options
                .message
                .clone()
                .unwrap_or_else(|| generate_batch_message(&all_changed));
            return Ok(vec![PendingCommit {
                name: "all".to_string(),
                message,
                files,
            }]);
        }

        // Group files by package name
        let mut groups: HashMap<String, Vec<(PathBuf, FileChangeKind)>> = HashMap::new();
        let mut ungrouped: Vec<(PathBuf, FileChangeKind)> = Vec::new();

        for (path, kind) in all_changed {
            match infer_package_name(&path) {
                Some(name) => groups.entry(name).or_default().push((path, kind)),
                None => ungrouped.push((path, kind)),
            }
        }

        let mut commits: Vec<PendingCommit> = Vec::new();

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

        // Handle ungrouped files
        if !ungrouped.is_empty() && options.include_untracked {
            let files = ungrouped.into_iter().map(|(p, _)| p).collect();
            commits.push(PendingCommit {
                name: "housekeeping".to_string(),
                message: "chore: update miscellaneous files".to_string(),
                files,
            });
        } else if !ungrouped.is_empty() {
            tracing::warn!(
                count = ungrouped.len(),
                "Files not associated with any package — use --include-untracked to commit them"
            );
        }

        Ok(commits)
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

            if commits.is_empty() {
                sender
                    .send_completed(OperationResult::Success(
                        OperationSuccess::SyncNothingToPush {
                            steps_completed: StepCount::new(0, 0),
                        },
                    ))
                    .await;
                return;
            }

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
                        commits_pushed: commits_created,
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

            // Step 2: Check for dirty working tree
            match git.repo_status(&repo_info.root) {
                Ok(status) if status.is_dirty() => {
                    sender
                        .send_completed(OperationResult::Failure(OperationFailure::Generic(
                            "Uncommitted changes detected. Run 'selfie sync push' first, then try again.".to_string(),
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
                    let changed_files = git
                        .diff_commits(&repo_info.root, &from, &to)
                        .unwrap_or_default();

                    let (packages_updated, packages_added, packages_removed) =
                        categorize_pull_changes(&changed_files);

                    // Check if any dotfile source files changed
                    let has_dotfile_changes = changed_files.iter().any(|f| {
                        !f.path
                            .extension()
                            .is_some_and(|ext| ext == "yml" || ext == "yaml")
                    });

                    if has_dotfile_changes {
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

/// Infer the package name from a file path.
///
/// Rules:
/// - YAML files (`*.yml`, `*.yaml`) → package name is the file stem
///   (e.g., `starship.yml` → `starship`, `packages/starship.yml` → `starship`)
/// - Files in a subdirectory → package name is the parent directory name
///   (e.g., `starship/starship.toml` → `starship`)
/// - Files at the root with no YAML extension → `None` (ungrouped)
fn infer_package_name(path: &Path) -> Option<String> {
    // Check if it's a YAML file — use file stem as package name
    if let Some(ext) = path.extension()
        && (ext == "yml" || ext == "yaml")
    {
        return path.file_stem().map(|s| s.to_string_lossy().to_string());
    }

    // For non-YAML files, use the parent directory name
    // e.g., `starship/starship.toml` → `starship`
    // e.g., `packages/starship/init.fish` → `starship` (last non-file component)
    path.parent()
        .and_then(|p| p.file_name())
        .map(|s| s.to_string_lossy().to_string())
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
    let label = if count == 1 { "file" } else { "files" };
    format!("chore: update {count} {label}")
}

/// Check if a path is a YAML file.
fn is_yaml_file(path: &Path) -> bool {
    path.extension()
        .is_some_and(|ext| ext == "yml" || ext == "yaml")
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
) -> (Vec<String>, Vec<String>, Vec<String>) {
    let mut updated = Vec::new();
    let mut added = Vec::new();
    let mut removed = Vec::new();

    // Use a set to deduplicate package names (multiple files may belong to one package)
    let mut seen_updated = std::collections::HashSet::new();
    let mut seen_added = std::collections::HashSet::new();
    let mut seen_removed = std::collections::HashSet::new();

    for file in changed_files {
        if let Some(name) = infer_package_name(&file.path) {
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

    #[test]
    fn infer_from_yaml_file_stem() {
        assert_eq!(
            infer_package_name(Path::new("starship.yml")),
            Some("starship".to_string())
        );
        assert_eq!(
            infer_package_name(Path::new("fnm.yaml")),
            Some("fnm".to_string())
        );
    }

    #[test]
    fn infer_from_nested_yaml() {
        // YAML in a subdirectory still uses file stem
        assert_eq!(
            infer_package_name(Path::new("packages/starship.yml")),
            Some("starship".to_string())
        );
    }

    #[test]
    fn infer_from_subdirectory() {
        assert_eq!(
            infer_package_name(Path::new("starship/starship.toml")),
            Some("starship".to_string())
        );
        assert_eq!(
            infer_package_name(Path::new("fnm/init.fish")),
            Some("fnm".to_string())
        );
    }

    #[test]
    fn infer_returns_none_for_root_non_yaml() {
        assert_eq!(infer_package_name(Path::new("README.md")), None);
        assert_eq!(infer_package_name(Path::new(".gitignore")), None);
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

        let (updated, added, removed) = categorize_pull_changes(&changed);
        assert_eq!(updated, vec!["starship"]);
        assert_eq!(added, vec!["fnm"]);
        assert_eq!(removed, vec!["old-tool"]);
    }

    #[test]
    fn categorize_deduplicates_package_names() {
        use crate::git::{ChangeType, ChangedFile};

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

        let (updated, added, removed) = categorize_pull_changes(&changed);
        assert_eq!(updated, vec!["starship"]);
        assert!(added.is_empty());
        assert!(removed.is_empty());
    }
}
