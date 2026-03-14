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
//! - [`validation`] - Validation types and utilities
//!
//! # Examples
//!
//! ```no_run
//! use selfie::package::PackageService;
//! use selfie::config::AppConfig;
//!
//! // Example usage would go here once the API is more stable
//! ```

pub mod commands;
pub mod config;
pub mod fs;
pub mod package;
pub mod validation;
