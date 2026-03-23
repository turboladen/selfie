//! gix-based implementation of [`GitStatusProvider`] and [`GitSyncProvider`].
//!
//! Uses the `gix` crate for native local git operations (discover, status, stage,
//! commit, diff) and shells out to the `git` binary for network operations (push,
//! fetch) to leverage the user's existing SSH/credential configuration.

use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    process::Command,
};

use super::status_provider::{
    GitDirectoryStatus, GitFileStatus, GitStatusError, GitStatusProvider,
};
use super::sync_provider::{
    ChangeType, ChangedFile, CommitId, FastForwardResult, GitSyncError, GitSyncProvider, RepoInfo,
    RepoStatus,
};

/// Git adapter backed by the `gix` crate (local ops) and `git` CLI (network ops).
#[derive(Debug, Clone)]
pub struct GixGitAdapter;

// ─── Helper ──────────────────────────────────────────────────────────────────

/// Open a `gix` repo from a path, mapping errors to `GitSyncError`.
///
/// Distinguishes "not a git repository" from real I/O or corruption errors,
/// matching the error discrimination in `GitStatusProvider::status_for_directory`.
fn open_repo(path: &Path) -> Result<gix::Repository, GitSyncError> {
    gix::discover(path).map_err(|e| match e {
        gix::discover::Error::Discover(_) => GitSyncError::NotARepo {
            path: path.to_path_buf(),
        },
        other => GitSyncError::OperationFailed {
            operation: "discover repository".to_string(),
            message: other.to_string(),
        },
    })
}

/// Run a `git` CLI command in the given directory.
fn run_git(repo_root: &Path, args: &[&str]) -> Result<String, GitSyncError> {
    let output = Command::new("git")
        .args(args)
        .current_dir(repo_root)
        .output()
        .map_err(|e| GitSyncError::OperationFailed {
            operation: format!("git {}", args.join(" ")),
            message: e.to_string(),
        })?;

    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        Err(GitSyncError::OperationFailed {
            operation: format!("git {}", args.join(" ")),
            message: stderr,
        })
    }
}

// ─── GitStatusProvider (read-only, existing) ─────────────────────────────────

impl GitStatusProvider for GixGitAdapter {
    fn status_for_directory(&self, directory: &Path) -> Result<GitDirectoryStatus, GitStatusError> {
        let repo = match gix::discover(directory) {
            Ok(repo) => repo,
            Err(gix::discover::Error::Discover(_)) => {
                return Ok(GitDirectoryStatus {
                    in_repo: false,
                    files: HashMap::new(),
                });
            }
            Err(other) => {
                return Err(GitStatusError::StatusError(other.to_string()));
            }
        };

        let workdir = repo
            .workdir()
            .ok_or_else(|| GitStatusError::StatusError("bare repository".to_string()))?;
        let workdir = dunce::canonicalize(workdir).unwrap_or_else(|_| workdir.to_path_buf());

        let platform = repo
            .status(gix::progress::Discard)
            .map_err(|e| GitStatusError::StatusError(e.to_string()))?;

        let iter = platform
            .into_iter(Vec::new())
            .map_err(|e| GitStatusError::StatusError(e.to_string()))?;

        let mut worktree_changes: HashSet<String> = HashSet::new();
        let mut index_changes: HashSet<String> = HashSet::new();
        let mut untracked: Vec<String> = Vec::new();

        for item in iter {
            let item = item.map_err(|e| GitStatusError::StatusError(e.to_string()))?;

            match item {
                gix::status::Item::IndexWorktree(iw_item) => {
                    use gix::status::index_worktree::Item;
                    match iw_item {
                        Item::Modification {
                            rela_path: relative_path,
                            ..
                        } => {
                            worktree_changes.insert(relative_path.to_string());
                        }
                        Item::DirectoryContents { entry, .. } => {
                            if entry.status == gix::dir::entry::Status::Untracked {
                                untracked.push(entry.rela_path.to_string());
                            }
                        }
                        Item::Rewrite { .. } => {}
                    }
                }
                gix::status::Item::TreeIndex(change) => {
                    let location = change.fields().0.to_string();
                    index_changes.insert(location);
                }
            }
        }

        let mut files = HashMap::new();

        for path in &untracked {
            let abs_path = workdir.join(path);
            files.insert(abs_path, GitFileStatus::Untracked);
        }

        for path in &worktree_changes {
            let abs_path = workdir.join(path);
            if index_changes.contains(path) {
                files.insert(abs_path, GitFileStatus::StagedAndModified);
            } else {
                files.insert(abs_path, GitFileStatus::Modified);
            }
        }

        for path in &index_changes {
            if !worktree_changes.contains(path) {
                let abs_path = workdir.join(path);
                files.insert(abs_path, GitFileStatus::Staged);
            }
        }

        Ok(GitDirectoryStatus {
            in_repo: true,
            files,
        })
    }
}

