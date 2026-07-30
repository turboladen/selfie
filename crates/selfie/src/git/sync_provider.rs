//! Git sync operations port.
//!
//! Defines the [`GitSyncProvider`] trait for git write/network operations
//! needed by the sync service (stage, commit, push, fetch, fast-forward, diff).
//!
//! This is separate from [`super::GitStatusProvider`] which provides read-only
//! status for annotating UI output. Both traits are implemented by a single
//! concrete adapter ([`super::GixGitAdapter`]), following interface segregation —
//! each consumer only sees the methods it needs.

use std::path::{Path, PathBuf};

use thiserror::Error;

use super::message::GitMessage;

/// Errors from git sync operations.
#[derive(Debug, Error)]
pub enum GitSyncError {
    /// The given path is not inside a git repository.
    #[error("not a git repository: {path}")]
    NotARepo { path: PathBuf },

    /// No remote is configured for the current branch.
    #[error("no remote configured for branch '{branch}'")]
    NoRemote { branch: String },

    /// Working tree has uncommitted changes.
    #[error("working tree is dirty — commit or stash changes first")]
    DirtyWorkingTree,

    /// Fast-forward is not possible (histories have diverged).
    #[error("remote has diverged — resolve manually with git")]
    Diverged,

    /// A git operation failed.
    ///
    /// Both fields are [`GitMessage`] rather than `String`, and that is what
    /// closes this variant rather than a convention asking people to be careful.
    /// `message` is the one that matters: it carries git's stderr, which a
    /// non-interactive git can fill with a remote URL carrying a credential.
    /// `operation` is a literal at every site but one — `run_git` builds it from
    /// the argument list, so a future `run_git(root, &["push", &url])` would put
    /// a URL there too. Typing both means one rule instead of two, and the
    /// compiler holds it either way.
    #[error("{operation}: {message}")]
    OperationFailed {
        operation: GitMessage,
        message: GitMessage,
    },
}

/// Information about a discovered git repository.
#[derive(Debug, Clone)]
pub struct RepoInfo {
    /// Absolute path to the repository root (workdir).
    pub root: PathBuf,
    /// Current branch name (e.g., `"main"`), if on a branch.
    pub branch: Option<String>,
    /// Name of the remote tracking the current branch (e.g., `"origin"`).
    pub remote_name: Option<String>,
}

/// Working tree status summary.
#[derive(Debug, Clone, Default)]
pub struct RepoStatus {
    /// Files with unstaged modifications.
    pub modified: Vec<PathBuf>,
    /// Files staged in the index.
    pub staged: Vec<PathBuf>,
    /// Untracked files.
    pub untracked: Vec<PathBuf>,
    /// Deleted files.
    pub deleted: Vec<PathBuf>,
    /// Commits ahead of the remote tracking branch.
    pub ahead: usize,
    /// Commits behind the remote tracking branch.
    pub behind: usize,
}

impl RepoStatus {
    /// Returns `true` if the working tree has any changes (including untracked files).
    #[must_use]
    pub fn is_dirty(&self) -> bool {
        !self.modified.is_empty()
            || !self.staged.is_empty()
            || !self.untracked.is_empty()
            || !self.deleted.is_empty()
    }

    /// Returns `true` if there are tracked file changes that could conflict with a merge.
    ///
    /// Unlike [`is_dirty`](Self::is_dirty), this excludes untracked files since they
    /// cannot conflict with a fast-forward merge. Use this for pull guards.
    #[must_use]
    pub fn has_uncommitted_changes(&self) -> bool {
        !self.modified.is_empty() || !self.staged.is_empty() || !self.deleted.is_empty()
    }

    /// Returns `true` if the working tree is completely clean.
    #[must_use]
    pub fn is_clean(&self) -> bool {
        !self.is_dirty()
    }
}

/// Opaque commit identifier.
///
/// Wraps a hex string to avoid leaking `gix` types through the port boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitId(pub String);

impl std::fmt::Display for CommitId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Show abbreviated hash (first 7 chars), like git's short SHA
        write!(f, "{}", &self.0[..self.0.len().min(7)])
    }
}

