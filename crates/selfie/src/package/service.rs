//! Package service implementation and core business logic
//!
//! This module provides the main service layer for package operations in the selfie library.
//! It implements the hexagonal architecture pattern with two primary ports:
//!
//! - [`SpecService`] — file/definition operations (create, validate, update, remove, spec_info)
//!   that work with package YAML files but never execute commands.
//! - [`PackageService`] — runtime operations (check, audit, install, list, status) that may
//!   execute system commands via [`CommandRunner`](crate::commands::runner::CommandRunner).
//!
//! Both traits are implemented by [`PackageServiceImpl`], which coordinates between the package
//! repository, command runner, and event streaming.

mod audit;
mod check;
mod create;
mod deps;
mod info;
mod install;
mod list;
mod remove;
mod spec_list;
mod steps;
mod update;
mod validate;
mod validate_all;

use std::{future::Future, path::PathBuf};

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::instrument;

use super::{
    event::{
        EventSender, EventStream, OperationContext, OperationResult, PackageEvent,
        metadata::OperationType,
    },
    git::GitStatusProvider,
    port::PackageRepository,
};

use crate::{commands::runner::CommandRunner, config::SelfieConfig};

/// Helper for tracking progress through operation steps
///
/// Provides a simple mechanism for tracking and reporting progress through
/// multi-step operations. Each operation can define a total number of steps
/// and then advance through them while providing user feedback.
#[derive(Debug, Clone)]
pub(crate) struct ProgressTracker {
    /// Current step number (0-based internally, 1-based for display)
    current_step: usize,
    /// Total number of steps in the operation
    total_steps: usize,
}

impl ProgressTracker {
    /// Create a new progress tracker for an operation with the specified number of steps
    ///
    /// # Arguments
    ///
    /// * `total_steps` - Total number of steps in the operation
    pub(crate) fn new(total_steps: usize) -> Self {
        Self {
            current_step: 0,
            total_steps,
        }
    }

    /// Advance to the next step and send a progress event
    ///
    /// Increments the current step counter and sends a progress event with
    /// the provided message, enhanced with step numbers (e.g., "Installing package (2/5)").
    ///
    /// # Arguments
    ///
    /// * `sender` - Event sender for broadcasting progress updates
    /// * `message` - Progress message to display to the user
    pub(crate) async fn next(&mut self, sender: &EventSender, message: impl std::fmt::Display) {
        self.current_step += 1;
        let enhanced_message = format!("{} ({}/{})", message, self.current_step, self.total_steps);
        sender
            .send_progress(self.current_step, self.total_steps, enhanced_message)
            .await;
    }

    /// Get the current step number (1-based for display)
    pub(crate) fn current_step(&self) -> usize {
        self.current_step
    }

    /// Get the total number of steps in the operation
    pub(crate) fn total_steps(&self) -> usize {
        self.total_steps
    }

    /// Update the total number of steps after initial creation
    ///
    /// This is useful when the total step count isn't known until after
    /// dependency resolution determines how many packages need installing.
    pub(crate) fn set_total_steps(&mut self, total: usize) {
        self.total_steps = total;
    }

    /// Reduce the total number of steps by the given amount
    ///
    /// Used when an early return skips steps (e.g., a package is already
    /// installed and skips the install/verify/complete steps).
    pub(crate) fn reduce_total_steps(&mut self, by: usize) {
        self.total_steps = self.total_steps.saturating_sub(by);
    }
}

/// Port for package definition (spec/file) operations (Hexagonal Architecture)
///
/// This trait defines the interface for operations on package definition files:
/// creating, validating, updating, removing, and retrieving spec info. These
/// operations do not run commands or check installation status.
#[cfg_attr(any(test, feature = "with_mocks"), mockall::automock)]
pub trait SpecService: Send + Sync {
    /// Create a new package definition file
    fn create(&self, package: super::Package) -> impl Future<Output = EventStream> + Send;

    /// Validate a package definition file
    fn validate(
        &self,
        package_name: &str,
        package_path: Option<PathBuf>,
    ) -> impl Future<Output = EventStream> + Send;

    /// Update a package's fields
    fn update(
        &self,
        package_name: &str,
        fields: super::event::PackageUpdateFields,
    ) -> impl Future<Output = EventStream> + Send;

    /// Remove a package from the repository
    fn remove(&self, package_name: &str) -> impl Future<Output = EventStream> + Send;