// ─── GitSyncProvider (write + network) ───────────────────────────────────────

impl GitSyncProvider for GixGitAdapter {
    fn discover_repo(&self, path: &Path) -> Result<RepoInfo, GitSyncError> {
        let repo = open_repo(path)?;

        let root = repo
            .workdir()
            .ok_or_else(|| GitSyncError::OperationFailed {
                operation: "discover".to_string(),
                message: "bare repository".to_string(),
            })?
            .to_path_buf();
        let root = dunce::canonicalize(&root).unwrap_or(root);

        // Get branch name via git CLI (more reliable across gix versions)
        let branch = run_git(&root, &["rev-parse", "--abbrev-ref", "HEAD"])
            .ok()
            .filter(|b| b != "HEAD"); // Detached HEAD → None

        // Get remote name via git CLI
        let remote_name = run_git(&root, &["remote"])
            .ok()
            .and_then(|output| output.lines().next().map(String::from));

        Ok(RepoInfo {
            root,
            branch,
            remote_name,
        })
    }

    fn repo_status(&self, repo_root: &Path) -> Result<RepoStatus, GitSyncError> {
        let repo = open_repo(repo_root)?;

        let platform =
            repo.status(gix::progress::Discard)
                .map_err(|e| GitSyncError::OperationFailed {
                    operation: "status".to_string(),
                    message: e.to_string(),
                })?;

        let iter = platform
            .into_iter(Vec::new())
            .map_err(|e| GitSyncError::OperationFailed {
                operation: "status".to_string(),
                message: e.to_string(),
            })?;

        let mut modified = Vec::new();
        let mut staged = Vec::new();
        let mut untracked = Vec::new();
        let mut deleted = Vec::new();

        // Track which paths have worktree vs index changes for proper classification
        let mut worktree_mods: HashSet<String> = HashSet::new();
        let mut index_mods: HashSet<String> = HashSet::new();

        for item in iter {
            let item = item.map_err(|e| GitSyncError::OperationFailed {
                operation: "status".to_string(),
                message: e.to_string(),
            })?;

            match item {
                gix::status::Item::IndexWorktree(iw_item) => {
                    use gix::status::index_worktree::Item;
                    match iw_item {
                        Item::Modification { rela_path, .. } => {
                            worktree_mods.insert(rela_path.to_string());
                        }
                        Item::DirectoryContents { entry, .. } => {
                            if entry.status == gix::dir::entry::Status::Untracked {
                                untracked.push(PathBuf::from(entry.rela_path.to_string()));
                            }
                        }
                        Item::Rewrite { .. } => {}
                    }
                }
                gix::status::Item::TreeIndex(change) => {
                    let location = change.fields().0.to_string();
                    index_mods.insert(location);
                }
            }
        }

        // Build final lists
        for path_str in &worktree_mods {
            modified.push(PathBuf::from(path_str));
        }
        for path_str in &index_mods {
            staged.push(PathBuf::from(path_str));
        }

        // Detect deleted files via git CLI (gix status doesn't surface deletions cleanly)
        if let Ok(output) = run_git(repo_root, &["ls-files", "--deleted"]) {
            for line in output.lines() {
                if !line.is_empty() {
                    deleted.push(PathBuf::from(line));
                }
            }
        }

        // Ahead/behind via git CLI (simpler than gix for remote tracking)
        let (ahead, behind) = self.ahead_behind_counts(repo_root);

        Ok(RepoStatus {
            modified,
            staged,
            untracked,
            deleted,
            ahead,
            behind,
        })
    }

    fn stage_files(&self, repo_root: &Path, files: &[PathBuf]) -> Result<(), GitSyncError> {
        if files.is_empty() {
            return Ok(());
        }

        // Use git CLI for staging — gix index manipulation is complex
        let mut args = vec!["add", "--"];
        let file_strs: Vec<String> = files
            .iter()
            .map(|f| f.to_string_lossy().to_string())
            .collect();
        let file_refs: Vec<&str> = file_strs.iter().map(String::as_str).collect();
        args.extend(file_refs);

        run_git(repo_root, &args)?;
        Ok(())
    }

