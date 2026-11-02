//! Git write and network operations: stage, commit, push, fetch, fast-forward,
//! diff.
//!
//! Separate from [`super::GitStatusProvider`], which is read-only status for
//! annotating output. One adapter ([`super::GixGitAdapter`]) implements both, so
//! the split exists to let each consumer see only what it needs.

use std::path::{Path, PathBuf};

use super::message::GitMessage;
use crate::parts::{Composed, Parts};

/// Errors from git sync operations.
#[derive(Debug, Clone)]
pub enum GitSyncError {
    /// The given path is not inside a git repository.
    NotARepo { path: PathBuf },

    /// No remote is configured for the current branch.
    NoRemote { branch: String },

    /// Working tree has uncommitted changes.
    DirtyWorkingTree,

    /// Fast-forward is not possible (histories have diverged).
    Diverged,

    /// The remote has commits the local branch lacks, so the push was refused.
    PushRejected,

    /// Committing is disabled, so no commit was created.
    CommitDisabled,

    /// A git operation failed. `operation` names it in selfie's own words, and
    /// `message` is git's text, redacted and bounded by its [`GitMessage`] type.
    // `message` carries git's stderr, which a non-interactive git can fill with
    // a remote URL carrying a credential, so only a `GitMessage` can hold it,
    // and it enters the sentence only through `Parts::git`. `operation` is a
    // `&'static str`, so no argument list, which can hold a URL or a commit ID,
    // can become the label.
    OperationFailed {
        operation: &'static str,
        message: GitMessage,
    },
}

crate::parts::display_from_parts!(error GitSyncError);

impl Composed for GitSyncError {
    // The repository's path is a path and the branch a name. Git's own text is
    // foreign, since it can quote any path git touched.
    fn parts(&self) -> Parts<'_> {
        match self {
            Self::NotARepo { path } => not_a_repository(path),
            Self::NoRemote { branch } => Parts::new()
                .text("no remote configured for branch '")
                .name(branch)
                .text("'"),
            Self::DirtyWorkingTree => {
                Parts::new().text("working tree is dirty — commit or stash changes first")
            }
            Self::Diverged => Parts::new().text("remote has diverged — resolve manually with git"),
            Self::PushRejected => {
                Parts::new().text("the remote has new commits. Run 'selfie sync pull' first")
            }
            Self::CommitDisabled => commit_disabled(),
            Self::OperationFailed { operation, message } => {
                Parts::new().text(*operation).text(": ").git(message)
            }
        }
    }
}

/// The sentence saying sync push will not commit, since committing is
/// disabled. Both the error and the operation's refusal word it with this.
pub(crate) fn commit_disabled() -> Parts<'static> {
    Parts::new().text(
        "sync push is disabled: its commit can record every tracked file as deleted. \
         Commit and push the repository with git until this is fixed",
    )
}

/// The sentence saying no git repository holds `path`, with the path a part
/// of its own.
pub(crate) fn not_a_repository(path: &Path) -> Parts<'_> {
    Parts::new().text("not a git repository: ").path(path)
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

    // Every git sync error, whole.
    #[test]
    fn each_git_sync_error_is_worded_whole() {
        let cases = [
            (
                GitSyncError::NotARepo {
                    path: PathBuf::from("/r\u{200B}p"),
                },
                "not a git repository: /r\u{200B}p",
            ),
            (
                GitSyncError::NoRemote {
                    branch: "m\u{200B}n".to_string(),
                },
                "no remote configured for branch 'm\u{200B}n'",
            ),
            (
                GitSyncError::DirtyWorkingTree,
                "working tree is dirty — commit or stash changes first",
            ),
            (
                GitSyncError::Diverged,
                "remote has diverged — resolve manually with git",
            ),
            (
                GitSyncError::PushRejected,
                "the remote has new commits. Run 'selfie sync pull' first",
            ),
            (
                GitSyncError::CommitDisabled,
                "sync push is disabled: its commit can record every tracked file as deleted. \
                 Commit and push the repository with git until this is fixed",
            ),
            (
                GitSyncError::OperationFailed {
                    operation: "git fetch",
                    message: GitMessage::new("cannot read '/q\u{200B}r'"),
                },
                "git fetch: cannot read '/q\u{200B}r'",
            ),
        ];
        for (error, sentence) in cases {
            assert_eq!(error.to_string(), sentence);
        }
        let status = super::super::GitStatusError::StatusError(GitMessage::new("cannot read '/q'"));
        assert_eq!(
            status.to_string(),
            "failed to query git status: cannot read '/q'"
        );
    }

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
