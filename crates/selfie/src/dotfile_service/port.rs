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

/// What a conflict resolver is given to decide with.
///
/// Ordinary files carry a rendered diff. Secret-bearing files carry the two
/// values plus a summary that describes their shape without revealing either.
///
/// The values reach the resolver and nothing else. The resolver is an optional
/// dependency the calling adapter injects, so an adapter that supplies none —
/// the MCP server, for instance — cannot receive secret content at all. That is
/// what makes an opt-in reveal safe to offer without it becoming a leak into
/// structured output.
pub enum ConflictDetail<'a> {
    /// A rendered unified diff, and the repository path it came from.
    ///
    /// May be empty if the files are binary or unreadable.
    Diff { source: &'a str, diff: &'a str },
    /// The two candidate contents, plus a non-revealing structural summary.
    ///
    /// Borrowed rather than owned. This does not stop a resolver copying the
    /// values — the CLI's reveal path necessarily does, to show them to a human.
    /// What it buys is that the values are never moved wholesale across the
    /// `'static` boundary the blocking resolver call requires, so the only copy
    /// that exists is one an adapter took deliberately.
    Secret {
        summary: &'a str,
        incoming: &'a [u8],
        current: &'a [u8],
    },
}

/// Port for resolving dotfile conflicts interactively.
///
/// The library calls [`resolve`](ConflictResolver::resolve) when a conflict is
/// detected. The CLI adapter prompts the user with `dialoguer`; tests can return
/// a fixed answer; the MCP server supplies no resolver at all.
pub trait ConflictResolver: Send + Sync {
    /// Decide what to do about the conflict at `target`.
    fn resolve(&self, target: &str, detail: ConflictDetail<'_>) -> ConflictResolution;
}

/// Options for dotfile apply operations
#[derive(Clone, Default)]
pub struct ApplyOptions {
    /// Show what would change without writing
    pub dry_run: bool,
    /// Auto-accept overwrite for conflicts (--yes flag)
    pub auto_accept: bool,
    /// Interactive conflict resolver. When set, conflicts call this instead of
    /// emitting a `DotfileConflict` event and skipping.
    ///
    /// For an ordinary repository-file conflict this is ignored when
    /// `auto_accept` is true. For a secret-bearing entry it is the **only** way
    /// to accept an overwrite: `auto_accept` is not consulted there at all, so a
    /// caller that supplies no resolver always gets the conflict reported and
    /// skipped.
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
///
/// Operations:
/// - `apply_all` / `apply` — Deploy dotfiles to target locations
/// - `check_drift` — Detect changes since last deploy
/// - `list` — Every dotfile entry declared, without running anything
/// - `track_standalone` — Start tracking a file as a standalone dotfile
/// - `track_for_package` — Add a file to an existing package's dotfiles
#[cfg_attr(any(test, feature = "with_mocks"), mockall::automock)]
pub trait DotfileService: Send + Sync {
    /// Deploy all dotfiles from all packages
    fn apply_all(&self, options: ApplyOptions) -> impl Future<Output = EventStream> + Send;

    /// Deploy dotfiles for a specific package
    fn apply(&self, name: &str, options: ApplyOptions) -> impl Future<Output = EventStream> + Send;

    /// Check for drift between deployed files and repo sources
    fn check_drift(&self) -> impl Future<Output = EventStream> + Send;

    /// Every dotfile entry declared across both directories.
    ///
    /// Reads files and runs nothing, so it cannot leak a provider's secret or
    /// raise an authentication prompt. A spec that did not parse is reported as
    /// [`SpecSkipped`](crate::package::event::PackageEvent::SpecSkipped) rather
    /// than dropped, because a listing missing a package silently is one the
    /// caller reads as the whole picture.
    fn list(&self) -> impl Future<Output = EventStream> + Send;

    /// Track a file as a standalone dotfile.
    ///
    /// Copies the file at `target_path` into the dotfiles directory under a new
    /// spec named `name`, writes the spec, and records initial deploy state.
    ///
    /// The writes are ordered so a failure leaves nothing to clean up: a refused
    /// copy writes no spec, and a spec that cannot be saved has the copy removed
    /// again. An unwritable deploy state is the one exception and the only partial
    /// outcome — the copy and the entry are correct, the failure names both, and an
    /// apply records the deployment. A second call is refused: the spec is there.
    fn track_standalone(
        &self,
        name: &str,
        target_path: &str,
    ) -> impl Future<Output = EventStream> + Send;

    /// Add a file to an existing package's dotfiles.
    ///
    /// Copies the file at `target_path` into the package's directory beside the
    /// YAML, adds a `dotfiles` entry, saves it, and records initial deploy state.
    ///
    /// Orders its writes and reports an unrecorded deployment as
    /// [`track_standalone`](Self::track_standalone) does. A second call reports the
    /// target as already tracked and records nothing, so only an apply finishes an
    /// unrecorded track — unless the target rule refuses it, which an existing entry
    /// with a relative or `~user/…` target meets before that answer.
    fn track_for_package(
        &self,
        package_name: &str,
        target_path: &str,
    ) -> impl Future<Output = EventStream> + Send;
}
