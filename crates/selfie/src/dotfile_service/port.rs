//! DotfileService port (trait) for dotfile deployment operations
//!
//! This module defines the hexagonal architecture port for dotfile deployment.
//! The [`DotfileService`] trait abstracts dotfile deployment, allowing
//! different implementations and enabling comprehensive testing through mocking.

use crate::package::event::EventStream;
use std::future::Future;

/// Options for dotfile apply operations
#[derive(Debug, Clone, Default)]
pub struct ApplyOptions {
    /// Show what would change without writing
    pub dry_run: bool,
    /// Auto-accept overwrite for conflicts (--yes flag)
    pub auto_accept: bool,
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
