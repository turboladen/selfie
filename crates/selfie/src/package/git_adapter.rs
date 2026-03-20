//! gix-based implementation of [`GitStatusProvider`].
//!
//! Uses the `gix` crate for native git status lookups without shelling out.

use std::{
    collections::{HashMap, HashSet},
    path::Path,
};

use super::git::{GitDirectoryStatus, GitFileStatus, GitStatusError, GitStatusProvider};

/// Git status provider backed by the `gix` crate.
#[derive(Debug, Clone)]
pub struct GixGitStatusProvider;

impl GitStatusProvider for GixGitStatusProvider {
    fn status_for_directory(&self, directory: &Path) -> Result<GitDirectoryStatus, GitStatusError> {
        let repo = match gix::discover(directory) {
            Ok(repo) => repo,
            Err(_) => {
                return Ok(GitDirectoryStatus {
                    in_repo: false,
                    files: HashMap::new(),
                });
            }
        };

        let workdir = repo
            .workdir()
            .ok_or_else(|| GitStatusError::StatusError("bare repository".to_string()))?
            .to_path_buf();

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
                        Item::Modification { rela_path, .. } => {
                            // Any modification between index and worktree = unstaged change
                            worktree_changes.insert(rela_path.to_string());
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
                    // HEAD→index change = staged
                    let location = change.fields().0.to_string();
                    index_changes.insert(location);
                }
            }
        }

        // Combine worktree and index changes into final status
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn non_git_directory_returns_not_in_repo() {
        let temp = tempfile::TempDir::new().unwrap();
        let provider = GixGitStatusProvider;
        let result = provider.status_for_directory(temp.path()).unwrap();
        assert!(!result.in_repo);
        assert!(result.files.is_empty());
    }

    #[test]
    fn git_repo_detected_as_in_repo() {
        let temp = tempfile::TempDir::new().unwrap();
        gix::init(temp.path()).unwrap();

        let provider = GixGitStatusProvider;
        let result = provider.status_for_directory(temp.path()).unwrap();
        assert!(result.in_repo);
    }

    #[test]
    fn untracked_file_detected() {
        let temp = tempfile::TempDir::new().unwrap();
        gix::init(temp.path()).unwrap();

        let file_path = temp.path().join("untracked.yml");
        fs::write(&file_path, "name: untracked\n").unwrap();

        let provider = GixGitStatusProvider;
        let result = provider.status_for_directory(temp.path()).unwrap();

        assert!(result.in_repo);
        assert_eq!(result.status_for_file(&file_path), GitFileStatus::Untracked);
    }
}