    fn commit(&self, repo_root: &Path, message: &str) -> Result<CommitId, GitSyncError> {
        let output = run_git(repo_root, &["commit", "-m", message])?;

        // Extract commit hash from git output (first line typically contains it)
        // Format: "[branch hash] message"
        let hash = output
            .split_whitespace()
            .nth(1)
            .unwrap_or("unknown")
            .trim_end_matches(']')
            .to_string();

        Ok(CommitId(hash))
    }

    fn push(&self, repo_root: &Path) -> Result<(), GitSyncError> {
        run_git(repo_root, &["push"]).map_err(|e| {
            // Detect common push failure: remote has new commits
            let msg = e.to_string();
            if msg.contains("rejected") || msg.contains("non-fast-forward") {
                GitSyncError::OperationFailed {
                    operation: "push".to_string(),
                    message: "Remote has new commits. Run 'selfie sync pull' first.".to_string(),
                }
            } else {
                e
            }
        })?;
        Ok(())
    }

    fn fetch(&self, repo_root: &Path) -> Result<(), GitSyncError> {
        run_git(repo_root, &["fetch"])?;
        Ok(())
    }

    fn fast_forward(&self, repo_root: &Path) -> Result<FastForwardResult, GitSyncError> {
        // Get current HEAD before merge
        let old_head = run_git(repo_root, &["rev-parse", "HEAD"])?;

        // Try fast-forward merge
        match run_git(repo_root, &["merge", "--ff-only", "@{u}"]) {
            Ok(output) => {
                if output.contains("Already up to date") {
                    return Ok(FastForwardResult::AlreadyUpToDate);
                }

                let new_head = run_git(repo_root, &["rev-parse", "HEAD"])?;
                let commit_count = run_git(
                    repo_root,
                    &["rev-list", "--count", &format!("{old_head}..{new_head}")],
                )?
                .parse::<usize>()
                .unwrap_or(0);

                Ok(FastForwardResult::Advanced {
                    from: CommitId(old_head),
                    to: CommitId(new_head),
                    commit_count,
                })
            }
            Err(e) => {
                let msg = e.to_string();
                if msg.contains("Not possible to fast-forward") || msg.contains("divergent") {
                    Ok(FastForwardResult::Diverged)
                } else {
                    Err(e)
                }
            }
        }
    }

    fn diff_commits(
        &self,
        repo_root: &Path,
        from: &CommitId,
        to: &CommitId,
    ) -> Result<Vec<ChangedFile>, GitSyncError> {
        let output = run_git(repo_root, &["diff", "--name-status", &from.0, &to.0])?;

        let mut changes = Vec::new();
        for line in output.lines() {
            let mut parts = line.splitn(2, '\t');
            let status = parts.next().unwrap_or("");
            let path = parts.next().unwrap_or("");

            if path.is_empty() {
                continue;
            }

            let change_type = match status {
                "A" => ChangeType::Added,
                "M" => ChangeType::Modified,
                "D" => ChangeType::Deleted,
                _ => ChangeType::Modified, // R, C, etc. → treat as modified
            };

            changes.push(ChangedFile {
                path: PathBuf::from(path),
                change_type,
            });
        }

        Ok(changes)
    }
}

