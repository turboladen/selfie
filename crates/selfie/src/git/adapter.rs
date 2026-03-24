//! gix-based implementation of [`GitStatusProvider`] and [`GitSyncProvider`].
//!
//! Uses the `gix` crate for local git operations (discover, status, stage,
//! commit) and shells out to the `git` binary for:
//! - **Network operations** (push, fetch) — to leverage the user's existing
//!   SSH agent and credential helpers, which gix's transport layer doesn't
//!   automatically integrate with.
//! - **Fast-forward merge** — because it updates both the ref and the working
//!   tree, requiring checkout logic (`gix-worktree-state`) we don't pull in.
//! - **Ancillary queries** (branch name, remote, ahead/behind, deleted files,
//!   diff) — tracked in selfie-y9n for conversion to native gix.

use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    process::{Command, Stdio},
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

// ─── Helpers ─────────────────────────────────────────────────────────────────

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

/// Check if a file has the executable bit set.
#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|m| m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable(_path: &Path) -> bool {
    false
}

/// Shorthand for creating a [`GitSyncError::OperationFailed`] from any error.
fn git_sync_err(operation: &str, e: impl std::fmt::Display) -> GitSyncError {
    GitSyncError::OperationFailed {
        operation: operation.to_string(),
        message: e.to_string(),
    }
}