/// Result of a fast-forward pull attempt.
#[derive(Debug, Clone)]
pub enum FastForwardResult {
    /// Successfully fast-forwarded.
    Advanced {
        from: CommitId,
        to: CommitId,
        commit_count: usize,
    },
    /// Already up to date — no new commits.
    AlreadyUpToDate,
    /// Cannot fast-forward (diverged histories).
    Diverged,
}

/// A file that changed between two commits.
#[derive(Debug, Clone)]
pub struct ChangedFile {
    /// Path relative to the repository root.
    pub path: PathBuf,
    /// Type of change.
    pub change_type: ChangeType,
}

/// Type of change for a file between commits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeType {
    Added,
    Modified,
    Deleted,
}

/// Port for git write and sync operations.
///
/// Provides the operations needed by [`crate::sync_service::SyncService`] to
/// stage, commit, push, and pull changes in the packages repository.
///
/// This trait is intentionally synchronous — git operations are brief filesystem
/// I/O that can be wrapped in `spawn_blocking` at the call site if needed.
#[cfg_attr(any(test, feature = "with_mocks"), mockall::automock)]
pub trait GitSyncProvider: Send + Sync {
    /// Discover the git repository containing the given path.
    ///
    /// Walks up from `path` until a `.git` directory is found.
    fn discover_repo(&self, path: &Path) -> Result<RepoInfo, GitSyncError>;

    /// Get the working tree status for the repository.
    fn repo_status(&self, repo_root: &Path) -> Result<RepoStatus, GitSyncError>;

    /// Stage specific files in the index.
    fn stage_files(&self, repo_root: &Path, files: &[PathBuf]) -> Result<(), GitSyncError>;

    /// Create a commit with the given message.
    ///
    /// Only staged files are committed. Returns the new commit's ID.
    fn commit(&self, repo_root: &Path, message: &str) -> Result<CommitId, GitSyncError>;

    /// Push commits to the remote tracking branch.
    fn push(&self, repo_root: &Path) -> Result<(), GitSyncError>;

    /// Fetch from the remote.
    fn fetch(&self, repo_root: &Path) -> Result<(), GitSyncError>;

    /// Attempt a fast-forward merge of the remote tracking branch.
    ///
    /// Returns the result indicating success, already-up-to-date, or diverged.
    fn fast_forward(&self, repo_root: &Path) -> Result<FastForwardResult, GitSyncError>;

    /// Diff two commits and return the list of changed files.
    fn diff_commits(
        &self,
        repo_root: &Path,
        from: &CommitId,
        to: &CommitId,
    ) -> Result<Vec<ChangedFile>, GitSyncError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commit_id_display_abbreviates() {
        let id = CommitId("abc1234def5678".to_string());
        assert_eq!(format!("{id}"), "abc1234");
    }

    #[test]
    fn commit_id_display_short_hash() {
        let id = CommitId("abc".to_string());
        assert_eq!(format!("{id}"), "abc");
    }

    #[test]
    fn repo_status_clean_when_empty() {
        let status = RepoStatus::default();
        assert!(status.is_clean());
        assert!(!status.is_dirty());
    }

    #[test]
    fn repo_status_dirty_with_modified() {
        let status = RepoStatus {
            modified: vec![PathBuf::from("foo.yml")],
            ..Default::default()
        };
        assert!(status.is_dirty());
        assert!(!status.is_clean());
    }

    #[test]
    fn repo_status_dirty_with_untracked() {
        let status = RepoStatus {
            untracked: vec![PathBuf::from("new.yml")],
            ..Default::default()
        };
        assert!(status.is_dirty());
    }

    #[test]
    fn has_uncommitted_changes_excludes_untracked() {
        let status = RepoStatus {
            untracked: vec![PathBuf::from("new.yml")],
            ..Default::default()
        };
        // Untracked files are dirty but NOT uncommitted changes
        assert!(status.is_dirty());
        assert!(!status.has_uncommitted_changes());
    }

    #[test]
    fn has_uncommitted_changes_includes_modified() {
        let status = RepoStatus {
            modified: vec![PathBuf::from("foo.yml")],
            ..Default::default()
        };
        assert!(status.has_uncommitted_changes());
    }
}
