//! Git sync service for pushing and pulling package specs.
//!
//! This module provides the [`SyncService`] trait and its concrete implementation
//! [`SyncServiceImpl`], which orchestrate git operations for syncing selfie's
//! package specs and dotfiles across machines.
//!
//! The service follows a two-phase push architecture:
//! 1. [`SyncService::prepare_push`] — analyzes changes and generates commits (non-mutating)
//! 2. [`SyncService::execute_push`] — stages, commits, and pushes (mutating, returns event stream)
//!
//! This keeps the service non-interactive while letting callers (CLI, MCP) decide
//! how to confirm or customize commit messages between phases.

pub mod port;
pub mod service;

pub use self::port::{ConfirmedCommit, PendingCommit, PrepareResult, PushOptions, SyncService};
pub use self::service::SyncServiceImpl;