impl GixGitAdapter {
    /// Get ahead/behind counts relative to the upstream tracking branch.
    /// Returns (0, 0) if no upstream is configured.
    fn ahead_behind_counts(&self, repo_root: &Path) -> (usize, usize) {
        let output = run_git(
            repo_root,
            &["rev-list", "--left-right", "--count", "HEAD...@{u}"],
        );

        match output {
            Ok(s) => {
                let mut parts = s.split_whitespace();
                let ahead = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
                let behind = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
                (ahead, behind)
            }
            Err(_) => (0, 0), // No upstream configured
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    // ─── GitStatusProvider tests ─────────────────────────────────────────────

    #[test]
    fn non_git_directory_returns_not_in_repo() {
        let temp = tempfile::TempDir::new().unwrap();
        let provider = GixGitAdapter;
        let result = provider.status_for_directory(temp.path()).unwrap();
        assert!(!result.in_repo);
        assert!(result.files.is_empty());
    }

    #[test]
    fn git_repo_detected_as_in_repo() {
        let temp = tempfile::TempDir::new().unwrap();
        gix::init(temp.path()).unwrap();

        let provider = GixGitAdapter;
        let result = provider.status_for_directory(temp.path()).unwrap();
        assert!(result.in_repo);
    }

    #[test]
    fn untracked_file_detected() {
        let temp = tempfile::TempDir::new().unwrap();
        gix::init(temp.path()).unwrap();

        let file_path = temp.path().join("untracked.yml");
        fs::write(&file_path, "name: untracked\n").unwrap();

        let provider = GixGitAdapter;
        let result = provider.status_for_directory(temp.path()).unwrap();

        assert!(result.in_repo);
        assert_eq!(result.status_for_file(&file_path), GitFileStatus::Untracked);
    }

    // ─── GitSyncProvider tests ───────────────────────────────────────────────

    #[test]
    fn discover_repo_not_found() {
        let temp = tempfile::TempDir::new().unwrap();
        let adapter = GixGitAdapter;
        let result = adapter.discover_repo(temp.path());
        assert!(result.is_err());
    }

    #[test]
    fn discover_repo_finds_root() {
        let temp = tempfile::TempDir::new().unwrap();
        gix::init(temp.path()).unwrap();

        let adapter = GixGitAdapter;
        let info = adapter.discover_repo(temp.path()).unwrap();

        let expected = dunce::canonicalize(temp.path()).unwrap();
        assert_eq!(info.root, expected);
    }

    #[test]
    fn repo_status_clean() {
        let temp = tempfile::TempDir::new().unwrap();
        gix::init(temp.path()).unwrap();

        let adapter = GixGitAdapter;
        let status = adapter.repo_status(temp.path()).unwrap();

        assert!(status.is_clean());
    }

    #[test]
    fn repo_status_detects_untracked() {
        let temp = tempfile::TempDir::new().unwrap();
        gix::init(temp.path()).unwrap();

        fs::write(temp.path().join("new.yml"), "name: new\n").unwrap();

        let adapter = GixGitAdapter;
        let status = adapter.repo_status(temp.path()).unwrap();

        assert!(status.is_dirty());
        assert_eq!(status.untracked.len(), 1);
    }

    #[test]
    fn stage_files_works() {
        let temp = tempfile::TempDir::new().unwrap();
        gix::init(temp.path()).unwrap();

        let file = temp.path().join("test.yml");
        fs::write(&file, "name: test\n").unwrap();

        let adapter = GixGitAdapter;
        adapter
            .stage_files(temp.path(), &[PathBuf::from("test.yml")])
            .unwrap();

        // Verify file is staged via status
        let status = adapter.repo_status(temp.path()).unwrap();
        assert!(!status.staged.is_empty() || status.untracked.is_empty());
    }

    #[test]
    fn commit_creates_commit() {
        let temp = tempfile::TempDir::new().unwrap();
        gix::init(temp.path()).unwrap();

        // Configure git user for commit (disable GPG signing for test isolation)
        run_git(temp.path(), &["config", "user.email", "test@test.com"]).unwrap();
        run_git(temp.path(), &["config", "user.name", "Test"]).unwrap();
        run_git(temp.path(), &["config", "commit.gpgsign", "false"]).unwrap();

        let file = temp.path().join("test.yml");
        fs::write(&file, "name: test\n").unwrap();

        let adapter = GixGitAdapter;
        adapter
            .stage_files(temp.path(), &[PathBuf::from("test.yml")])
            .unwrap();

        let result = adapter.commit(temp.path(), "test commit");
        assert!(result.is_ok(), "commit should succeed: {result:?}");
    }

    #[test]
    fn diff_commits_detects_changes() {
        let temp = tempfile::TempDir::new().unwrap();
        gix::init(temp.path()).unwrap();

        run_git(temp.path(), &["config", "user.email", "test@test.com"]).unwrap();
        run_git(temp.path(), &["config", "user.name", "Test"]).unwrap();
        run_git(temp.path(), &["config", "commit.gpgsign", "false"]).unwrap();

        // First commit
        let file = temp.path().join("a.yml");
        fs::write(&file, "name: a\n").unwrap();
        run_git(temp.path(), &["add", "a.yml"]).unwrap();
        run_git(temp.path(), &["commit", "-m", "first"]).unwrap();
        let first = run_git(temp.path(), &["rev-parse", "HEAD"]).unwrap();

        // Second commit — add a new file
        let file2 = temp.path().join("b.yml");
        fs::write(&file2, "name: b\n").unwrap();
        run_git(temp.path(), &["add", "b.yml"]).unwrap();
        run_git(temp.path(), &["commit", "-m", "second"]).unwrap();
        let second = run_git(temp.path(), &["rev-parse", "HEAD"]).unwrap();

        let adapter = GixGitAdapter;
        let changes = adapter
            .diff_commits(temp.path(), &CommitId(first), &CommitId(second))
            .unwrap();

        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].path, PathBuf::from("b.yml"));
        assert_eq!(changes[0].change_type, ChangeType::Added);
    }
}