    /// Get package definition info (no runtime status check)
    fn spec_info(&self, package_name: &str) -> impl Future<Output = EventStream> + Send;

    /// List all specs without checking runtime status
    fn list(&self, show_all: bool) -> impl Future<Output = EventStream> + Send;

    /// Validate all specs for the current environment
    fn validate_all(&self) -> impl Future<Output = EventStream> + Send;
}

/// Port for runtime package operations (Hexagonal Architecture)
///
/// This trait defines the interface for operations that interact with the system
/// at runtime: checking installation status, installing packages, auditing, and
/// listing. These operations may execute commands.
#[cfg_attr(any(test, feature = "with_mocks"), mockall::automock)]
pub trait PackageService: Send + Sync {
    /// Check if a package is already installed
    fn check(&self, package_name: &str) -> impl Future<Output = EventStream> + Send;

    /// Audit a package's installation sources and detect conflicts
    fn audit(&self, package_name: &str) -> impl Future<Output = EventStream> + Send;

    /// Audit all packages' installation sources and detect conflicts
    fn audit_all(&self) -> impl Future<Output = EventStream> + Send;

    /// Install a package using its configured installation method
    fn install(&self, package_name: &str) -> impl Future<Output = EventStream> + Send;

    /// List all available packages in the package directory
    fn list(&self, show_all: bool) -> impl Future<Output = EventStream> + Send;

    /// Check installation status for a package in the current environment
    fn status(&self, package_name: &str) -> impl Future<Output = EventStream> + Send;
}

/// Concrete implementation of the `PackageService` trait
///
/// This implementation coordinates between the package repository (for loading
/// package definitions), command runner (for executing installation/check commands),
/// and application configuration to provide complete package management functionality.
///
/// The implementation uses dependency injection through generic parameters to
/// support different storage backends and command execution strategies.
#[derive(Debug)]
pub struct PackageServiceImpl<R, CR, G = super::git_adapter::GixGitStatusProvider> {
    /// Repository for loading and managing package definitions
    package_repository: R,
    /// Command runner for executing system commands
    command_runner: CR,
    /// Git status provider for annotating specs with git state
    git_provider: G,
    /// Application configuration including environment and settings
    config: SelfieConfig,
    /// Token used to signal graceful cancellation of in-flight operations
    cancellation_token: CancellationToken,
}

impl<R, CR, G> PackageServiceImpl<R, CR, G>
where
    R: PackageRepository + Clone + 'static,
    CR: CommandRunner + Clone + 'static,
    G: GitStatusProvider + Clone + 'static,
{
    /// Create a new package service instance
    ///
    /// # Arguments
    ///
    /// * `package_repository` - Repository implementation for package storage
    /// * `command_runner` - Command runner implementation for executing system commands
    /// * `git_provider` - Git status provider for file status lookups
    /// * `config` - Application configuration
    pub fn new(
        package_repository: R,
        command_runner: CR,
        git_provider: G,
        config: SelfieConfig,
        cancellation_token: CancellationToken,
    ) -> Self {
        Self {
            package_repository,
            command_runner,
            git_provider,
            config,
            cancellation_token,
        }
    }

    /// Create an event stream from an async operation
    ///
    /// This helper function creates a [`futures::Stream`] that emits [`PackageEvent`]s
    /// from an async operation. The operation is executed in a background task and
    /// communicates through a channel.
    ///
    /// # Arguments
    ///
    /// * `f` - Async function that takes an event sender and performs the operation
    ///
    /// # Returns
    ///
    /// A boxed stream of package events
    fn create_event_stream<F, Fut>(f: F) -> EventStream
    where
        F: FnOnce(mpsc::Sender<PackageEvent>) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = ()> + Send,
    {
        let (tx, rx) = mpsc::channel(32);

        tokio::spawn(async move {
            f(tx).await;
        });

        Box::pin(futures::stream::unfold(rx, |mut rx| async move {
            rx.recv().await.map(|event| (event, rx))
        }))
    }

    /// Execute an operation with full dependency injection and standard event handling
    ///
    /// This helper method provides a standardized way to execute package operations
    /// with proper event handling, progress tracking, and dependency injection.
    /// It handles operation startup, environment logging, and result completion.
    ///
    /// # Arguments
    ///
    /// * `operation_type` - Type of operation being performed
    /// * `package_name` - Name of the package being operated on
    /// * `context` - Additional operation context (paths, target environment, etc.)
    /// * `total_steps` - Total number of steps for progress tracking
    /// * `handler` - Async function that performs the actual operation
    ///
    /// # Returns
    ///
    /// An event stream that emits operation progress and results
    fn execute_operation_with_deps<F, Fut>(
        &self,
        operation_type: OperationType,
        package_name: &str,
        context: OperationContext,
        total_steps: usize,
        handler: F,
    ) -> EventStream
    where
        F: FnOnce(R, CR, SelfieConfig, EventSender, ProgressTracker, CancellationToken) -> Fut
            + Send
            + 'static,
        Fut: std::future::Future<Output = OperationResult> + Send,
    {
        let repo = self.package_repository.clone();
        let command_runner = self.command_runner.clone();
        let config = self.config.clone();
        let package_name = package_name.to_string();
        let token = self.cancellation_token.clone();

        Self::create_event_stream(move |tx| async move {
            let sender = EventSender::new_with_context(
                tx.clone(),
                operation_type,
                package_name.clone(),
                config.environment().to_string(),
                context,
            );

            // Check for cancellation before starting
            if token.is_cancelled() {
                sender
                    .send_canceled("Operation cancelled before start")
                    .await;
                return;
            }

            sender.send_started().await;
            sender
                .send_trace(format!("Current environment: {}", config.environment()))
                .await;

            let progress = ProgressTracker::new(total_steps);
            let result = handler(
                repo,
                command_runner,
                config,
                sender.clone(),
                progress,
                token.clone(),
            )
            .await;

            // If cancelled during execution, emit Canceled instead of Completed
            if token.is_cancelled() {
                sender.send_canceled("Operation cancelled").await;
            } else {
                sender.send_completed(result).await;
            }
        })
    }
}

