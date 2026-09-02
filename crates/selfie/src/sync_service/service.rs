//! SyncService implementation.
//!
//! Orchestrates git operations for syncing selfie package specs and dotfiles.
//! Uses [`GitSyncProvider`] for git operations and [`DotfileService`] for
//! drift checking during `sync status`.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};

use tokio::sync::mpsc;

use crate::{
    config::SelfieConfig,
    dotfile_service::port::DotfileService,
    git::{
        message::GitMessage,
        sync_provider::{ChangeType, GitSyncError, GitSyncProvider},
    },
    package::event::{
        EventSender, EventStream, OperationContext, OperationFailure, OperationResult,
        OperationSuccess, PackageEvent, StepCount, metadata::OperationType,
    },
    privilege::{Privilege, SudoPolicy, WriteScope},
};

use super::port::{
    ConfirmedCommit, PendingCommit, PrepareResult, PushOptions, SyncError, SyncService,
};

/// Concrete implementation of [`SyncService`].
///
/// Generic over:
/// - `G`: [`GitSyncProvider`] — for testable git mocking
/// - `D`: [`DotfileService`] — for drift checking in `sync status`
/// - `P`: [`Privilege`] — for the sudo refusal on the mutating operations
#[derive(Debug, Clone)]
pub struct SyncServiceImpl<G, D, P> {
    git: G,
    dotfile_service: D,
    config: SelfieConfig,
    sudo_policy: SudoPolicy<P>,
}

