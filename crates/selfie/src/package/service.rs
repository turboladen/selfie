//! Package service implementation and core business logic
//!
//! This module provides the main service layer for package operations in the selfie library.
//! It implements the hexagonal architecture pattern with the `PackageService` trait as the
//! primary port for package management operations.
//!
//! The service handles all package lifecycle operations including installation, checking,
//! validation, and information retrieval. It coordinates between the package repository,
//! command execution, and event streaming to provide a complete package management experience.

mod check;
mod create;
mod deps;
mod info;
mod install;
mod list;
mod steps;
mod validate;

use std::{future::Future, path::PathBuf};

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::instrument;

use super::{
    event::{
        EventSender, EventStream, OperationContext, OperationResult, PackageEvent,
        metadata::OperationType,
    },
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
}

/// Primary port for package operations (Hexagonal Architecture)
///
/// This trait defines the main interface for all package management operations
/// in the selfie library. It abstracts the business logic from UI concerns by
/// providing an event-driven interface that streams operation progress and results.
///
/// All operations return an `EventStream` that allows real-time monitoring of
/// progress, errors, and results. This enables different UI implementations
/// (CLI, GUI, etc.) to provide appropriate user feedback.
#[cfg_attr(any(test, feature = "with_mocks"), mockall::automock)]
pub trait PackageService: Send + Sync {
    /// Check if a package is already installed
    ///
    /// Runs the package's configured check command to determine if it's already
    /// installed in the current environment. This is useful before attempting
    /// installation to avoid unnecessary work.
    ///
    /// # Arguments
    ///
    /// * `package_name` - Name of the package to check
    ///
    /// # Returns
    ///
    /// An event stream that will emit progress events and the final check result
    ///
    /// # Errors
    ///
    /// This method returns an `EventStream` directly and cannot fail at the call site.
    /// However, errors may be emitted through the event stream.
    fn check(&self, package_name: &str) -> impl Future<Output = EventStream> + Send;

    /// Install a package using its configured installation method
    ///
    /// Executes the package's installation command for the current environment.
    /// This includes dependency resolution, command validation, and installation
    /// execution with progress tracking.
    ///
    /// # Arguments
    ///
    /// * `package_name` - Name of the package to install
    ///
    /// # Returns
    ///
    /// An event stream that will emit progress events and the final installation result
    ///
    /// # Errors
    ///
    /// This method returns an `EventStream` directly and cannot fail at the call site,
    /// however, errors may be emitted through the event stream.
    fn install(&self, package_name: &str) -> impl Future<Output = EventStream> + Send;

    /// Get detailed information about a package
    ///
    /// Retrieves comprehensive information about a package including its
    /// configuration, available environments, dependencies, and current
    /// installation status.
    ///
    /// # Arguments
    ///
    /// * `package_name` - Name of the package to get information about
    ///
    /// # Returns
    ///
    /// An event stream that will emit package information events and the final result
    ///
    /// # Errors
    ///
    /// This method returns an `EventStream` directly and cannot fail at the call site.
    /// However, errors may be emitted through the event stream.
    fn info(&self, package_name: &str) -> impl Future<Output = EventStream> + Send;

    /// Validate a package definition file
    ///
    /// Performs comprehensive validation of a package definition including
    /// schema validation, environment configuration checks, and command
    /// syntax verification.
    ///
    /// # Arguments
    ///
    /// * `package_name` - Name of the package to validate
    /// * `package_path` - Optional explicit path to the package file
    ///
    /// # Returns
    ///
    /// An event stream that will emit validation results and the final result
    ///
    /// # Errors
    ///
    /// This method returns an `EventStream` directly and cannot fail at the call site.
    /// However, errors may be emitted through the event stream.
    fn validate(
        &self,
        package_name: &str,
        package_path: Option<PathBuf>,
    ) -> impl Future<Output = EventStream> + Send;

    /// List all available packages in the package directory
    ///
    /// Discovers and lists all package definition files in the configured
    /// package directory, providing basic information about each package.
    ///
    /// # Returns
    ///
    /// An event stream that will emit package list events and the final result
    ///
    /// # Errors
    ///
    /// This method returns an `EventStream` directly and cannot fail at the call site.
    /// However, errors may be emitted through the event stream.
    fn list(&self, show_all: bool) -> impl Future<Output = EventStream> + Send;