impl<R, CR, G> SpecService for PackageServiceImpl<R, CR, G>
where
    R: PackageRepository + Clone + std::fmt::Debug + Send + Sync + 'static,
    CR: CommandRunner + Clone + std::fmt::Debug + Send + Sync + 'static,
    G: GitStatusProvider + Clone + std::fmt::Debug + Send + Sync + 'static,
{
    #[instrument]
    async fn create(&self, package: super::Package) -> EventStream {
        let package_name = package.name().to_string();
        self.execute_operation_with_deps(
            OperationType::PackageCreate,
            &package_name,
            OperationContext::default(),
            2, // Check existence + save
            move |repo, _, config, sender, mut progress, _token| async move {
                create::handle_create(package, &repo, &config, &sender, &mut progress).await
            },
        )
    }

    async fn validate(&self, package_name: &str, package_path: Option<PathBuf>) -> EventStream {
        let context = OperationContext {
            package_path,
            target_environment: None,
        };

        let package_name_owned = package_name.to_string();
        self.execute_operation_with_deps(
            OperationType::PackageValidate,
            package_name,
            context,
            3, // load_package + validate_package + result processing
            move |repo, _, config, sender, mut progress, _token| async move {
                validate::handle_validate(
                    &package_name_owned,
                    &repo,
                    &config,
                    &sender,
                    &mut progress,
                )
                .await
            },
        )
    }

    #[instrument]
    async fn update(
        &self,
        package_name: &str,
        fields: super::event::PackageUpdateFields,
    ) -> EventStream {
        let package_name_owned = package_name.to_string();
        self.execute_operation_with_deps(
            OperationType::PackageUpdate,
            package_name,
            OperationContext::default(),
            3, // Load + apply changes + save
            move |repo, _, config, sender, mut progress, _token| async move {
                update::handle_update(
                    &package_name_owned,
                    fields,
                    &repo,
                    &config,
                    &sender,
                    &mut progress,
                )
                .await
            },
        )
    }

    #[instrument]
    async fn remove(&self, package_name: &str) -> EventStream {
        let package_name_owned = package_name.to_string();
        self.execute_operation_with_deps(
            OperationType::PackageRemove,
            package_name,
            OperationContext::default(),
            3, // Load + check dependents + remove
            move |repo, _, config, sender, mut progress, _token| async move {
                remove::handle_remove(&package_name_owned, &repo, &config, &sender, &mut progress)
                    .await
            },
        )
    }

    async fn spec_info(&self, package_name: &str) -> EventStream {
        let package_name_owned = package_name.to_string();
        let git = self.git_provider.clone();
        self.execute_operation_with_deps(
            OperationType::SpecInfo,
            package_name,
            OperationContext::default(),
            2, // Load package + gather info
            move |repo, _, config, sender, mut progress, _token| async move {
                info::handle_spec_info(
                    &package_name_owned,
                    &repo,
                    &config,
                    &git,
                    &sender,
                    &mut progress,
                )
                .await
            },
        )
    }

    async fn list(&self, show_all: bool) -> EventStream {
        let git = self.git_provider.clone();
        self.execute_operation_with_deps(
            OperationType::SpecList,
            "",
            OperationContext::default(),
            2, // Load packages + emit items
            move |repo, _, config, sender, mut progress, _token| async move {
                spec_list::handle_spec_list(&repo, &config, &git, &sender, &mut progress, show_all)
                    .await
            },
        )
    }

    async fn validate_all(&self) -> EventStream {
        self.execute_operation_with_deps(
            OperationType::SpecValidateAll,
            "",
            OperationContext::default(),
            2, // Load packages + validate each
            move |repo, _, config, sender, mut progress, _token| async move {
                validate_all::handle_validate_all(&repo, &config, &sender, &mut progress).await
            },
        )
    }
}

