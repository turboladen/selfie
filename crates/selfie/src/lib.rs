//! Selfie - A personal meta-package manager
//!
//! The `selfie` library provides core functionality for managing packages across multiple
//! package managers and environments. It implements a hexagonal architecture with ports
//! and adapters to allow flexible integration with different UIs and systems.
//!
//! # Architecture
//!
//! This library follows the Hexagonal Architecture pattern (also known as Ports and Adapters).
//! The core business logic is isolated from external concerns like file systems, command
//! execution, and user interfaces through well-defined interfaces (ports).
//!
//! # Main Components
//!
//! - [`package`] - Core package definitions, services, and domain logic
//! - [`config`] - Application configuration management
//! - [`commands`] - Command execution abstractions
//! - [`fs`] - File system abstractions
//! - [`privilege`] - Whether the process holds privilege it should not write with
//! - [`validation`] - Validation types and utilities
//!
//! # Examples
//!
//! ```no_run
//! use selfie::package::PackageService;
//! use selfie::config::SelfieConfig;
//!
//! // Example usage would go here once the API is more stable
//! ```

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
