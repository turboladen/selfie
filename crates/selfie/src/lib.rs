//! Selfie - a personal meta-package manager.
//!
//! Manages packages across package managers and environments. Ports and adapters
//! throughout: the core logic never touches a file system, a process, or a
//! terminal directly, so a CLI and an MCP server can drive the same code and
//! present it differently.
//!
//! - [`package`] — package definitions, services, and domain logic
//! - [`dotfile_service`] — deploying dotfiles to their targets
//! - [`sync_service`] — syncing specs and dotfiles over git
//! - [`config`] — configuration
//! - [`commands`] — command execution
//! - [`fs`] — file system access
//! - [`privilege`] — whether the process holds privilege it should not write with
//! - [`validation`] — validation types
//! - [`yaml`] — reading YAML files

// Every non-unix code path was unbuilt by CI and silently disabled a security
// check, so the workspace refuses to build on a non-unix target. The guard is
// wider than the supported set: macOS and Linux are the targets CI builds and
// tests, and another unix target compiles here without being one of them.
#[cfg(not(unix))]
compile_error!("selfie builds only on unix targets; macOS and Linux are the supported ones");

pub mod commands;
pub mod config;
pub mod dotfile_service;
pub mod fs;
pub mod git;
pub mod namespace;
pub mod package;
pub(crate) mod paths;
pub mod privilege;
pub mod sync_service;
pub mod validation;
pub mod yaml;

/// Returns `singular` when `count == 1`, otherwise `plural`.
pub fn pluralize<'a>(count: usize, singular: &'a str, plural: &'a str) -> &'a str {
    if count == 1 { singular } else { plural }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pluralize_zero_returns_plural() {
        assert_eq!(pluralize(0, "file", "files"), "files");
    }

    #[test]
    fn pluralize_one_returns_singular() {
        assert_eq!(pluralize(1, "commit", "commits"), "commit");
    }

    #[test]
    fn pluralize_many_returns_plural() {
        assert_eq!(pluralize(5, "package", "packages"), "packages");
    }
}