/// Run a `git` CLI command in the given directory.
///
/// Stdin is set to `Stdio::null` and `GIT_TERMINAL_PROMPT=0` to prevent git
/// from blocking on credential prompts, which is critical for non-interactive
/// callers like the MCP server.
fn run_git(repo_root: &Path, args: &[&str]) -> Result<String, GitSyncError> {
    let output = Command::new("git")
        .args(args)
        .current_dir(repo_root)
        .stdin(Stdio::null())
        .env("GIT_TERMINAL_PROMPT", "0")
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

/// Intermediate representation of gix status output, shared between
/// [`GitStatusProvider::status_for_directory`] and
/// [`GitSyncProvider::repo_status`].
struct RawStatus {
    worktree_mods: HashSet<String>,
    index_mods: HashSet<String>,
    untracked: Vec<String>,
}

/// Iterate gix status items and classify them into worktree modifications,
/// index (staged) modifications, and untracked files.
fn collect_raw_status(repo: &gix::Repository) -> Result<RawStatus, String> {
    let platform = repo
        .status(gix::progress::Discard)
        .map_err(|e| e.to_string())?;

    let iter = platform
        .into_iter(std::iter::empty::<gix::bstr::BString>())
        .map_err(|e| e.to_string())?;

    let mut worktree_mods = HashSet::new();
    let mut index_mods = HashSet::new();
    let mut untracked = Vec::new();

    for item in iter {
        let item = item.map_err(|e| e.to_string())?;

        match item {
            gix::status::Item::IndexWorktree(iw_item) => {
                use gix::status::index_worktree::Item;
                match iw_item {
                    Item::Modification { rela_path, .. } => {
                        worktree_mods.insert(rela_path.to_string());
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
                let location = change.location().to_string();
                index_mods.insert(location);
            }
        }
    }

    Ok(RawStatus {
        worktree_mods,
        index_mods,
        untracked,
    })
}

/// Parse a single line from `git diff --name-status` output into a
/// [`ChangedFile`], returning `None` for blank or unparseable lines.
///
/// Rename/copy lines have the format `R100\told\tnew` (three tab-separated
/// fields). We take the *last* field (the new path) for those statuses.
fn parse_diff_line(line: &str) -> Option<ChangedFile> {
    let fields: Vec<&str> = line.split('\t').collect();
    if fields.len() < 2 {
        return None;
    }

    let status = fields[0];
    let change_type = match status {
        "A" => ChangeType::Added,
        "M" => ChangeType::Modified,
        "D" => ChangeType::Deleted,
        _ => ChangeType::Modified, // R*, C*, etc. → treat as modified
    };

    // For rename/copy (R*, C*) use the last field (new path); otherwise use
    // the second field.
    let path = fields.last().filter(|p| !p.is_empty())?;

    Some(ChangedFile {
        path: PathBuf::from(path),
        change_type,
    })
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

        let raw = collect_raw_status(&repo).map_err(GitStatusError::StatusError)?;

        let mut files = HashMap::new();

        for path in &raw.untracked {
            files.insert(workdir.join(path), GitFileStatus::Untracked);
        }

        for path in &raw.worktree_mods {
            let status = if raw.index_mods.contains(path) {
                GitFileStatus::StagedAndModified
            } else {
                GitFileStatus::Modified
            };
            files.insert(workdir.join(path), status);
        }

        for path in &raw.index_mods {
            if !raw.worktree_mods.contains(path) {
                files.insert(workdir.join(path), GitFileStatus::Staged);
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

        let raw = collect_raw_status(&repo).map_err(|msg| GitSyncError::OperationFailed {
            operation: "status".to_string(),
            message: msg,
        })?;

        let modified = raw.worktree_mods.iter().map(PathBuf::from).collect();
        let staged = raw.index_mods.iter().map(PathBuf::from).collect();
        let untracked = raw.untracked.iter().map(PathBuf::from).collect();

        // Detect deleted files via git CLI (gix status doesn't surface deletions cleanly)
        let mut deleted = Vec::new();
        if let Ok(output) = run_git(repo_root, &["ls-files", "--deleted"]) {
            for line in output.lines().filter(|l| !l.is_empty()) {
                deleted.push(PathBuf::from(line));
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

        let repo = open_repo(repo_root)?;
        let mut index = repo
            .open_index()
            .map_err(|e| git_sync_err("open index", e))?;

        let workdir = repo
            .workdir()
            .ok_or_else(|| GitSyncError::OperationFailed {
                operation: "stage".to_string(),
                message: "bare repository".to_string(),
            })?;

        for file in files {
            let full_path = workdir.join(file);
            let rela_bstring = gix::path::into_bstr(file);
            let rela_path: &gix::bstr::BStr = rela_bstring.as_ref();

            if full_path.exists() {
                // File exists → hash it as a blob and upsert into the index.
                let data = std::fs::read(&full_path).map_err(|e| git_sync_err("read file", e))?;
                let blob_id = repo
                    .write_blob(&data)
                    .map_err(|e| git_sync_err("write blob", e))?;

                let entry_mode = if is_executable(&full_path) {
                    gix::index::entry::Mode::FILE_EXECUTABLE
                } else {
                    gix::index::entry::Mode::FILE
                };

                match index.entry_index_by_path_and_stage(
                    rela_path,
                    gix::index::entry::Stage::Unconflicted,
                ) {
                    Some(pos) => {
                        // Update existing entry in place.
                        let entry = &mut index.entries_mut()[pos];
                        entry.id = blob_id.detach();
                        entry.mode = entry_mode;
                        entry.flags.remove(
                            gix::index::entry::Flags::ASSUME_VALID
                                | gix::index::entry::Flags::UPDATE,
                        );
                    }
                    None => {
                        // New file — push and re-sort.
                        index.dangerously_push_entry(
                            Default::default(),
                            blob_id.detach(),
                            gix::index::entry::Flags::empty(),
                            entry_mode,
                            rela_path,
                        );
                        index.sort_entries();
                    }
                }
            } else {
                // File deleted → remove from index.
                let rela_owned = rela_path.to_owned();
                index.remove_entries(|_, path, _| path == rela_owned);
            }
        }

        index
            .write(Default::default())
            .map_err(|e| git_sync_err("write index", e))?;
        Ok(())
    }

    fn commit(&self, repo_root: &Path, message: &str) -> Result<CommitId, GitSyncError> {
        let repo = open_repo(repo_root)?;
        let index = repo
            .open_index()
            .map_err(|e| git_sync_err("open index", e))?;

        // Build a tree from the current index by starting from HEAD's tree
        // (or the empty tree for the first commit) and upserting every entry.
        let head_tree_id = repo
            .head_tree_id_or_empty()
            .map_err(|e| git_sync_err("head tree", e))?;
        let mut editor = repo
            .edit_tree(head_tree_id)
            .map_err(|e| git_sync_err("edit tree", e))?;

        // Collect HEAD tree entries so we can detect deletions.
        let head_entries: HashSet<String> = repo
            .find_tree(head_tree_id)
            .ok()
            .map(|tree| {
                tree.traverse()
                    .breadthfirst
                    .files()
                    .unwrap_or_default()
                    .into_iter()
                    .map(|r| r.filepath.to_string())
                    .collect()
            })
            .unwrap_or_default();

        // Upsert all index entries into the tree.
        let index_paths: HashSet<String> = index
            .entries()
            .iter()
            .map(|e| e.path(&index).to_string())
            .collect();

        for entry in index.entries() {
            let path = entry.path(&index);
            let kind = match entry.mode {
                gix::index::entry::Mode::FILE_EXECUTABLE => {
                    gix::object::tree::EntryKind::BlobExecutable
                }
                gix::index::entry::Mode::SYMLINK => gix::object::tree::EntryKind::Link,
                _ => gix::object::tree::EntryKind::Blob,
            };
            editor
                .upsert(path.to_owned(), kind, entry.id)
                .map_err(|e| git_sync_err("upsert tree entry", e))?;
        }

        // Remove entries that were in HEAD but are no longer in the index.
        for path in &head_entries {
            if !index_paths.contains(path) {
                let bpath: &gix::bstr::BStr = path.as_str().into();
                editor
                    .remove(bpath.to_owned())
                    .map_err(|e| git_sync_err("remove tree entry", e))?;
            }
        }

        let tree_id = editor.write().map_err(|e| git_sync_err("write tree", e))?;

        // Determine parent(s).
        let parents: Vec<gix::ObjectId> = repo
            .head_id()
            .map(|id| vec![id.detach()])
            .unwrap_or_default();

        let commit_id = repo
            .commit("HEAD", message, tree_id, parents)
            .map_err(|e| git_sync_err("commit", e))?;

        Ok(CommitId(commit_id.to_string()))
    }

    fn push(&self, repo_root: &Path) -> Result<(), GitSyncError> {
        // Shells out because gix's transport layer (blocking-network-client)
        // doesn't automatically integrate with the user's SSH agent and
        // credential helpers the way the git CLI does.
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
        // Shells out for the same reason as push — SSH/credential integration.
        run_git(repo_root, &["fetch"])?;
        Ok(())
    }

    fn fast_forward(&self, repo_root: &Path) -> Result<FastForwardResult, GitSyncError> {
        // Shells out because ff-merge updates both the ref AND the working tree.
        // gix can update refs, but a full working-tree checkout (handling file
        // modes, symlinks, dirty-file guards) requires gix-worktree-state which
        // we don't pull in.
        let old_head = run_git(repo_root, &["rev-parse", "HEAD"])?;

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
        Ok(output.lines().filter_map(parse_diff_line).collect())
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

    /// Create a temporary git repo, returning the tempdir handle and its path.
    fn init_repo() -> (tempfile::TempDir, PathBuf) {
        let temp = tempfile::TempDir::new().unwrap();
        gix::init(temp.path()).unwrap();
        let path = temp.path().to_path_buf();
        (temp, path)
    }

    /// Create a temporary git repo with an initial commit (so the index exists).
    fn init_repo_for_commits() -> (tempfile::TempDir, PathBuf) {
        let (temp, path) = init_repo();
        run_git(&path, &["config", "user.email", "test@test.com"]).unwrap();
        run_git(&path, &["config", "user.name", "Test"]).unwrap();
        run_git(&path, &["config", "commit.gpgsign", "false"]).unwrap();
        // Create an initial commit so the index file exists on disk.
        run_git(&path, &["commit", "--allow-empty", "-m", "initial"]).unwrap();
        (temp, path)
    }

    // ─── parse_diff_line tests ──────────────────────────────────────────────

    #[test]
    fn parse_diff_line_added() {
        let result = parse_diff_line("A\tnew_file.yml").unwrap();
        assert_eq!(result.path, PathBuf::from("new_file.yml"));
        assert_eq!(result.change_type, ChangeType::Added);
    }

    #[test]
    fn parse_diff_line_modified() {
        let result = parse_diff_line("M\texisting.yml").unwrap();
        assert_eq!(result.change_type, ChangeType::Modified);
    }

    #[test]
    fn parse_diff_line_deleted() {
        let result = parse_diff_line("D\tremoved.yml").unwrap();
        assert_eq!(result.change_type, ChangeType::Deleted);
    }

    #[test]
    fn parse_diff_line_rename_uses_new_path() {
        // Rename lines have three fields: R100\told\tnew
        let result = parse_diff_line("R100\told_name.yml\tnew_name.yml").unwrap();
        assert_eq!(result.change_type, ChangeType::Modified);
        assert_eq!(result.path, PathBuf::from("new_name.yml"));
    }

    #[test]
    fn parse_diff_line_rename_single_field_uses_that_path() {
        // Two-field rename (shouldn't happen in practice, but test graceful handling)
        let result = parse_diff_line("R100\trenamed.yml").unwrap();
        assert_eq!(result.path, PathBuf::from("renamed.yml"));
    }

    #[test]
    fn parse_diff_line_empty_returns_none() {
        assert!(parse_diff_line("").is_none());
    }

    #[test]
    fn parse_diff_line_no_tab_returns_none() {
        assert!(parse_diff_line("M").is_none());
    }

    // ─── GitStatusProvider tests ─────────────────────────────────────────────

    #[test]
    fn non_git_directory_returns_not_in_repo() {
        let temp = tempfile::TempDir::new().unwrap();
        let result = GixGitAdapter.status_for_directory(temp.path()).unwrap();
        assert!(!result.in_repo);
        assert!(result.files.is_empty());
    }

    #[test]
    fn git_repo_detected_as_in_repo() {
        let (_temp, path) = init_repo();
        let result = GixGitAdapter.status_for_directory(&path).unwrap();
        assert!(result.in_repo);
    }

    #[test]
    fn untracked_file_detected() {
        let (_temp, path) = init_repo();
        let file_path = path.join("untracked.yml");
        fs::write(&file_path, "name: untracked\n").unwrap();

        let result = GixGitAdapter.status_for_directory(&path).unwrap();

        assert!(result.in_repo);
        assert_eq!(result.status_for_file(&file_path), GitFileStatus::Untracked);
    }

    // ─── GitSyncProvider tests ───────────────────────────────────────────────

    #[test]
    fn discover_repo_not_found() {
        let temp = tempfile::TempDir::new().unwrap();
        assert!(GixGitAdapter.discover_repo(temp.path()).is_err());
    }

    #[test]
    fn discover_repo_finds_root() {
        let (_temp, path) = init_repo();
        let info = GixGitAdapter.discover_repo(&path).unwrap();

        let expected = dunce::canonicalize(&path).unwrap();
        assert_eq!(info.root, expected);
    }

    #[test]
    fn repo_status_clean() {
        let (_temp, path) = init_repo();
        let status = GixGitAdapter.repo_status(&path).unwrap();
        assert!(status.is_clean());
    }

    #[test]
    fn repo_status_detects_untracked() {
        let (_temp, path) = init_repo();
        fs::write(path.join("new.yml"), "name: new\n").unwrap();

        let status = GixGitAdapter.repo_status(&path).unwrap();

        assert!(status.is_dirty());
        assert_eq!(status.untracked.len(), 1);
    }

    #[test]
    fn stage_files_works() {
        let (_temp, path) = init_repo_for_commits();
        fs::write(path.join("test.yml"), "name: test\n").unwrap();

        GixGitAdapter
            .stage_files(&path, &[PathBuf::from("test.yml")])
            .unwrap();

        let status = GixGitAdapter.repo_status(&path).unwrap();
        assert!(!status.staged.is_empty() || status.untracked.is_empty());
    }

    #[test]
    fn commit_creates_commit() {
        let (_temp, path) = init_repo_for_commits();
        fs::write(path.join("test.yml"), "name: test\n").unwrap();

        GixGitAdapter
            .stage_files(&path, &[PathBuf::from("test.yml")])
            .unwrap();

        let result = GixGitAdapter.commit(&path, "test commit");
        assert!(result.is_ok(), "commit should succeed: {result:?}");
    }

    #[test]
    fn diff_commits_detects_changes() {
        let (_temp, path) = init_repo_for_commits();

        // First commit
        fs::write(path.join("a.yml"), "name: a\n").unwrap();
        run_git(&path, &["add", "a.yml"]).unwrap();
        run_git(&path, &["commit", "-m", "first"]).unwrap();
        let first = run_git(&path, &["rev-parse", "HEAD"]).unwrap();

        // Second commit
        fs::write(path.join("b.yml"), "name: b\n").unwrap();
        run_git(&path, &["add", "b.yml"]).unwrap();
        run_git(&path, &["commit", "-m", "second"]).unwrap();
        let second = run_git(&path, &["rev-parse", "HEAD"]).unwrap();

        let changes = GixGitAdapter
            .diff_commits(&path, &CommitId(first), &CommitId(second))
            .unwrap();

        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].path, PathBuf::from("b.yml"));
        assert_eq!(changes[0].change_type, ChangeType::Added);
    }
}
