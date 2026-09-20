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
//! - `backup` — Copies of what a target held before an apply overwrote it
//! - [`deploy`] — Pure decision logic: checksums, path resolution, deploy-vs-skip-vs-conflict
//! - [`state`] — `DeployState` persistence: per-machine checksum tracking and drift detection
//! - [`diff`] — Unified diff generation for conflict display
//! - `directory` — What selfie says about the standalone dotfiles directory
//! - `resolve` — Apply-time content resolution for secret-bearing entries
//! - [`semantic`] — Heuristic analysis of shell config files for duplicate-detection warnings
//! - `template` — Named-value substitution for templated dotfiles

mod backup;
pub mod deploy;
pub mod diff;
pub(crate) mod directory;
pub mod port;
pub(crate) mod resolve;
pub mod semantic;
pub mod service;
pub mod state;
mod state_file;
pub(crate) mod template;
