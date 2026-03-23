//! DotfileService port (trait) for dotfile deployment operations
//!
//! This module defines the hexagonal architecture port for dotfile deployment.
//! The [`DotfileService`] trait abstracts dotfile deployment, allowing
//! different implementations and enabling comprehensive testing through mocking.

use std::future::Future;
use std::sync::Arc;

use crate::package::event::EventStream;

/// The user's chosen resolution for a dotfile conflict.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConflictResolution {
    /// Overwrite the target with the source (repo wins).
    Accept,
    /// Keep the target as-is and skip this dotfile.
    Skip,
}

/// Port for resolving dotfile conflicts interactively.
///
/// The library calls [`resolve`](ConflictResolver::resolve) when a conflict is
/// detected. The CLI adapter can prompt the user with `dialoguer`; tests can
/// return a fixed answer; the MCP server can return `Skip`.
pub trait ConflictResolver: Send + Sync {
    /// Decide what to do about a conflict between `source` and `target`.
    ///
    /// `diff` contains the unified diff string (may be empty if files are
    /// binary or unreadable).
    fn resolve(&self, source: &str, target: &str, diff: &str) -> ConflictResolution;
}

/// Options for dotfile apply operations
#[derive(Clone, Default)]
pub struct ApplyOptions {
    /// Show what would change without writing
    pub dry_run: bool,
    /// Auto-accept overwrite for conflicts (--yes flag)
    pub auto_accept: bool,
    /// Interactive conflict resolver. When set, conflicts call this instead of
    /// emitting a `DotfileConflict` event and skipping. Ignored when
    /// `auto_accept` is true.
    pub conflict_resolver: Option<Arc<dyn ConflictResolver>>,
}

impl std::fmt::Debug for ApplyOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ApplyOptions")
            .field("dry_run", &self.dry_run)
            .field("auto_accept", &self.auto_accept)
            .field(
                "conflict_resolver",
                &self.conflict_resolver.as_ref().map(|_| "<resolver>"),
            )
            .finish()
    }
}

/// Port for dotfile deployment operations (Hexagonal Architecture)
///
/// This trait abstracts dotfile deployment operations to allow different
/// implementations and enable comprehensive testing through mocking.
/// It provides three operations:
///
/// - `apply_all` — Deploy all dotfiles from all packages
/// - `apply` — Deploy dotfiles for a specific package
/// - `check_drift` — Check for drift between deployed files and repo sources
#[cfg_attr(any(test, feature = "with_mocks"), mockall::automock)]
pub trait DotfileService: Send + Sync {
    /// Deploy all dotfiles from all packages
    fn apply_all(&self, options: ApplyOptions) -> impl Future<Output = EventStream> + Send;

    /// Deploy dotfiles for a specific package
    fn apply(&self, name: &str, options: ApplyOptions) -> impl Future<Output = EventStream> + Send;

    /// Check for drift between deployed files and repo sources
    fn check_drift(&self) -> impl Future<Output = EventStream> + Send;
}
