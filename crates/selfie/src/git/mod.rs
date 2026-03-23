//! Git integration types and adapters.
//!
//! This module provides the git status port (trait) and its `gix`-backed adapter,
//! shared by both the package service and sync service.

pub mod adapter;
pub mod status_provider;

// Re-export all public types at the module level for convenience.
pub use self::adapter::GixGitAdapter;
pub use self::status_provider::{
    GitDirectoryStatus, GitFileStatus, GitStatusError, GitStatusProvider,
};

#[cfg(any(test, feature = "with_mocks"))]
pub use self::status_provider::MockGitStatusProvider;
