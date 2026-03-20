//! Git status types and trait for checking file status in git repositories.
//!
//! This module defines the port (trait) for git status lookups, following
//! hexagonal architecture. The library emits git status data through events;
//! callers (CLI, MCP) decide how to render it.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};

use thiserror::Error;

/// Git status of a single file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GitFileStatus {
    /// Tracked, no changes
    Clean,
    /// Unstaged changes in the worktree
    Modified,
    /// Changes staged in the index
    Staged,
    /// Both staged and unstaged changes
    StagedAndModified,
    /// Not tracked by git
    Untracked,
    /// The package directory is not inside a git repository
    NotInRepo,
}

/// Aggregated git status for all files in a directory.
#[derive(Debug, Clone)]
pub struct GitDirectoryStatus {
    /// Whether the directory is inside a git repository
    pub in_repo: bool,
    /// Per-file status (only files with non-clean status are present)
    pub files: HashMap<PathBuf, GitFileStatus>,
}

impl GitDirectoryStatus {
    /// Look up the git status for a specific file path.
    ///
    /// Returns `Clean` if the directory is in a repo but the file has no changes
    /// (tracked with no modifications). Returns `NotInRepo` if the directory
    /// is not inside a git repository.
    pub fn status_for_file(&self, path: &Path) -> GitFileStatus {
        if !self.in_repo {
            return GitFileStatus::NotInRepo;
        }
        // Canonicalize the lookup path to match the canonicalized keys in the HashMap.
        // This handles symlink mismatches (e.g., /tmp → /private/tmp on macOS).
        let canonical = dunce::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        self.files
            .get(&canonical)
            .cloned()
            .unwrap_or(GitFileStatus::Clean)
    }
}

/// Errors that can occur when checking git status.
#[derive(Debug, Error)]
pub enum GitStatusError {
    #[error("failed to query git status: {0}")]
    StatusError(String),
}

/// Port for checking git status of files (Hexagonal Architecture).
///
/// Implementations provide git status information for files in a directory,
/// allowing the service layer to annotate spec/package data with git state.
///
/// Note: this trait is intentionally synchronous. The `gix` adapter does filesystem
/// I/O which blocks the async executor thread briefly. For typical selfie package
/// directories (small number of files), this is negligible. If profiling shows this
/// is a bottleneck, wrap calls in `tokio::task::spawn_blocking` at the call site.
#[cfg_attr(any(test, feature = "with_mocks"), mockall::automock)]
pub trait GitStatusProvider: Send + Sync {
    /// Get the git status of all files in the given directory.
    fn status_for_directory(&self, directory: &Path) -> Result<GitDirectoryStatus, GitStatusError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_for_file_returns_clean_when_tracked_without_changes() {
        let status = GitDirectoryStatus {
            in_repo: true,
            files: HashMap::new(),
        };
        assert_eq!(
            status.status_for_file(Path::new("some-file.yml")),
            GitFileStatus::Clean
        );
    }

    #[test]
    fn status_for_file_returns_not_in_repo_when_not_in_repo() {
        let status = GitDirectoryStatus {
            in_repo: false,
            files: HashMap::new(),
        };
        assert_eq!(
            status.status_for_file(Path::new("some-file.yml")),
            GitFileStatus::NotInRepo
        );
    }

    #[test]
    fn status_for_file_returns_stored_status() {
        let mut files = HashMap::new();
        files.insert(PathBuf::from("modified.yml"), GitFileStatus::Modified);
        files.insert(PathBuf::from("staged.yml"), GitFileStatus::Staged);

        let status = GitDirectoryStatus {
            in_repo: true,
            files,
        };

        assert_eq!(
            status.status_for_file(Path::new("modified.yml")),
            GitFileStatus::Modified
        );
        assert_eq!(
            status.status_for_file(Path::new("staged.yml")),
            GitFileStatus::Staged
        );
        assert_eq!(
            status.status_for_file(Path::new("clean.yml")),
            GitFileStatus::Clean
        );
    }
}