impl<R, CR, G> PackageService for PackageServiceImpl<R, CR, G>
where
    R: PackageRepository + Clone + std::fmt::Debug + Send + Sync + 'static,
    CR: CommandRunner + Clone + std::fmt::Debug + Send + Sync + 'static,
    G: GitStatusProvider + Clone + std::fmt::Debug + Send + Sync + 'static,
{
    #[instrument]
    async fn check(&self, package_name: &str) -> EventStream {
        let package_name_owned = package_name.to_string();
        self.execute_operation_with_deps(
            OperationType::PackageCheck,
            package_name,
            OperationContext::default(),
            3, // Load package + check environment + run check command
            move |repo, command_runner, config, sender, mut progress, token| async move {
                check::handle_check(
                    &package_name_owned,
                    &repo,
                    &config,
                    &command_runner,
                    &sender,
                    &mut progress,
                    &token,
                )
                .await
            },
        )
    }

    #[instrument]
    async fn audit(&self, package_name: &str) -> EventStream {
        let package_name_owned = package_name.to_string();
        self.execute_operation_with_deps(
            OperationType::PackageAudit,
            package_name,
            OperationContext::default(),
            3, // Load package + check environment + run audit command
            move |repo, command_runner, config, sender, mut progress, token| async move {
                audit::handle_audit(
                    &package_name_owned,
                    &repo,
                    &config,
                    &command_runner,
                    &sender,
                    &mut progress,
                    &token,
                )
                .await
            },
        )
    }

    #[instrument]
    async fn audit_all(&self) -> EventStream {
        self.execute_operation_with_deps(
            OperationType::PackageAudit,
            "",
            OperationContext::default(),
            1,
            move |repo, command_runner, config, sender, mut progress, token| async move {
                audit::handle_audit_all(
                    &repo,
                    &config,
                    &command_runner,
                    &sender,
                    &mut progress,
                    &token,
                )
                .await
            },
        )
    }

    #[instrument]
    async fn install(&self, package_name: &str) -> EventStream {
        let package_name_owned = package_name.to_string();
        self.execute_operation_with_deps(
            OperationType::PackageInstall,
            package_name,
            OperationContext::default(),
            1, // Initial step (dependency resolution); total is adjusted dynamically
            move |repo, command_runner, config, sender, mut progress, token| async move {
                install::handle_install(
                    &package_name_owned,
                    &repo,
                    &config,
                    &command_runner,
                    &sender,
                    &mut progress,
                    &token,
                )
                .await
            },
        )
    }

    async fn list(&self, show_all: bool) -> EventStream {
        self.execute_operation_with_deps(
            OperationType::PackageList,
            "",
            OperationContext::default(),
            2, // Load packages + check status
            move |repo, command_runner, config, sender, mut progress, token| async move {
                list::handle_list(
                    &repo,
                    &config,
                    &command_runner,
                    &sender,
                    &mut progress,
                    show_all,
                    &token,
                )
                .await
            },
        )
    }

    async fn status(&self, package_name: &str) -> EventStream {
        let package_name_owned = package_name.to_string();
        self.execute_operation_with_deps(
            OperationType::PackageStatus,
            package_name,
            OperationContext::default(),
            2, // Load package + check status
            move |repo, command_runner, config, sender, mut progress, token| async move {
                info::handle_status(
                    &package_name_owned,
                    &repo,
                    &config,
                    &command_runner,
                    &sender,
                    &mut progress,
                    &token,
                )
                .await
            },
        )
    }
}