impl<G, D, P> SyncServiceImpl<G, D, P>
where
    G: GitSyncProvider + Clone + Send + Sync + 'static,
    D: DotfileService + Clone + Send + Sync + 'static,
    P: Privilege,
{
    /// Create a new sync service instance.
    ///
    /// `sudo_policy` is its own rather than being read back out of
    /// `dotfile_service`: [`DotfileService`] is a port about dotfiles, and giving
    /// it a "what privilege am I running with" accessor would put a question
    /// there that has nothing to do with the port's job.
    pub fn new(
        git: G,
        dotfile_service: D,
        config: SelfieConfig,
        sudo_policy: SudoPolicy<P>,
    ) -> Self {
        Self {
            git,
            dotfile_service,
            config,
            sudo_policy,
        }
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

/// Run a blocking git operation on the tokio blocking thread pool.
///
/// Git operations do filesystem and network I/O that can block the async
/// executor. This helper moves them to a dedicated thread via
/// [`tokio::task::spawn_blocking`].
async fn blocking_git<F, T>(op: &str, f: F) -> Result<T, GitSyncError>
where
    F: FnOnce() -> Result<T, GitSyncError> + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(f).await.map_err(|e| {
        let message = if e.is_cancelled() {
            "blocking task was cancelled".to_string()
        } else if e.is_panic() {
            let panic = e.into_panic();
            if let Some(s) = panic.downcast_ref::<&str>() {
                format!("blocking task panicked: {s}")
            } else if let Some(s) = panic.downcast_ref::<String>() {
                format!("blocking task panicked: {s}")
            } else {
                "blocking task panicked".to_string()
            }
        } else {
            format!("blocking task failed: {e}")
        };
        GitSyncError::OperationFailed {
            operation: GitMessage::new(op),
            message: GitMessage::new(message),
        }
    })?
}

impl<G, D, P> SyncService for SyncServiceImpl<G, D, P>
where
    G: GitSyncProvider + Clone + Send + Sync + 'static,
    D: DotfileService + Clone + Send + Sync + 'static,
    P: Privilege + Send + Sync,
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
            let repo_info = match blocking_git("discover_repo", {
                let git = git.clone();
                let dir = config.package_directory().to_path_buf();
                move || git.discover_repo(&dir)
            })
            .await
            {
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

            let repo_status = match blocking_git("repo_status", {
                let git = git.clone();
                let root = repo_info.root.clone();
                move || git.repo_status(&root)
            })
            .await
            {
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

            // Step 2: Check dotfile drift (non-fatal — drift is supplementary info,
            // unlike git status which is required for sync to function)
            let drift_stream = dotfile_service.check_drift().await;
            let (drifted_targets, total_deployed, drift_error) =
                collect_drift_summary(drift_stream).await;

            if let Some(error_msg) = drift_error {
                sender
                    .send_warning(format!("Drift check failed: {error_msg}"))
                    .await;
            }

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
        // Ahead of the git work, because the CLI prompts per proposed commit
        // between prepare and execute. Refusing at `execute_push` alone would
        // walk the user through every one of those prompts and then discard the
        // answers.
        if let Some(refusal) = self.sudo_policy.refusal(WriteScope::Repository) {
            return Err(SyncError::Privilege(refusal));
        }

        let repo_info = blocking_git("discover_repo", {
            let git = self.git.clone();
            let dir = self.config.package_directory().to_path_buf();
            move || git.discover_repo(&dir)
        })
        .await
        .map_err(|e| match e {
            GitSyncError::NotARepo { path } => SyncError::NotARepo { path },
            other => SyncError::GitError(other.to_string()),
        })?;

        let status = blocking_git("repo_status", {
            let git = self.git.clone();
            let root = repo_info.root.clone();
            move || git.repo_status(&root)
        })
        .await
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
        validate_changed_packages(
            &repo_info.root,
            self.config.package_directory(),
            &all_changed,
            environment,
        )?;

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

        // From the package directory, not the repo root. The repository loads
        // specs from there, and the repo is discovered by walking up from it,
        // so the two are the same directory only in the flat layout. Built from
        // the root, this set is empty whenever specs live in a subdirectory,
        // and every dotfile source is reported ungrouped instead of committed
        // with its package.
        let spec_names = spec_names_in(self.config.package_directory());
        let (commits, warnings) = group_changes_by_package(all_changed, options, &spec_names);

        Ok(PrepareResult {
            pending_commits: commits,
            ahead,
            warnings,
        })
    }

    async fn execute_push(&self, commits: Vec<ConfirmedCommit>) -> EventStream {
        // Checked again here, not only in `prepare_push`. The two are separate
        // trait methods and the MCP server calls them as separate tool
        // invocations, so nothing guarantees the query ran first.
        let refusal = self.sudo_policy.refusal(WriteScope::Repository);
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

            if let Some(refusal) = refusal {
                sender
                    .send_completed(OperationResult::Failure(OperationFailure::Privilege(
                        refusal,
                    )))
                    .await;
                return;
            }

            let repo_info = match blocking_git("discover_repo", {
                let git = git.clone();
                let dir = config.package_directory().to_path_buf();
                move || git.discover_repo(&dir)
            })
            .await
            {
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

            for (i, commit) in commits.into_iter().enumerate() {
                let ConfirmedCommit { files, message } = commit;

                sender
                    .send_progress(i + 1, total_steps, format!("Committing: {message}"))
                    .await;

                // Stage files — move `files` directly to avoid cloning Vec<PathBuf>
                if let Err(e) = blocking_git("stage_files", {
                    let git = git.clone();
                    let root = repo_info.root.clone();
                    move || git.stage_files(&root, &files)
                })
                .await
                {
                    sender
                        .send_completed(OperationResult::Failure(OperationFailure::Generic(
                            format!("Failed to stage files: {e}"),
                        )))
                        .await;
                    return;
                }

                // Commit — clone message for the closure, keep original for the event
                let msg = message.clone();
                match blocking_git("commit", {
                    let git = git.clone();
                    let root = repo_info.root.clone();
                    move || git.commit(&root, &msg)
                })
                .await
                {
                    Ok(commit_id) => {
                        let package_name = extract_package_name_from_message(&message);
                        sender
                            .send(PackageEvent::SyncCommitCreated {
                                operation_info: sender.operation_info(),
                                package_name,
                                message: format!("{commit_id} {message}"),
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
            let ahead_before_push = blocking_git("repo_status", {
                let git = git.clone();
                let root = repo_info.root.clone();
                move || git.repo_status(&root)
            })
            .await
            .map(|s| s.ahead)
            .unwrap_or(commits_created);

            // Push all commits
            sender
                .send_progress(total_steps, total_steps, "Pushing to remote")
                .await;

            if let Err(e) = blocking_git("push", {
                let git = git.clone();
                let root = repo_info.root.clone();
                move || git.push(&root)
            })
            .await
            {
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
        let refusal = self.sudo_policy.refusal(WriteScope::Repository);
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

            if let Some(refusal) = refusal {
                sender
                    .send_completed(OperationResult::Failure(OperationFailure::Privilege(
                        refusal,
                    )))
                    .await;
                return;
            }

            // Step 1: Discover repo
            let repo_info = match blocking_git("discover_repo", {
                let git = git.clone();
                let dir = config.package_directory().to_path_buf();
                move || git.discover_repo(&dir)
            })
            .await
            {
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
            match blocking_git("repo_status", {
                let git = git.clone();
                let root = repo_info.root.clone();
                move || git.repo_status(&root)
            })
            .await
            {
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
            if let Err(e) = blocking_git("fetch", {
                let git = git.clone();
                let root = repo_info.root.clone();
                move || git.fetch(&root)
            })
            .await
            {
                sender
                    .send_completed(OperationResult::Failure(OperationFailure::Generic(
                        format!("Fetch failed: {e}"),
                    )))
                    .await;
                return;
            }

            // Step 4: Fast-forward merge
            sender.send_progress(2, 3, "Merging remote changes").await;
            match blocking_git("fast_forward", {
                let git = git.clone();
                let root = repo_info.root.clone();
                move || git.fast_forward(&root)
            })
            .await
            {
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
                    let changed_files = match blocking_git("diff_commits", {
                        let git = git.clone();
                        let root = repo_info.root.clone();
                        move || git.diff_commits(&root, &from, &to)
                    })
                    .await
                    {
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

                    // One read of the package directory for both consumers.
                    let spec_names = spec_names_in(config.package_directory());

                    let (packages_updated, packages_added, packages_removed) =
                        categorize_pull_changes(&changed_files, &spec_names);

                    if has_dotfile_changes(&changed_files, &spec_names) {
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
///
/// Informational notices are excluded. This is a gate on pushing, and an `Info`
/// issue is by definition not a defect — a package with a provider-sourced
/// dotfile always carries one, so counting it here would block `sync push` for
/// every correct package that uses the feature.
// Spec files in `dir` that resolve to one package name, grouped under that
// name. Only groups of two or more are returned.
//
// A collision is a property of the directory, not of any one file, so the
// per-file validation cannot see it: each colliding file is individually valid.
fn colliding_specs(dir: &Path) -> Vec<(String, Vec<String>)> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        // An unreadable directory is not this check's business to report; the
        // per-file validation already fails on anything it cannot read.
        return Vec::new();
    };

    let names = entries
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|name| is_spec_file(Path::new(name)));

    group_name_collisions(names)
}

// Split from the directory read so it can be tested on any file system. Two
// extensions of one name exist anywhere, but a host that folds case cannot hold
// two capitalizations of one name at all, so a test building real files reaches
// only half of what this groups.
//
// Grouping is by folded *stem*, matching how the package repository resolves a
// name: capitalization and the choice of `.yml` or `.yaml` are both invisible
// to it, so `Neovim.yml`, `neovim.yml` and `neovim.yaml` are three files
// claiming one package.
fn group_name_collisions<I>(names: I) -> Vec<(String, Vec<String>)>
where
    I: IntoIterator<Item = String>,
{
    let mut by_name: BTreeMap<String, Vec<String>> = BTreeMap::new();

    for file_name in names {
        let Some(name) = crate::package::spec_name_from_file_name(&file_name) else {
            continue;
        };
        by_name.entry(name).or_default().push(file_name);
    }

    by_name
        .into_iter()
        .filter(|(_, names)| names.len() > 1)
        .map(|(name, mut names)| {
            names.sort();
            (name, names)
        })
        .collect()
}

// Both flavors leave the package unresolvable, but only one destroys a file, so
// the refusals do not say the same thing. Two capitalizations of one file name
// cannot both survive a checkout on a case-insensitive file system; two
// extensions can, and telling someone their `.yml`/`.yaml` pair will be
// discarded sends them looking for a difference that is not there.
fn name_collision_message(names: &[String]) -> String {
    let mut folded: Vec<String> = names.iter().map(|n| n.to_lowercase()).collect();
    folded.sort();
    let capitalization_differs = folded.windows(2).any(|pair| pair[0] == pair[1]);

    let joined = names.join(" and ");

    if capitalization_differs {
        format!(
            "{joined} fold to one package name, and checking this out on a case-insensitive \
             file system discards one of them. Rename all but one before pushing."
        )
    } else {
        format!(
            "{joined} name the same package under different extensions, so no command can \
             resolve it. Rename or remove all but one before pushing."
        )
    }
}

fn validate_changed_packages(
    repo_root: &Path,
    package_dir: &Path,
    changes: &[(PathBuf, FileChangeKind)],
    environment: &str,
) -> Result<(), SyncError> {
    use super::port::{PackageValidationFailure, PackageValidationIssue};

    let mut failures: Vec<PackageValidationFailure> = Vec::new();

    // A spec is a direct child of the package directory, which is where the
    // repository loads them from and how deep it looks. Anything else ending in
    // `.yml` belongs to someone else -- a CI workflow, a linter config, a
    // Compose file -- and parsing one as a package refuses the push over fields
    // it was never going to have.
    let names_a_spec =
        |path: &Path| is_spec_file(path) && repo_root.join(path).parent() == Some(package_dir);

    // The package directory is scanned whether or not a spec in it changed. A
    // collision is a property of the directory, not of any one file, so the
    // per-file loop below cannot see it: each colliding file is individually
    // valid, and a push carrying nothing but dotfile sources touches none of
    // them at all.
    //
    // Only that directory. The repo root is a different one whenever the
    // package directory sits inside a larger dotfiles repository, and scanning
    // it would report two unrelated YAML files there as one package.
    //
    // Refusing the push is the only place the damage can still be prevented.
    // The machine that loses a spec is the one that pulls, and by then the file
    // is gone with nothing left to warn about.
    for (_name, names) in colliding_specs(package_dir) {
        let relative = package_dir.strip_prefix(repo_root).unwrap_or(package_dir);
        failures.push(PackageValidationFailure {
            path: relative.join(&names[0]).display().to_string(),
            issues: vec![PackageValidationIssue {
                level: "ERROR".to_string(),
                category: "NameCollision".to_string(),
                field: "-".to_string(),
                message: name_collision_message(&names),
                location: None,
            }],
        });
    }

    for (path, kind) in changes {
        if *kind == FileChangeKind::Deleted || !names_a_spec(path) {
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
        package.set_source(abs_path, content);

        let result = package.validate(environment);
        let issues: Vec<PackageValidationIssue> = result
            .issues()
            .all_issues()
            .iter()
            // Positive, not negative: naming the levels that block a push means a
            // level added later is absent here and absent from the match below,
            // which fails the build at the one place that has to decide about it.
            // A `!= Info` filter plus a wildcard would silently classify it as a
            // warning and silently block every push.
            .filter(|i| {
                matches!(
                    i.level(),
                    crate::validation::ValidationLevel::Error
                        | crate::validation::ValidationLevel::Warning
                )
            })
            .map(|i| PackageValidationIssue {
                level: match i.level() {
                    crate::validation::ValidationLevel::Error => "ERROR".to_string(),
                    crate::validation::ValidationLevel::Warning => "WARN".to_string(),
                    // Filtered out above; kept exhaustive so a new level is a
                    // compile error rather than a silent reclassification.
                    crate::validation::ValidationLevel::Info => {
                        unreachable!("Info is filtered out before this map")
                    }
                },
                category: format!("{:?}", i.category()),
                field: i.field().to_string(),
                message: i.message().to_string(),
                location: i.location().map(str::to_string),
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
    spec_names: &BTreeSet<String>,
) -> (Vec<PendingCommit>, Vec<String>) {
    let mut groups: HashMap<String, Vec<(PathBuf, FileChangeKind)>> = HashMap::new();
    let mut ungrouped: Vec<(PathBuf, FileChangeKind)> = Vec::new();

    for (path, kind) in &changes {
        match infer_package_name(path, spec_names) {
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
fn has_dotfile_changes(
    changed_files: &[crate::git::ChangedFile],
    spec_names: &BTreeSet<String>,
) -> bool {
    changed_files.iter().any(|f| {
        !is_spec_file(&f.path)
            && f.path
                .parent()
                .and_then(|p| p.file_name())
                .is_some_and(|dir| spec_named(spec_names, &dir.to_string_lossy()).is_some())
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
fn infer_package_name(path: &Path, spec_names: &BTreeSet<String>) -> Option<String> {
    // Folded, because this is the key changes are grouped under and identity is
    // folded everywhere else. Returning the spelling on disk splits one package
    // into two commits as soon as two of its files disagree about case --
    // `Neovim.yml` grouping as `Neovim` while `neovim/starship.toml` groups as
    // `neovim`.
    if let Some(name) = path
        .file_name()
        .and_then(|file_name| file_name.to_str())
        .and_then(crate::package::spec_name_from_file_name)
    {
        return Some(name);
    }

    // Not a spec, so it belongs to the package its parent directory names --
    // if a spec answers to that name. Without the check, `docs/sync.md` would
    // be grouped as the package "docs".
    let dir_name = path
        .parent()
        .and_then(|p| p.file_name())
        .map(|s| s.to_string_lossy().to_string())?;

    spec_named(spec_names, &dir_name)
}

/// Generate a conventional commit message for a package's changes.
fn generate_commit_message(name: &str, entries: &[(PathBuf, FileChangeKind)]) -> String {
    let has_yaml_changes = entries.iter().any(|(p, _)| is_spec_file(p));
    let has_non_yaml_changes = entries.iter().any(|(p, _)| !is_spec_file(p));
    let has_new_yaml = entries
        .iter()
        .any(|(p, k)| is_spec_file(p) && *k == FileChangeKind::Added);
    let has_deleted_yaml = entries
        .iter()
        .any(|(p, k)| is_spec_file(p) && *k == FileChangeKind::Deleted);

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

/// Check whether a path names a package spec.
///
/// Not the same question as whether it is YAML: `.yml` is a hidden file with
/// no stem, so it names no package and this returns false for it.
fn is_spec_file(path: &Path) -> bool {
    // Answered by the same function the package repository resolves names
    // with. When these disagreed, a `Neovim.YML` the loader read as a spec was
    // invisible here, so the push guard and per-file validation both skipped it.
    path.file_name()
        .and_then(|name| name.to_str())
        .and_then(crate::package::spec_name_from_file_name)
        .is_some()
}

/// The folded names of every spec sitting directly in `spec_dir`.
///
/// That is the package directory, not the repo root: the repository loads specs
/// from there, and the repo is discovered by walking up from it. Callers pass
/// the configured package directory.
///
/// Built once per operation. Grouping consults it for each changed file, so
/// reading the directory inside that loop would cost one `read_dir` per change.
fn spec_names_in(spec_dir: &Path) -> BTreeSet<String> {
    // Reading the directory rather than probing `{name}.yml` and `{name}.yaml`.
    // Those two paths answer for one capitalization only, so a spec stored as
    // `Starship.YML` left `starship/starship.toml` unmatched on a
    // case-sensitive file system and matched on a case-insensitive one -- the
    // per-machine split that folding names exists to remove.
    let Ok(entries) = std::fs::read_dir(spec_dir) else {
        return BTreeSet::new();
    };

    entries
        .flatten()
        .filter_map(|entry| {
            let file_name = entry.file_name();
            file_name
                .to_str()
                .and_then(crate::package::spec_name_from_file_name)
        })
        .collect()
}

/// The folded form of `name` if a spec answers to it.
///
/// Returns the folded name rather than a `bool` so a caller that needs it --
/// grouping keys on it -- does not fold a second time.
fn spec_named(spec_names: &BTreeSet<String>, name: &str) -> Option<String> {
    // Folded here rather than at each call site, so a caller cannot compare a
    // raw directory name against a set that holds only folded ones.
    let folded = name.to_lowercase();
    spec_names.contains(&folded).then_some(folded)
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
/// Consumes the event stream and extracts drifted target paths, total
/// deployed count, and any failure message from drift events.
///
/// Assumes the stream emits exactly one `Completed` event (either success or
/// failure), matching the `check_drift()` contract.
async fn collect_drift_summary(stream: EventStream) -> (Vec<String>, usize, Option<String>) {
    use futures::StreamExt;

    let mut drifted_targets = Vec::new();
    let mut total_deployed = 0;
    let mut drift_error = None;

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
            PackageEvent::Completed {
                result: OperationResult::Failure(failure),
                ..
            } => {
                drift_error = Some(failure.to_string());
            }
            _ => {}
        }
    }

    (drifted_targets, total_deployed, drift_error)
}

// ─── Pull change categorization ──────────────────────────────────────────────

/// Categorize changed files from a pull into updated, added, and removed package names.
fn categorize_pull_changes(
    changed_files: &[crate::git::ChangedFile],
    spec_names: &BTreeSet<String>,
) -> (Vec<String>, Vec<String>, Vec<String>) {
    let mut updated = Vec::new();
    let mut added = Vec::new();
    let mut removed = Vec::new();

    let mut seen_updated = std::collections::HashSet::new();
    let mut seen_added = std::collections::HashSet::new();
    let mut seen_removed = std::collections::HashSet::new();

    for file in changed_files {
        if let Some(name) = infer_package_name(&file.path, spec_names) {
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

    // Create a temp dir with YAML spec files for testing infer_package_name.
    fn repo_with_specs(specs: &[&str]) -> (tempfile::TempDir, PathBuf) {
        let temp = tempfile::TempDir::new().unwrap();
        let root = temp.path().to_path_buf();
        for spec in specs {
            std::fs::write(root.join(spec), "name: test\n").unwrap();
        }
        (temp, root)
    }

    // The name is the grouping key, so an unfolded one splits a single package
    // across two commits as soon as two of its files disagree about case. Both
    // of these belong to the package `neovim` and have to report it identically.
    #[test]
    fn a_package_groups_under_one_name_however_its_files_are_spelled() {
        let (_temp, root) = repo_with_specs(&["Neovim.YML"]);
        let spec_names = spec_names_in(&root);

        assert_eq!(
            infer_package_name(Path::new("Neovim.YML"), &spec_names),
            Some("neovim".to_string()),
            "the spec itself"
        );
        assert_eq!(
            infer_package_name(Path::new("Neovim/init.lua"), &spec_names),
            Some("neovim".to_string()),
            "a dotfile source beside it"
        );
    }

    #[test]
    fn infer_from_yaml_file_stem() {
        let temp = tempfile::TempDir::new().unwrap();
        assert_eq!(
            infer_package_name(Path::new("starship.yml"), &spec_names_in(temp.path())),
            Some("starship".to_string())
        );
        assert_eq!(
            infer_package_name(Path::new("fnm.yaml"), &spec_names_in(temp.path())),
            Some("fnm".to_string())
        );
    }

    #[test]
    fn infer_from_nested_yaml() {
        let temp = tempfile::TempDir::new().unwrap();
        assert_eq!(
            infer_package_name(
                Path::new("packages/starship.yml"),
                &spec_names_in(temp.path())
            ),
            Some("starship".to_string())
        );
    }

    #[test]
    fn infer_from_subdirectory_with_spec_on_disk() {
        let (_temp, root) = repo_with_specs(&["starship.yml", "fnm.yml"]);

        assert_eq!(
            infer_package_name(Path::new("starship/starship.toml"), &spec_names_in(&root)),
            Some("starship".to_string())
        );
        assert_eq!(
            infer_package_name(Path::new("fnm/init.fish"), &spec_names_in(&root)),
            Some("fnm".to_string())
        );
    }

    #[test]
    fn infer_dotfile_only_change_still_groups() {
        // Even when the YAML spec itself didn't change, dotfile-only edits
        // should be grouped under the package (spec exists on disk).
        let (_temp, root) = repo_with_specs(&["starship.yml"]);
        assert_eq!(
            infer_package_name(Path::new("starship/starship.toml"), &spec_names_in(&root)),
            Some("starship".to_string())
        );
    }

    #[test]
    fn infer_subdirectory_without_spec_is_ungrouped() {
        let temp = tempfile::TempDir::new().unwrap();
        // No docs.yml on disk → should NOT be grouped
        assert_eq!(
            infer_package_name(Path::new("docs/sync.md"), &spec_names_in(temp.path())),
            None
        );
    }

    #[test]
    fn infer_returns_none_for_root_non_yaml() {
        let temp = tempfile::TempDir::new().unwrap();
        assert_eq!(
            infer_package_name(Path::new("README.md"), &spec_names_in(temp.path())),
            None
        );
        assert_eq!(
            infer_package_name(Path::new(".gitignore"), &spec_names_in(temp.path())),
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
        let (updated, added, removed) =
            categorize_pull_changes(&changed, &spec_names_in(temp.path()));
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

        let (updated, added, removed) = categorize_pull_changes(&changed, &spec_names_in(&root));
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

        let (commits, warnings) =
            group_changes_by_package(changes, &options, &spec_names_in(temp.path()));

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

        let (commits, warnings) =
            group_changes_by_package(changes, &options, &spec_names_in(temp.path()));

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

        let (commits, warnings) =
            group_changes_by_package(changes, &options, &spec_names_in(temp.path()));

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

        let (commits, _) = group_changes_by_package(changes, &options, &spec_names_in(&root));

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

        let (commits, warnings) =
            group_changes_by_package(changes, &options, &spec_names_in(&root));

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
        assert!(has_dotfile_changes(&files, &spec_names_in(&root)));
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
        assert!(has_dotfile_changes(&files, &spec_names_in(&root)));
    }

    #[test]
    fn no_dotfile_changes_for_unrelated_subdirectory() {
        use crate::git::{ChangeType, ChangedFile};

        let temp = tempfile::TempDir::new().unwrap();
        let files = vec![ChangedFile {
            path: PathBuf::from("docs/sync.md"),
            change_type: ChangeType::Modified,
        }];
        assert!(!has_dotfile_changes(&files, &spec_names_in(temp.path())));
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
        assert!(!has_dotfile_changes(&files, &spec_names_in(temp.path())));
    }

    #[test]
    fn no_dotfile_changes_for_yaml_only() {
        use crate::git::{ChangeType, ChangedFile};

        let temp = tempfile::TempDir::new().unwrap();
        let files = vec![ChangedFile {
            path: PathBuf::from("starship.yml"),
            change_type: ChangeType::Modified,
        }];
        assert!(!has_dotfile_changes(&files, &spec_names_in(temp.path())));
    }

    // ─── collect_drift_summary tests ──────────────────────────────────────────

    fn test_operation_info() -> crate::package::event::OperationInfo {
        crate::package::event::OperationInfo {
            id: uuid::Uuid::new_v4(),
            operation_type: OperationType::DotfileDrift,
            package_name: String::new(),
            environment: "test".to_string(),
            context: OperationContext::default(),
            timestamp: std::time::Instant::now(),
        }
    }

    fn events_to_stream(events: Vec<PackageEvent>) -> EventStream {
        Box::pin(futures::stream::iter(events))
    }

    #[tokio::test]
    async fn collect_drift_summary_surfaces_failure() {
        let events = vec![PackageEvent::Completed {
            operation_info: test_operation_info(),
            result: OperationResult::Failure(OperationFailure::Generic(
                "permission denied".to_string(),
            )),
        }];

        let (drifted, total, error) = collect_drift_summary(events_to_stream(events)).await;

        assert!(drifted.is_empty());
        assert_eq!(total, 0);
        assert_eq!(error.as_deref(), Some("permission denied"));
    }

    #[tokio::test]
    async fn collect_drift_summary_returns_none_on_success() {
        let events = vec![
            PackageEvent::DotfileDriftDetected {
                operation_info: test_operation_info(),
                target: "/home/user/.bashrc".to_string(),
                drift_type: "content mismatch".to_string(),
                reason: None,
            },
            PackageEvent::Completed {
                operation_info: test_operation_info(),
                result: OperationResult::Success(OperationSuccess::DotfileDriftChecked {
                    drift_count: 1,
                    total_count: 3,
                    environment: "test".to_string(),
                    steps_completed: crate::package::event::StepCount::new(3, 3),
                }),
            },
        ];

        let (drifted, total, error) = collect_drift_summary(events_to_stream(events)).await;

        assert_eq!(drifted, vec!["/home/user/.bashrc"]);
        assert_eq!(total, 3);
        assert!(error.is_none());
    }
}

#[cfg(test)]
mod push_validation_tests {
    use super::*;

    fn write_package(dir: &std::path::Path, name: &str, body: &str) -> PathBuf {
        let path = dir.join(format!("{name}.yml"));
        std::fs::write(&path, body).unwrap();
        PathBuf::from(format!("{name}.yml"))
    }

    #[test]
    fn an_informational_notice_does_not_block_a_push() {
        // `validate_changed_packages` fails the push on ANY issue it collects, so
        // an informational notice reaching that list would block `sync push` for
        // every package using a provider-sourced dotfile — which is every correct
        // use of the feature.
        let temp = tempfile::TempDir::new().unwrap();
        let relative = write_package(
            temp.path(),
            "creds",
            "name: creds\nenvironments:\n  test:\n    install: echo i\ndotfiles:\n  \
             - command: op read op://Private/token\n    target: ~/.creds\n",
        );

        let result = validate_changed_packages(
            temp.path(),
            temp.path(),
            &[(relative, FileChangeKind::Modified)],
            "test",
        );

        assert!(
            result.is_ok(),
            "an Info-only package must be pushable, got: {result:?}"
        );
    }

    #[test]
    fn a_real_validation_error_still_blocks_a_push() {
        // The counterpart: filtering notices must not have disarmed the gate.
        let temp = tempfile::TempDir::new().unwrap();
        let relative = write_package(
            temp.path(),
            "broken",
            "name: broken\nenvironments:\n  test:\n    install: echo i\ndotfiles:\n  \
             - source: a.tpl\n    command: op read x\n    target: ~/.creds\n",
        );

        let result = validate_changed_packages(
            temp.path(),
            temp.path(),
            &[(relative, FileChangeKind::Modified)],
            "test",
        );

        assert!(
            matches!(result, Err(SyncError::ValidationFailed { .. })),
            "a package with a real error must still fail, got: {result:?}"
        );
    }
}

#[cfg(test)]
mod credential_egress_tests {
    //! Whether a credential in a git error survives the trip to the strings the
    //! CLI prints and the MCP server serializes.
    //!
    //! These build their `GitSyncError` through [`GitMessage`], as `run_git`
    //! does, so what they prove is that **nothing downstream re-leaks** — not
    //! that redaction fires on real git output. The tests in `git::adapter` that
    //! run an actual `git` against a loopback 401 are the ones that prove that,
    //! and they are the only ones that do.

    use futures::StreamExt;

    use super::*;
    use crate::git::{
        CommitId, FastForwardResult, GitMessage, GitSyncError, GitSyncProvider, RepoInfo,
        RepoStatus,
    };
    use crate::privilege::Elevation;
    use crate::sync_service::port::PushOptions;
    use test_common::assert_secret_free;

    const FIXTURE_TOKEN: &str = "Zk9qP2mW7xR4tL6vB1nH3jD5";

    // git 2.50.1's own output for a non-interactive fetch whose remote URL
    // carries a token as its username.
    fn leaky_git_stderr() -> String {
        format!(
            "fatal: could not read Password for 'http://{FIXTURE_TOKEN}@127.0.0.1:8731': \
             terminal prompts disabled"
        )
    }

    // Which git call fails.
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum FailAt {
        RepoStatus,
        Push,
    }

    // A `GitSyncProvider` that fails one operation with git's real leaky
    // stderr, cleaned exactly as `run_git` cleans it.
    //
    // Hand-written rather than `MockGitSyncProvider` because `SyncServiceImpl`
    // requires `Clone`, which mockall does not generate.
    #[derive(Clone)]
    struct GitFailingWith {
        fail_at: FailAt,
    }

    impl GitFailingWith {
        fn error(&self) -> GitSyncError {
            GitSyncError::OperationFailed {
                operation: GitMessage::new("git push"),
                message: GitMessage::from_stderr(leaky_git_stderr().as_bytes()),
            }
        }
    }

    impl GitSyncProvider for GitFailingWith {
        fn discover_repo(&self, path: &Path) -> Result<RepoInfo, GitSyncError> {
            Ok(RepoInfo {
                root: path.to_path_buf(),
                branch: Some("main".to_string()),
                remote_name: Some("origin".to_string()),
            })
        }

        fn repo_status(&self, _: &Path) -> Result<RepoStatus, GitSyncError> {
            if self.fail_at == FailAt::RepoStatus {
                return Err(self.error());
            }
            Ok(RepoStatus {
                modified: vec![PathBuf::from("starship.yml")],
                ..Default::default()
            })
        }

        fn stage_files(&self, _: &Path, _: &[PathBuf]) -> Result<(), GitSyncError> {
            Ok(())
        }

        fn commit(&self, _: &Path, _: &str) -> Result<CommitId, GitSyncError> {
            Ok(CommitId("abc1234def".to_string()))
        }

        fn push(&self, _: &Path) -> Result<(), GitSyncError> {
            if self.fail_at == FailAt::Push {
                return Err(self.error());
            }
            Ok(())
        }

        fn fetch(&self, _: &Path) -> Result<(), GitSyncError> {
            Ok(())
        }

        fn fast_forward(&self, _: &Path) -> Result<FastForwardResult, GitSyncError> {
            Ok(FastForwardResult::AlreadyUpToDate)
        }

        fn diff_commits(
            &self,
            _: &Path,
            _: &CommitId,
            _: &CommitId,
        ) -> Result<Vec<crate::git::ChangedFile>, GitSyncError> {
            Ok(Vec::new())
        }
    }

    // Never called: neither `prepare_push` nor `execute_push` touches it.
    #[derive(Clone)]
    pub(super) struct UnusedDotfileService;

    impl DotfileService for UnusedDotfileService {
        async fn apply_all(&self, _: crate::dotfile_service::port::ApplyOptions) -> EventStream {
            unreachable!("the push paths do not deploy dotfiles")
        }

        async fn apply(
            &self,
            _: &str,
            _: crate::dotfile_service::port::ApplyOptions,
        ) -> EventStream {
            unreachable!("the push paths do not deploy dotfiles")
        }

        async fn check_drift(&self) -> EventStream {
            unreachable!("only `status` checks drift")
        }

        async fn track_standalone(&self, _: &str, _: &str) -> EventStream {
            unreachable!("the push paths do not track dotfiles")
        }

        async fn track_for_package(&self, _: &str, _: &str) -> EventStream {
            unreachable!("the push paths do not track dotfiles")
        }
    }

    // A privilege port reporting a fixed answer, so these tests do not depend on
    // how the suite was invoked.
    #[derive(Clone, Copy, Debug)]
    pub(super) struct RunningAs(pub(super) Elevation);

    impl Privilege for RunningAs {
        fn elevation(&self) -> Elevation {
            self.0
        }
    }

    fn service(
        fail_at: FailAt,
    ) -> SyncServiceImpl<GitFailingWith, UnusedDotfileService, RunningAs> {
        service_running_as(fail_at, Elevation::Unprivileged)
    }

    fn service_running_as(
        fail_at: FailAt,
        elevation: Elevation,
    ) -> SyncServiceImpl<GitFailingWith, UnusedDotfileService, RunningAs> {
        SyncServiceImpl::new(
            GitFailingWith { fail_at },
            UnusedDotfileService,
            // Built with the crate's own types: `test_common` links `selfie` as
            // an external crate, so its `SelfieConfig` is a different type here.
            crate::config::SelfieConfigBuilder::default()
                .environment("test-env")
                .package_directory("/tmp/selfie-packages")
                .build(),
            SudoPolicy::new(RunningAs(elevation)),
        )
    }

    // The event-stream exit: `OperationFailure::Generic` reaches
    // `PackageEvent::Completed`, which the CLI prints and which
    // `event_collector` serializes into the MCP tool result as
    // `{"status":"failure","error": <this>}`.
    #[tokio::test]
    async fn a_failed_push_keeps_the_credential_out_of_every_event() {
        let stream = service(FailAt::Push)
            .execute_push(vec![ConfirmedCommit {
                files: vec![PathBuf::from("starship.yml")],
                message: "chore(starship): update package spec".to_string(),
            }])
            .await;

        let events: Vec<PackageEvent> = stream.collect().await;
        assert!(!events.is_empty(), "the scan must have something to scan");

        // Every event, not the one the leak was expected in.
        for event in &events {
            assert_secret_free(&format!("{event:?}"), FIXTURE_TOKEN, "a PackageEvent Debug");
        }

        let failure = events
            .iter()
            .find_map(|e| match e {
                PackageEvent::Completed {
                    result: OperationResult::Failure(f),
                    ..
                } => Some(f),
                _ => None,
            })
            .expect("the push must have failed");

        // Exactly what `event_collector.rs` puts in the MCP JSON.
        let rendered = format!("{failure}");
        assert_secret_free(&rendered, FIXTURE_TOKEN, "the MCP-facing failure string");
        // Control: git's message really did travel this far, so the scan above
        // was not passing on an empty or generic error.
        assert!(
            rendered.contains("could not read Password"),
            "git's diagnosis must survive redaction, got: {rendered}"
        );
    }

    // selfie-adsm. Sync was not covered by selfie-tcu2's refusal: it holds a
    // `DotfileService` that carries the gate, but only ever calls `check_drift`,
    // which is correctly ungated -- so nothing in the sync path consulted it.
    // Committing and pushing as root leaves root-owned objects, refs and index
    // entries in a repository the user owns, and unlike deploy state that does
    // not self-heal.
    mod running_under_sudo {
        use super::*;

        // Panics on every call, so a passing test proves the refusal fired
        // *before* git was reached rather than merely instead of reporting its
        // result. Asserting on the returned error alone would not: a gate placed
        // after the git work returns the same error.
        #[derive(Clone)]
        struct GitThatMustNotRun;

        const NEVER: &str = "git must not be reached when the run is refused";

        impl GitSyncProvider for GitThatMustNotRun {
            fn discover_repo(&self, _: &Path) -> Result<RepoInfo, GitSyncError> {
                unreachable!("{NEVER}")
            }
            fn repo_status(&self, _: &Path) -> Result<RepoStatus, GitSyncError> {
                unreachable!("{NEVER}")
            }
            fn stage_files(&self, _: &Path, _: &[PathBuf]) -> Result<(), GitSyncError> {
                unreachable!("{NEVER}")
            }
            fn commit(&self, _: &Path, _: &str) -> Result<CommitId, GitSyncError> {
                unreachable!("{NEVER}")
            }
            fn push(&self, _: &Path) -> Result<(), GitSyncError> {
                unreachable!("{NEVER}")
            }
            fn fetch(&self, _: &Path) -> Result<(), GitSyncError> {
                unreachable!("{NEVER}")
            }
            fn fast_forward(&self, _: &Path) -> Result<FastForwardResult, GitSyncError> {
                unreachable!("{NEVER}")
            }
            fn diff_commits(
                &self,
                _: &Path,
                _: &CommitId,
                _: &CommitId,
            ) -> Result<Vec<crate::git::ChangedFile>, GitSyncError> {
                unreachable!("{NEVER}")
            }
        }

        fn refusing() -> SyncServiceImpl<GitThatMustNotRun, UnusedDotfileService, RunningAs> {
            SyncServiceImpl::new(
                GitThatMustNotRun,
                UnusedDotfileService,
                crate::config::SelfieConfigBuilder::default()
                    .environment("test-env")
                    .package_directory("/tmp/selfie-packages")
                    .build(),
                SudoPolicy::new(RunningAs(Elevation::Sudo)),
            )
        }

        fn refused(events: &[PackageEvent]) -> bool {
            events.iter().any(|e| {
                matches!(
                    e,
                    PackageEvent::Completed {
                        result: OperationResult::Failure(OperationFailure::Privilege(_)),
                        ..
                    }
                )
            })
        }

        // Refused at the query step, ahead of the git work, because the CLI
        // prompts per proposed commit between prepare and execute. Gating only
        // `execute_push` would walk the user through every prompt first.
        #[tokio::test]
        async fn prepare_push_is_refused_before_git_runs() {
            let error = refusing()
                .prepare_push(&PushOptions::default())
                .await
                .expect_err("a sudo run must not prepare a push");

            assert!(
                matches!(error, SyncError::Privilege(_)),
                "expected a privilege refusal, got: {error:?}"
            );
        }

        // Checked separately from `prepare_push` because the two are separate
        // trait methods, and the MCP server calls them as separate tool
        // invocations -- nothing guarantees the query ran first.
        #[tokio::test]
        async fn execute_push_is_refused_before_git_runs() {
            let events: Vec<PackageEvent> = refusing()
                .execute_push(vec![ConfirmedCommit {
                    files: vec![PathBuf::from("starship.yml")],
                    message: "chore(starship): update package spec".to_string(),
                }])
                .await
                .collect()
                .await;

            assert!(refused(&events), "expected a privilege refusal: {events:?}");
        }

        #[tokio::test]
        async fn pull_is_refused_before_git_runs() {
            let events: Vec<PackageEvent> = refusing().pull().await.collect().await;

            assert!(refused(&events), "expected a privilege refusal: {events:?}");
        }

        // Both controls assert the run reached *validation*, not that it
        // succeeded: these fixtures name a `starship.yml` that is not on disk, so
        // `prepare_push` always ends in `ValidationFailed`. That is the useful
        // assertion anyway -- validation runs after `discover_repo` and
        // `repo_status`, so reaching it proves the run got well past the gate and
        // did real work, which `!matches!(.., Privilege(_))` alone would not.
        //
        // Without these two, every test above would still pass if the gate
        // refused unconditionally, and `sudo` would not be what is under test.
        fn reached_validation(result: &Result<PrepareResult, SyncError>) -> bool {
            matches!(result, Err(SyncError::ValidationFailed { .. }))
        }

        #[tokio::test]
        async fn real_root_may_still_push() {
            let result = service_running_as(FailAt::Push, Elevation::Root)
                .prepare_push(&PushOptions::default())
                .await;

            assert!(
                reached_validation(&result),
                "a root run with no SUDO_UID must not be refused: {result:?}"
            );
        }

        #[tokio::test]
        async fn allow_sudo_overrides_the_sync_refusal() {
            let service = SyncServiceImpl::new(
                GitFailingWith {
                    fail_at: FailAt::Push,
                },
                UnusedDotfileService,
                crate::config::SelfieConfigBuilder::default()
                    .environment("test-env")
                    .package_directory("/tmp/selfie-packages")
                    .build(),
                SudoPolicy::new(RunningAs(Elevation::Sudo)).allowing_sudo(),
            );

            let result = service.prepare_push(&PushOptions::default()).await;

            assert!(
                reached_validation(&result),
                "--allow-sudo must override the gate: {result:?}"
            );
        }
    }

    // The other MCP exit. `prepare_push` returns `SyncError` directly, and
    // `selfie_sync_push` renders it into its own error JSON without ever
    // touching the event stream — so the event scan above cannot cover it.
    #[tokio::test]
    async fn a_failed_prepare_keeps_the_credential_out_of_the_returned_error() {
        let error = service(FailAt::RepoStatus)
            .prepare_push(&PushOptions::default())
            .await
            .expect_err("repo_status failed, so prepare must fail");

        let rendered = error.to_string();
        assert_secret_free(&rendered, FIXTURE_TOKEN, "a SyncError Display");
        assert_secret_free(&format!("{error:?}"), FIXTURE_TOKEN, "a SyncError Debug");
        assert!(
            rendered.contains("could not read Password"),
            "git's diagnosis must survive redaction, got: {rendered}"
        );
    }
}

#[cfg(test)]
mod name_collision_tests {
    use crate::git::{
        CommitId, FastForwardResult, GitSyncError, GitSyncProvider, RepoInfo, RepoStatus,
    };
    use std::path::{Path, PathBuf};

    // The repo is discovered by walking up from the package directory, so a
    // package directory below the repo root leaves the two distinct. Nothing
    // else in this file models that: the other git fake returns the path it was
    // handed, which makes root and package directory the same and hides every
    // bug about confusing them.
    #[derive(Clone)]
    struct GitWithRepoAboveThePackageDir {
        status: RepoStatus,
    }

    impl GitSyncProvider for GitWithRepoAboveThePackageDir {
        fn discover_repo(&self, path: &Path) -> Result<RepoInfo, GitSyncError> {
            Ok(RepoInfo {
                root: path
                    .parent()
                    .expect("the fixture nests the package directory")
                    .to_path_buf(),
                branch: Some("main".to_string()),
                remote_name: Some("origin".to_string()),
            })
        }

        fn repo_status(&self, _: &Path) -> Result<RepoStatus, GitSyncError> {
            Ok(self.status.clone())
        }

        fn stage_files(&self, _: &Path, _: &[PathBuf]) -> Result<(), GitSyncError> {
            unreachable!("preparing a push stages nothing")
        }

        fn commit(&self, _: &Path, _: &str) -> Result<CommitId, GitSyncError> {
            unreachable!("preparing a push commits nothing")
        }

        fn push(&self, _: &Path) -> Result<(), GitSyncError> {
            unreachable!("preparing a push pushes nothing")
        }

        fn fetch(&self, _: &Path) -> Result<(), GitSyncError> {
            unreachable!("preparing a push fetches nothing")
        }

        fn fast_forward(&self, _: &Path) -> Result<FastForwardResult, GitSyncError> {
            unreachable!("preparing a push merges nothing")
        }

        fn diff_commits(
            &self,
            _: &Path,
            _: &CommitId,
            _: &CommitId,
        ) -> Result<Vec<crate::git::ChangedFile>, GitSyncError> {
            unreachable!("only pull diffs commits")
        }
    }

    // A dotfile source belongs to the package its parent directory names, and
    // the spec that answers to that name lives in the package directory. Built
    // from the repo root instead, the set is empty whenever the package
    // directory is a subdirectory, so this file is reported ungrouped and left
    // out of the push -- the user's dotfile edit silently does not sync.
    #[tokio::test]
    async fn a_dotfile_source_is_grouped_when_the_package_dir_is_below_the_repo_root() {
        use crate::sync_service::port::{PushOptions, SyncService};

        let root = tempfile::tempdir().unwrap();
        let packages = root.path().join("packages");
        std::fs::create_dir_all(packages.join("starship")).unwrap();
        std::fs::write(
            packages.join("starship.yml"),
            "name: starship\nenvironments:\n  test-env:\n    install: \"true\"\n",
        )
        .unwrap();
        std::fs::write(packages.join("starship/starship.toml"), "x = 1\n").unwrap();

        let service = super::SyncServiceImpl::new(
            GitWithRepoAboveThePackageDir {
                status: RepoStatus {
                    modified: vec![PathBuf::from("packages/starship/starship.toml")],
                    staged: vec![],
                    untracked: vec![],
                    deleted: vec![],
                    ahead: 0,
                    behind: 0,
                },
            },
            crate::sync_service::service::credential_egress_tests::UnusedDotfileService,
            crate::config::SelfieConfigBuilder::default()
                .environment("test-env")
                .package_directory(&packages)
                .build(),
            super::SudoPolicy::new(
                crate::sync_service::service::credential_egress_tests::RunningAs(
                    crate::privilege::Elevation::Unprivileged,
                ),
            ),
        );

        let prepared = service
            .prepare_push(&PushOptions::default())
            .await
            .expect("the fixture is valid, so preparing must succeed");

        assert!(
            prepared.warnings.is_empty(),
            "the file belongs to `starship`, so nothing is ungrouped: {:?}",
            prepared.warnings
        );
        let names: Vec<&str> = prepared
            .pending_commits
            .iter()
            .map(|c| c.name.as_str())
            .collect();
        assert_eq!(names, vec!["starship"], "got: {names:?}");
    }

    use super::{colliding_specs, group_name_collisions, name_collision_message};

    // The guard is only worth anything if a push actually refuses. Detection
    // being correct proves nothing about the call site: deleting it leaves
    // every other test in this module passing.
    #[test]
    fn a_push_touching_a_colliding_directory_is_refused() {
        use super::{FileChangeKind, validate_changed_packages};
        use std::path::PathBuf;

        let root = tempfile::tempdir().unwrap();
        let packages = root.path().join("packages");
        std::fs::create_dir_all(&packages).unwrap();
        let spec = "name: neovim\nenvironments:\n  test-env:\n    install: \"true\"\n";
        std::fs::write(packages.join("Neovim.yml"), spec).unwrap();
        std::fs::write(packages.join("neovim.yml"), spec).unwrap();

        if std::fs::read_dir(&packages).unwrap().count() < 2 {
            eprintln!(
                "SKIPPED a_push_touching_a_colliding_directory_is_refused: this file \
                 system folds case, so the pair cannot be created"
            );
            return;
        }

        let changes = vec![(
            PathBuf::from("packages/neovim.yml"),
            FileChangeKind::Modified,
        )];
        // `packages`, not the root: that is where this fixture puts the specs,
        // and the scan looks where the repository loads them from.
        let result = validate_changed_packages(root.path(), &packages, &changes, "test-env");

        let Err(error) = result else {
            panic!("a push carrying a case collision must be refused");
        };
        let rendered = format!("{error:?}");
        assert!(rendered.contains("NameCollision"), "got: {rendered}");
        assert!(rendered.contains("Neovim.yml"), "got: {rendered}");
    }

    // Reads a real directory, so it can only run where the colliding pair can
    // exist. Skipping is loud rather than silent: a quietly-skipped test looks
    // identical to a passing one.
    #[test]
    fn a_real_directory_holding_both_capitalizations_is_reported() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Neovim.yml"), "name: Neovim").unwrap();
        std::fs::write(dir.path().join("neovim.yml"), "name: neovim").unwrap();

        let entries = std::fs::read_dir(dir.path()).unwrap().count();
        if entries < 2 {
            eprintln!(
                "SKIPPED a_real_directory_holding_both_capitalizations_is_reported: \
                 this file system folds case, so the pair cannot be created"
            );
            return;
        }

        let groups = colliding_specs(dir.path());
        assert_eq!(groups.len(), 1, "got: {groups:?}");
        assert_eq!(groups[0].1, vec!["Neovim.yml", "neovim.yml"]);
    }

    fn group(names: &[&str]) -> Vec<(String, Vec<String>)> {
        group_name_collisions(names.iter().map(|s| (*s).to_string()))
    }

    // The pair a push must refuse. Both names are reported, so the message can
    // tell the user which files to rename, and the group is keyed by the
    // package name they collide on rather than by either file name.
    #[test]
    fn names_differing_only_by_case_are_one_group() {
        assert_eq!(
            group(&["Neovim.yml", "neovim.yml"]),
            vec![(
                "neovim".to_string(),
                vec!["Neovim.yml".to_string(), "neovim.yml".to_string()]
            )]
        );
    }

    // Distinct packages must not be grouped, or every push on a healthy
    // repository is refused.
    #[test]
    fn distinct_names_do_not_collide() {
        assert!(group(&["neovim.yml", "vim.yml", "emacs.yml"]).is_empty());
    }

    // A lone file is not a collision.
    #[test]
    fn a_single_file_is_not_a_collision() {
        assert!(group(&["Neovim.yml"]).is_empty());
    }

    // A sync repository holds more YAML than selfie's. Treating all of it as
    // specs parses a CI workflow as a package and refuses the push over fields
    // it was never going to have -- `unknown field 'jobs'`, `unknown field
    // 'on'`, `at least one environment must be defined`. The repository reads
    // specs from the package directory and looks no deeper, so that is the rule
    // the push gate has to use as well.
    //
    // Needs no particular file system: nothing here depends on how the disk
    // compares names.
    #[test]
    fn yaml_that_is_not_a_spec_does_not_fail_a_push() {
        use super::{FileChangeKind, validate_changed_packages};

        let root = tempfile::tempdir().unwrap();
        let packages = root.path().join("packages");
        std::fs::create_dir_all(&packages).unwrap();
        std::fs::create_dir_all(root.path().join(".github/workflows")).unwrap();
        std::fs::write(
            root.path().join(".github/workflows/ci.yml"),
            "name: CI\non: [push]\njobs:\n  test:\n    runs-on: ubuntu-latest\n",
        )
        .unwrap();

        let changes = vec![(
            PathBuf::from(".github/workflows/ci.yml"),
            FileChangeKind::Modified,
        )];
        let result = validate_changed_packages(root.path(), &packages, &changes, "test-env");

        assert!(
            result.is_ok(),
            "a workflow file is not a package spec: {result:?}"
        );
    }

    // The other half of the same rule: a `.yml` sitting in a subdirectory of
    // the package directory is not a spec either, because the repository lists
    // that directory without descending into it.
    #[test]
    fn yaml_below_the_package_directory_is_not_a_spec() {
        use super::{FileChangeKind, validate_changed_packages};

        let root = tempfile::tempdir().unwrap();
        let packages = root.path().join("packages");
        std::fs::create_dir_all(packages.join("starship")).unwrap();
        std::fs::write(
            packages.join("starship/config.yml"),
            "format: \"$all\"\nnot_a_selfie_spec: true\n",
        )
        .unwrap();

        let changes = vec![(
            PathBuf::from("packages/starship/config.yml"),
            FileChangeKind::Modified,
        )];
        let result = validate_changed_packages(root.path(), &packages, &changes, "test-env");

        assert!(
            result.is_ok(),
            "a dotfile source that happens to be YAML is not a spec: {result:?}"
        );
    }

    // A dotfile source is associated with its package by looking for a spec of
    // the same name in the repo root. Probing `{name}.yml` and `{name}.yaml`
    // answers for one capitalization only, so a spec stored as `Starship.YML`
    // left `starship/starship.toml` ungrouped and out of the push.
    //
    // Only a case-sensitive file system separates the two implementations: on
    // one that folds case, `starship.yml` resolves to `Starship.YML` and the
    // path probe is rescued by the file system rather than by being right.
    // Skipping is loud, because a quietly-skipped test reads as a passing one.
    #[test]
    fn a_dotfile_source_finds_its_spec_under_any_capitalization() {
        use super::{spec_named, spec_names_in};

        let root = tempfile::tempdir().unwrap();
        let spec = "name: starship\nenvironments:\n  test-env:\n    install: \"true\"\n";
        std::fs::write(root.path().join("Starship.YML"), spec).unwrap();

        if root.path().join("starship.yml").exists() {
            eprintln!(
                "SKIPPED a_dotfile_source_finds_its_spec_under_any_capitalization: this file \
                 system folds case, so a path probe finds the spec without folding names"
            );
            return;
        }

        let spec_names = spec_names_in(root.path());

        assert!(
            spec_named(&spec_names, "starship").is_some(),
            "a spec stored as `Starship.YML` must answer to `starship`"
        );
        assert!(
            spec_named(&spec_names, "neovim").is_none(),
            "a name with no spec behind it must not match"
        );
    }

    // The end-to-end half of the extension case, and it runs on every machine:
    // unlike two capitalizations, `neovim.yml` and `neovim.yaml` coexist on a
    // case-insensitive file system too. Nothing here needs a skip.
    #[test]
    fn a_push_touching_two_extensions_of_one_name_is_refused() {
        use super::{FileChangeKind, validate_changed_packages};
        use std::path::PathBuf;

        let root = tempfile::tempdir().unwrap();
        let spec = "name: neovim\nenvironments:\n  test-env:\n    install: \"true\"\n";
        std::fs::write(root.path().join("neovim.yml"), spec).unwrap();
        std::fs::write(root.path().join("neovim.yaml"), spec).unwrap();
        assert_eq!(
            std::fs::read_dir(root.path()).unwrap().count(),
            2,
            "both files must exist for this to test anything"
        );

        let changes = vec![(PathBuf::from("neovim.yml"), FileChangeKind::Modified)];
        let result = validate_changed_packages(root.path(), root.path(), &changes, "test-env");

        let Err(error) = result else {
            panic!("a push carrying two extensions of one name must be refused");
        };
        let rendered = format!("{error:?}");
        assert!(rendered.contains("NameCollision"), "got: {rendered}");
        assert!(rendered.contains("neovim.yaml"), "got: {rendered}");
        assert!(rendered.contains("neovim.yml"), "got: {rendered}");
        assert!(rendered.contains("different extensions"), "got: {rendered}");
    }

    // A package name is the stem, so two extensions claim one name exactly as
    // two capitalizations do. The loader reports the ambiguity, but only on the
    // machine that already has both files; push is where it can still be kept
    // out of everyone else's clone.
    #[test]
    fn the_same_stem_under_two_extensions_is_one_package() {
        let groups = group(&["neovim.yml", "neovim.yaml"]);
        assert_eq!(groups.len(), 1, "got: {groups:?}");
        assert_eq!(groups[0].1, vec!["neovim.yaml", "neovim.yml"]);
    }

    // Every way of spelling one name lands in a single group, so the refusal
    // names all of them at once rather than surfacing a new pair per push.
    #[test]
    fn case_and_extension_differences_group_together() {
        let groups = group(&["Neovim.yml", "neovim.YAML", "neovim.yml"]);
        assert_eq!(groups.len(), 1, "got: {groups:?}");
        assert_eq!(groups[0].1.len(), 3);
    }

    // Saying a `.yml`/`.yaml` pair will be discarded on checkout is false --
    // both survive -- and sends the reader hunting for a capitalization
    // difference that is not there.
    #[test]
    fn the_extension_refusal_does_not_blame_capitalization() {
        let rendered =
            name_collision_message(&["neovim.yaml".to_string(), "neovim.yml".to_string()]);

        assert!(rendered.contains("different extensions"), "got: {rendered}");
        assert!(!rendered.contains("case-insensitive"), "got: {rendered}");
        assert!(!rendered.contains("discards"), "got: {rendered}");
    }

    // The case refusal keeps the consequence that makes it urgent: a file is
    // destroyed on checkout, not merely a name left unresolvable.
    #[test]
    fn the_case_refusal_names_the_file_that_is_lost() {
        let rendered =
            name_collision_message(&["Neovim.yml".to_string(), "neovim.yml".to_string()]);

        assert!(rendered.contains("case-insensitive"), "got: {rendered}");
        assert!(rendered.contains("discards"), "got: {rendered}");
    }

    // Folding is Unicode, matching the loader's name comparison.
    #[test]
    fn folding_is_not_ascii_only() {
        assert_eq!(group(&["\u{dc}nicode.yml", "\u{fc}nicode.yml"]).len(), 1);
    }

    // Three-way collisions report every file, not just the first pair.
    #[test]
    fn every_colliding_file_is_reported() {
        let groups = group(&["NEOVIM.yml", "Neovim.yml", "neovim.yml"]);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].1.len(), 3);
    }

    // A collision already on disk is not created by the push that carries it,
    // and a change to a dotfile source touches no spec at all. Scanning only
    // the directories of changed specs would look everywhere except the one
    // place specs live.
    #[test]
    fn a_push_carrying_only_dotfile_sources_still_sees_a_collision() {
        use super::{FileChangeKind, validate_changed_packages};
        use std::path::PathBuf;

        let root = tempfile::tempdir().unwrap();
        let spec = "name: neovim\nenvironments:\n  test-env:\n    install: \"true\"\n";
        std::fs::write(root.path().join("Neovim.yml"), spec).unwrap();
        std::fs::write(root.path().join("neovim.yml"), spec).unwrap();

        if std::fs::read_dir(root.path()).unwrap().count() < 2 {
            eprintln!(
                "SKIPPED a_push_carrying_only_dotfile_sources_still_sees_a_collision: this \
                 file system folds case, so the pair cannot be created"
            );
            return;
        }

        std::fs::create_dir_all(root.path().join("starship")).unwrap();
        std::fs::write(root.path().join("starship/starship.toml"), "x = 1\n").unwrap();

        // Nothing here is a spec, so the per-file loop never reads the root.
        let changes = vec![(
            PathBuf::from("starship/starship.toml"),
            FileChangeKind::Modified,
        )];
        let result = validate_changed_packages(root.path(), root.path(), &changes, "test-env");

        let Err(error) = result else {
            panic!("a push carrying a case collision must be refused");
        };
        let rendered = format!("{error:?}");
        assert!(rendered.contains("NameCollision"), "got: {rendered}");
        assert!(rendered.contains("Neovim.yml"), "got: {rendered}");
    }

    // The repo is discovered by walking up from the package directory, so the
    // two are the same directory only when the package directory happens to be
    // the top of the repository. Scanning the root alone looks at a directory
    // that holds no specs and lets the collision through.
    #[test]
    fn a_package_directory_below_the_repo_root_is_still_scanned() {
        use super::{FileChangeKind, validate_changed_packages};
        use std::path::PathBuf;

        let root = tempfile::tempdir().unwrap();
        let packages = root.path().join("packages");
        std::fs::create_dir_all(&packages).unwrap();
        let spec = "name: neovim\nenvironments:\n  test-env:\n    install: \"true\"\n";
        std::fs::write(packages.join("neovim.yml"), spec).unwrap();
        std::fs::write(packages.join("neovim.yaml"), spec).unwrap();

        std::fs::write(root.path().join("README.md"), "hi\n").unwrap();
        let changes = vec![(PathBuf::from("README.md"), FileChangeKind::Modified)];
        let result = validate_changed_packages(root.path(), &packages, &changes, "test-env");

        let Err(error) = result else {
            panic!("a push must be refused over a collision in the package directory");
        };
        let rendered = format!("{error:?}");
        assert!(rendered.contains("NameCollision"), "got: {rendered}");
        assert!(rendered.contains("neovim.yaml"), "got: {rendered}");
    }

    // The package repository folds the extension as well as the stem, so
    // `Neovim.YML` loads as package `neovim`. Everything push does with a spec
    // is gated on this predicate, so a file it fails to recognize is skipped by
    // the collision check and by per-file validation while remaining a spec --
    // which is how an uppercase extension would carry a colliding pair through
    // a push that reports no problem.
    #[test]
    fn an_uppercase_extension_still_names_a_spec() {
        use super::is_spec_file;
        use std::path::Path;

        assert!(is_spec_file(Path::new("Neovim.YML")));
        assert!(is_spec_file(Path::new("neovim.Yaml")));
        assert!(is_spec_file(Path::new("neovim.yml")));
        assert!(is_spec_file(Path::new("neovim.yaml")));
        assert!(!is_spec_file(Path::new("neovim.toml")));
        assert!(!is_spec_file(Path::new("neovim")));
    }
}
