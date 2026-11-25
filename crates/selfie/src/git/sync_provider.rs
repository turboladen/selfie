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
///
/// [`kind`](Self::kind) is its label, the variant's name in snake_case.
#[derive(Debug, Clone, strum::IntoStaticStr, strum::VariantNames)]
#[strum(serialize_all = "snake_case")]
pub enum GitSyncError {
    /// The given path is not inside a git repository.
    NotARepo { path: PathBuf },

    /// No remote is configured for the current branch.
    NoRemote { branch: String },

    /// The index holds staged changes, which a pull would disturb.
    StagedChanges,

    /// Fast-forward is not possible (histories have diverged).
    Diverged,

    /// The remote has commits the local branch lacks, so the push was refused.
    PushRejected,

    /// git has an operation in progress, so nothing was staged or committed.
    OperationInProgress { operation: InProgressOperation },

    /// A path to stage or commit has unresolved conflicts, so the index holds
    /// no single entry to record for it. `path` is relative to the repository
    /// root.
    Conflicted { path: PathBuf },

    /// The commit would record the same tree as its parent, so none was made.
    NothingToCommit,

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

impl GitSyncError {
    /// The label for this error, such as `"push_rejected"`.
    #[must_use]
    pub fn kind(&self) -> &'static str {
        self.into()
    }

    /// Whether this error is a refusal a step made after it ran, such as a pull
    /// that found staged changes, rather than the step itself failing.
    #[must_use]
    pub fn is_refusal(&self) -> bool {
        // Exhaustive, so a new variant has to choose a side.
        match self {
            Self::StagedChanges | Self::Diverged => true,
            Self::NotARepo { .. }
            | Self::NoRemote { .. }
            | Self::PushRejected
            | Self::OperationInProgress { .. }
            | Self::Conflicted { .. }
            | Self::NothingToCommit
            | Self::OperationFailed { .. } => false,
        }
    }
}

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
            Self::StagedChanges => Parts::new()
                .text("Staged changes detected. Commit or unstage them before pulling."),
            Self::Diverged => Parts::new().text("Remote has diverged. Resolve manually with git."),
            Self::PushRejected => {
                Parts::new().text("the remote has commits this machine lacks. Merge or rebase onto them with git, then push again")
            }
            Self::OperationInProgress { operation } => Parts::new()
                .text(operation.described())
                .text(" is in progress. Finish or abort it with git first"),
            Self::Conflicted { path } => Parts::new().path(path).text(" has unresolved conflicts"),
            Self::NothingToCommit => {
                Parts::new().text("nothing to commit: the staged files match the last commit")
            }
            Self::OperationFailed { operation, message } => {
                Parts::new().text(*operation).text(": ").git(message)
            }
        }
    }
}

/// An operation git can leave in progress, which owns the index until it is
/// finished or aborted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InProgressOperation {
    Merge,
    Rebase,
    CherryPick,
    Revert,
    Bisect,
    /// `git am`, applying patches from a mailbox.
    ApplyMailbox,
}

impl InProgressOperation {
    /// The operation as the subject of a sentence, such as "a merge".
    #[must_use]
    pub const fn described(self) -> &'static str {
        match self {
            Self::Merge => "a merge",
            Self::Rebase => "a rebase",
            Self::CherryPick => "a cherry-pick",
            Self::Revert => "a revert",
            Self::Bisect => "a bisect",
            Self::ApplyMailbox => "a git am",
        }
    }
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

    /// Stage `files`, relative to the repository root, as `git add` stages
    /// them: the repository's attributes and filters apply, a symlink is
    /// staged as a link, and a path gone from the working tree is staged as
    /// removed. A path already gone from the index too is left as it is. No
    /// other index entry changes.
    ///
    /// # Errors
    ///
    /// [`GitSyncError::OperationInProgress`] and [`GitSyncError::Conflicted`]
    /// as [`commit`](Self::commit) gives them, before anything is written, and
    /// [`GitSyncError::OperationFailed`] when git refuses a path, such as one
    /// beyond a symlink, in which case the index is left as it was.
    fn stage_files(&self, repo_root: &Path, files: &[PathBuf]) -> Result<(), GitSyncError>;

    /// Commit `files`, relative to the repository root, as the index holds them.
    ///
    /// The new commit's tree is its parent's with only `files` changed: each one
    /// recorded as staged, or removed when the index has no entry for it. Every
    /// other staged change stays staged and uncommitted. The commit is signed
    /// when the repository's git configuration says to, and no hook runs.
    /// Returns the new commit's ID.
    ///
    /// # Errors
    ///
    /// [`GitSyncError::OperationInProgress`] while git has a merge, rebase or
    /// similar operation unfinished, [`GitSyncError::Conflicted`] for a path with
    /// unresolved conflicts, and [`GitSyncError::NothingToCommit`] when the tree
    /// would not change. A path with no index entry that is a directory in the
    /// parent's tree gives [`GitSyncError::OperationFailed`]: it is refused
    /// rather than removed with everything below it. None of these writes
    /// anything, and neither does a signing failure: the branch moves only after
    /// the commit is complete, and only if it still points at the parent.
    fn commit(
        &self,
        repo_root: &Path,
        files: &[PathBuf],
        message: &str,
    ) -> Result<CommitId, GitSyncError>;

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

    // The label sets are the MCP contract for a failed sync step, so each label
    // is pinned. A rename here is a change an MCP client sees.
    #[test]
    fn every_git_sync_error_and_step_has_its_label() {
        use strum::VariantNames;

        assert_eq!(
            GitSyncError::VARIANTS,
            [
                "not_a_repo",
                "no_remote",
                "staged_changes",
                "diverged",
                "push_rejected",
                "operation_in_progress",
                "conflicted",
                "nothing_to_commit",
                "operation_failed",
            ]
        );
        assert_eq!(GitSyncError::PushRejected.kind(), "push_rejected");
        assert_eq!(
            crate::sync_service::port::GitStep::VARIANTS,
            [
                "discover",
                "status",
                "check_status",
                "stage",
                "commit",
                "push",
                "fetch",
                "fast_forward",
            ]
        );
        assert_eq!(crate::sync_service::port::GitStep::Push.label(), "push");
    }

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
                GitSyncError::StagedChanges,
                "Staged changes detected. Commit or unstage them before pulling.",
            ),
            (
                GitSyncError::Diverged,
                "Remote has diverged. Resolve manually with git.",
            ),
            (
                GitSyncError::PushRejected,
                "the remote has commits this machine lacks. Merge or rebase onto them with git, then push again",
            ),
            (
                GitSyncError::OperationInProgress {
                    operation: InProgressOperation::CherryPick,
                },
                "a cherry-pick is in progress. Finish or abort it with git first",
            ),
            (
                GitSyncError::Conflicted {
                    path: PathBuf::from("c/f\u{200B}o"),
                },
                "c/f\u{200B}o has unresolved conflicts",
            ),
            (
                GitSyncError::NothingToCommit,
                "nothing to commit: the staged files match the last commit",
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
