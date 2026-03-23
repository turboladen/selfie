//! Sync command handlers — status, push, pull.
//!
//! These commands wrap git operations for syncing package specs and dotfiles
//! across machines. See the design spec at
//! `docs/superpowers/specs/2026-03-23-git-sync-design.md`.

pub(crate) mod pull;
pub(crate) mod push;
pub(crate) mod status;