    /// Create a new package definition file
    ///
    /// Saves the provided package to the repository if no package with
    /// the same name already exists.
    ///
    /// # Arguments
    ///
    /// * `package` - The fully constructed package to create
    ///
    /// # Returns
    ///
    /// An event stream that will emit creation progress events and the final result
    ///
    /// # Errors
    ///
    /// This method returns an `EventStream` directly and cannot fail at the call site.
    /// However, errors may be emitted through the event stream.
    fn create(&self, package: super::Package) -> impl Future<Output = EventStream> + Send;
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
pub struct PackageServiceImpl<R, CR> {
    /// Repository for loading and managing package definitions
    package_repository: R,
    /// Command runner for executing system commands
    command_runner: CR,
    /// Application configuration including environment and settings
    config: SelfieConfig,
    /// Token used to signal graceful cancellation of in-flight operations
    cancellation_token: CancellationToken,
}

impl<R, CR> PackageServiceImpl<R, CR>
where
    R: PackageRepository + Clone + 'static,
    CR: CommandRunner + Clone + 'static,
{
    /// Create a new package service instance
    ///
    /// # Arguments
    ///
    /// * `package_repository` - Repository implementation for package storage
    /// * `command_runner` - Command runner implementation for executing system commands
    /// * `config` - Application configuration
    pub fn new(
        package_repository: R,
        command_runner: CR,
        config: SelfieConfig,
        cancellation_token: CancellationToken,
    ) -> Self {
        Self {
            package_repository,
            command_runner,
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

impl<R, CR> PackageService for PackageServiceImpl<R, CR>
where
    R: PackageRepository + Clone + std::fmt::Debug + Send + Sync + 'static,
    CR: CommandRunner + Clone + std::fmt::Debug + Send + Sync + 'static,
{
    /// Check if a package is already installed
    ///
    /// Runs the package's configured check command to determine installation status.
    /// This operation loads the package definition, validates the environment
    /// configuration, and executes the check command if available.
    ///
    /// The check operation consists of:
    /// 1. Loading the package definition from the repository
    /// 2. Validating the current environment configuration
    /// 3. Executing the package's check command (if configured)
    ///
    /// # Arguments
    ///
    /// * `package_name` - Name of the package to check
    ///
    /// # Returns
    ///
    /// An event stream that emits:
    /// - Progress events for each step
    /// - Success/failure result with installation status
    /// - Error events if the operation fails
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

    /// Install a package using its configured installation method
    ///
    /// Executes the complete package installation process including dependency
    /// resolution, environment validation, and command execution. This operation
    /// will check if the package is already installed before proceeding.
    ///
    /// The installation operation consists of:
    /// 1. Loading the package definition from the repository
    /// 2. Validating the current environment configuration
    /// 3. Resolving and checking dependencies
    /// 4. Executing the package's installation command
    /// 5. Verifying the installation was successful
    ///
    /// # Arguments
    ///
    /// * `package_name` - Name of the package to install
    ///
    /// # Returns
    ///
    /// An event stream that emits:
    /// - Progress events for each installation step
    /// - Success/failure result with installation details
    /// - Error events if the installation fails
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

    /// Validate a package definition file
    ///
    /// Performs comprehensive validation of a package definition including
    /// schema validation, environment configuration checks, command syntax
    /// verification, and dependency validation.
    ///
    /// The validation operation consists of:
    /// 1. Loading the package definition (from path or repository)
    /// 2. Validating the package schema and required fields
    /// 3. Checking environment configurations and commands
    ///
    /// # Arguments
    ///
    /// * `package_name` - Name of the package to validate
    /// * `package_path` - Optional explicit path to the package file
    ///
    /// # Returns
    ///
    /// An event stream with validation results including any issues found
    ///
    /// # Errors
    ///
    /// This method returns an `EventStream` directly and cannot fail at the call site.
    /// However, errors may be emitted through the event stream.
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

    /// List all available packages in the package directory
    ///
    /// Discovers and lists all package definition files in the configured
    /// package directory. For each package, provides basic information
    /// including name, version, description, and available environments.
    ///
    /// The list operation consists of:
    /// 1. Scanning the package directory for definition files
    /// 2. Loading and parsing each package definition
    /// 3. Collecting package metadata and status information
    ///
    /// # Returns
    ///
    /// An event stream that will emit package list events and the final result
    ///
    /// # Errors
    ///
    /// This method returns an `EventStream` directly and cannot fail at the call site.
    /// However, errors may be emitted through the event stream.
    async fn list(&self, show_all: bool) -> EventStream {
        self.execute_operation_with_deps(
            OperationType::PackageList,
            "", // No specific package for list operation
            OperationContext::default(),
            5, // Load + process + sort + check status + finalize
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

    /// Get detailed information about a package
    ///
    /// Retrieves comprehensive information about a package including its
    /// configuration, available environments, dependencies, installation
    /// status, and command details. This is useful for troubleshooting
    /// and understanding package configurations.
    ///
    /// The info operation consists of:
    /// 1. Loading the package definition from the repository
    /// 2. Gathering package metadata and configuration details
    /// 3. Checking current installation status (if check command available)
    ///
    /// # Arguments
    ///
    /// * `package_name` - Name of the package to get information about
    ///
    /// # Returns
    ///
    /// An event stream that will emit package information events and the final result
    ///
    /// # Errors
    ///
    /// This method returns an `EventStream` directly and cannot fail at the call site.
    /// However, errors may be emitted through the event stream.
    async fn info(&self, package_name: &str) -> EventStream {
        let package_name_owned = package_name.to_string();
        self.execute_operation_with_deps(
            OperationType::PackageInfo,
            package_name,
            OperationContext::default(),
            3, // Load package + gather info + check status
            move |repo, command_runner, config, sender, mut progress, token| async move {
                info::handle_info(
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

    /// Create a new package definition file
    ///
    /// Saves the provided package to the repository if no package with
    /// the same name already exists. The operation checks for existing
    /// packages and writes the new definition file.
    ///
    /// The create operation consists of:
    /// 1. Checking if a package with the same name already exists
    /// 2. Saving the package definition to the repository
    ///
    /// # Arguments
    ///
    /// * `package` - The fully constructed package to create
    ///
    /// # Returns
    ///
    /// An event stream that will emit creation progress events and the final result
    ///
    /// # Errors
    ///
    /// This method returns an `EventStream` directly and cannot fail at the call site.
    /// However, errors may be emitted through the event stream.
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
}
