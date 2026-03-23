//! Git integration types and adapters.
//!
//! This module provides the git ports (traits) and `gix`-backed adapter,
//! shared by both the package service and sync service.
//!
//! Two traits follow interface segregation:
//! - [`GitStatusProvider`] — read-only status for annotating UI output
//! - [`GitSyncProvider`] — write/sync operations for the sync service

pub mod adapter;
pub mod status_provider;
pub mod sync_provider;

// Re-export all public types at the module level for convenience.
pub use self::adapter::GixGitAdapter;
pub use self::status_provider::{
    GitDirectoryStatus, GitFileStatus, GitStatusError, GitStatusProvider,
};
pub use self::sync_provider::{
    ChangeType, ChangedFile, CommitId, FastForwardResult, GitSyncError, GitSyncProvider, RepoInfo,
    RepoStatus,
};

#[cfg(any(test, feature = "with_mocks"))]
pub use self::status_provider::MockGitStatusProvider;

#[cfg(any(test, feature = "with_mocks"))]
pub use self::sync_provider::MockGitSyncProvider;
