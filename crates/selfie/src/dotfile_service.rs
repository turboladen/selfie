//! Dotfile deployment subsystem.
//!
//! This module tree implements `selfie apply` — deploying dotfiles from a
//! source repository to their target locations on the user's machine. It follows
//! the same hexagonal architecture as the package subsystem: a port trait
//! ([`port::DotfileService`]) defines operations, and a concrete adapter
//! ([`service::DotfileServiceImpl`]) wires together the package repository, file
//! system, and deploy state.
//!
//! ## Module layout
//!
//! - [`port`] — The `DotfileService` trait and `ApplyOptions` request type
//! - [`service`] — Concrete implementation: apply, conflict resolution, drift checking
//! - [`deploy`] — Pure decision logic: checksums, path resolution, deploy-vs-skip-vs-conflict
//! - [`state`] — `DeployState` persistence: per-machine checksum tracking and drift detection
//! - [`diff`] — Unified diff generation for conflict display
//! - [`semantic`] — Heuristic analysis of shell config files for duplicate-detection warnings
//! - [`template`] — Named-value substitution for templated dotfiles

pub mod deploy;
pub mod diff;
pub mod port;
pub mod semantic;
pub mod service;
pub mod state;
pub(crate) mod template;
