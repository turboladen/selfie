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
//! - [`service`] — The adapter: collecting packages, and the `DotfileService` impl
//! - `apply` — Applying every entry of every selected package
//! - `drift` — Reporting how far every tracked target has drifted from its source
//! - [`track`] — Taking a file the user already has under management
//! - `backup` — Copies of what a target held before an apply overwrote it
//! - [`deploy`] — Pure decision logic: checksums, path resolution, deploy-vs-skip-vs-conflict
//! - `deploy_entry` — Writing one entry to its target and recording what was written
//! - [`state`] — `DeployState` persistence: per-machine checksum tracking and drift detection
//! - [`diff`] — Unified diff generation for conflict display
//! - `directory` — What selfie says about the standalone dotfiles directory
//! - `resolve` — Apply-time content resolution for secret-bearing entries
//! - `refusal` — What is at a deploy target, and how a refused deploy is worded
//! - `secret` — Deploying the secret-bearing entries of one package
//! - `warning` — What collecting packages found worth saying, and name collisions
//! - [`semantic`] — Heuristic analysis of shell config files for duplicate-detection warnings
//! - `template` — Named-value substitution for templated dotfiles
//! - `state_file` — Reading and writing the deploy-state file

mod apply;
mod backup;
pub mod deploy;
mod deploy_entry;
pub mod diff;
pub(crate) mod directory;
mod drift;
pub mod port;
mod refusal;
pub(crate) mod resolve;
mod secret;
pub mod semantic;
pub mod service;
pub mod state;
mod state_file;
pub(crate) mod template;
pub mod track;
mod warning;
