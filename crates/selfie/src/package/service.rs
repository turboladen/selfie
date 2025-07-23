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
mod info;
mod install;
mod list;
mod steps;
mod validate;

use std::path::PathBuf;

use tokio::sync::mpsc;
use tracing::instrument;

use super::{
    event::{
        EventSender, EventStream, OperationContext, OperationResult, PackageEvent,
        metadata::OperationType,
    },
    port::PackageRepository,
};

use crate::{commands::runner::CommandRunner, config::AppConfig, package::port::PackageError};

/// Helper for tracking progress through operation steps
///
/// Provides a simple mechanism for tracking and reporting progress through
/// multi-step operations. Each operation can define a total number of steps
/// and then advance through them while providing user feedback.
#[derive(Debug, Clone)]
pub(crate) struct ProgressTracker {
    /// Current step number (0-based internally, 1-based for display)
    current_step: u32,
    /// Total number of steps in the operation
    total_steps: u32,
}

impl ProgressTracker {
    /// Create a new progress tracker for an operation with the specified number of steps
    ///
    /// # Arguments
    ///
    /// * `total_steps` - Total number of steps in the operation
    pub(crate) fn new(total_steps: u32) -> Self {
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
    pub(crate) fn current_step(&self) -> u32 {
        self.current_step
    }

    /// Get the total number of steps in the operation
    pub(crate) fn total_steps(&self) -> u32 {
        self.total_steps
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
#[cfg_attr(test, mockall::automock)]
#[async_trait::async_trait]
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
    async fn check(&self, package_name: &str) -> EventStream;

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
    async fn install(&self, package_name: &str) -> EventStream;

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
    /// An event stream with package information, or an error if the package cannot be found
    ///
    /// # Errors
    ///
    /// Returns [`PackageError`] if:
    /// - The package definition file cannot be found
    /// - Multiple packages with the same name are found
    /// - The package definition file is malformed
    /// - File system access fails during package loading
    /// - The package repository is inaccessible
    async fn info(&self, package_name: &str) -> Result<EventStream, PackageError>;

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
    /// An event stream with validation results, or an error if validation cannot proceed
    ///
    /// # Errors
    ///
    /// Returns [`PackageError`] if:
    /// - The package definition file cannot be found when path not specified
    /// - Multiple packages with the same name are found
    /// - The specified package path does not exist or is not accessible
    /// - The package definition file cannot be read due to permissions
    /// - File system access fails during validation setup
    /// - The package repository is inaccessible
    async fn validate(
        &self,
        package_name: &str,
        package_path: Option<PathBuf>,
    ) -> Result<EventStream, PackageError>;

    /// List all available packages in the package directory
    ///
    /// Discovers and lists all package definition files in the configured
    /// package directory, providing basic information about each package.
    ///
    /// # Returns
    ///
    /// An event stream with the list of available packages, or an error if listing fails
    ///
    /// # Errors
    ///
    /// Returns [`PackageError`] if:
    /// - The package directory cannot be accessed due to permissions or path issues
    /// - The package directory does not exist
    /// - Package definition files cannot be read due to permissions
    /// - File system access fails during directory traversal
    /// - The package repository encounters critical errors during listing
    async fn list(&self) -> Result<EventStream, PackageError>;

    /// Create a new package definition file
    ///
    /// Creates a new package definition file with a basic template structure.
    /// This provides a starting point for users to define their own packages.
    ///
    /// # Arguments
    ///
    /// * `package_name` - Name of the new package to create
    ///
    /// # Returns
    ///
    /// An event stream with creation progress, or an error if creation fails
    ///
    /// # Errors
    ///
    /// Returns [`PackageError`] if:
    /// - A package with the same name already exists in the package directory
    /// - The package directory is not writable due to permissions
    /// - The package directory does not exist and cannot be created
    /// - File system access fails during template creation or file writing
    /// - Package name contains invalid characters for file system usage
    /// - Disk space is insufficient for creating the package file
    async fn create(&self, package_name: &str) -> Result<EventStream, PackageError>;
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
    config: AppConfig,
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
    pub fn new(package_repository: R, command_runner: CR, config: AppConfig) -> Self {
        Self {
            package_repository,
            command_runner,
            config,
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
        total_steps: u32,
        handler: F,
    ) -> EventStream
    where
        F: FnOnce(R, CR, AppConfig, EventSender, ProgressTracker) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = OperationResult> + Send,
    {
        let repo = self.package_repository.clone();
        let command_runner = self.command_runner.clone();
        let config = self.config.clone();
        let package_name = package_name.to_string();

        Self::create_event_stream(move |tx| async move {
            let sender = EventSender::new_with_context(
                tx.clone(),
                operation_type,
                package_name.clone(),
                config.environment().to_string(),
                context,
            );

            sender.send_started().await;
            sender
                .send_trace(format!("Current environment: {}", config.environment()))
                .await;

            let progress = ProgressTracker::new(total_steps);
            let result = handler(repo, command_runner, config, sender.clone(), progress).await;
            sender.send_completed(result).await;
        })
    }

    /// Execute an operation with simplified event handling
    ///
    /// A simpler version of operation execution that doesn't require full dependency
    /// injection. This is useful for operations that don't need access to the
    /// repository or command runner.
    ///
    /// # Arguments
    ///
    /// * `operation_type` - Type of operation being performed
    /// * `package_name` - Name of the package being operated on
    /// * `context` - Additional operation context
    /// * `total_steps` - Total number of steps for progress tracking
    /// * `handler` - Async function that performs the actual operation
    ///
    /// # Returns
    ///
    /// An event stream that emits operation progress and results
    fn execute_operation<F, Fut>(
        &self,
        operation_type: OperationType,
        package_name: &str,
        context: OperationContext,
        total_steps: u32,
        handler: F,
    ) -> EventStream
    where
        F: FnOnce(EventSender, ProgressTracker) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = OperationResult> + Send,
    {
        let config = self.config.clone();
        let package_name = package_name.to_string();

        Self::create_event_stream(move |tx| async move {
            let sender = EventSender::new_with_context(
                tx,
                operation_type,
                package_name,
                config.environment().to_string(),
                context,
            );

            sender.send_started().await;
            sender
                .send_trace(format!("Current environment: {}", config.environment()))
                .await;

            let progress = ProgressTracker::new(total_steps);
            let result = handler(sender.clone(), progress).await;
            sender.send_completed(result).await;
        })
    }
}

#[async_trait::async_trait]
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
            move |repo, command_runner, config, sender, mut progress| async move {
                check::handle_check(
                    &package_name_owned,
                    &repo,
                    &config,
                    &command_runner,
                    &sender,
                    &mut progress,
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
            7, // fetch_package + find_env + pre_check + get_command + execute_command + post_check + completion
            move |repo, command_runner, config, sender, mut progress| async move {
                install::handle_install(
                    &package_name_owned,
                    &repo,
                    &config,
                    &command_runner,
                    &sender,
                    &mut progress,
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
    /// Returns [`PackageError`] if:
    /// - The package definition file cannot be found
    /// - The package definition file cannot be read
    /// - Critical validation setup fails
    async fn validate(
        &self,
        package_name: &str,
        package_path: Option<PathBuf>,
    ) -> Result<EventStream, PackageError> {
        let context = OperationContext {
            package_path,
            target_environment: None,
        };

        let package_name_owned = package_name.to_string();
        Ok(self.execute_operation_with_deps(
            OperationType::PackageValidate,
            package_name,
            context,
            3, // load_package + validate_package + result processing
            move |repo, command_runner, config, sender, mut progress| async move {
                validate::handle_validate(
                    &package_name_owned,
                    &repo,
                    &config,
                    &command_runner,
                    &sender,
                    &mut progress,
                )
                .await
            },
        ))
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
    /// An event stream with the list of available packages and their details
    ///
    /// # Errors
    ///
    /// Returns [`PackageError`] if:
    /// - The package directory cannot be accessed
    /// - Package definition files cannot be read
    /// - File system operations fail
    async fn list(&self) -> Result<EventStream, PackageError> {
        Ok(self.execute_operation_with_deps(
            OperationType::PackageList,
            "", // No specific package for list operation
            OperationContext::default(),
            3, // Load packages + process + finalize
            move |repo, command_runner, config, sender, mut progress| async move {
                list::handle_list(&repo, &config, &command_runner, &sender, &mut progress).await
            },
        ))
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
    /// An event stream with detailed package information
    ///
    /// # Errors
    ///
    /// Returns [`PackageError`] if:
    /// - The package definition file cannot be found
    /// - The package definition file is malformed
    /// - File system access fails
    async fn info(&self, package_name: &str) -> Result<EventStream, PackageError> {
        let package_name_owned = package_name.to_string();
        Ok(self.execute_operation_with_deps(
            OperationType::PackageInfo,
            package_name,
            OperationContext::default(),
            3, // Load package + gather info + check status
            move |repo, command_runner, config, sender, mut progress| async move {
                info::handle_info(
                    &package_name_owned,
                    &repo,
                    &config,
                    &command_runner,
                    &sender,
                    &mut progress,
                )
                .await
            },
        ))
    }

    /// Create a new package definition file
    ///
    /// Creates a new package definition file with a basic template structure
    /// in the configured package directory. The template includes placeholders
    /// for common configuration options and environment setups.
    ///
    /// # Arguments
    ///
    /// * `package_name` - Name of the new package to create
    ///
    /// # Returns
    ///
    /// An event stream with creation progress and result
    ///
    /// # Errors
    ///
    /// Returns [`PackageError`] if:
    /// - A package with the same name already exists
    /// - The package directory is not writable
    /// - File system operations fail
    ///
    /// # Note
    ///
    /// This operation is currently not fully implemented and will return
    /// a placeholder success message.
    async fn create(&self, package_name: &str) -> Result<EventStream, PackageError> {
        Ok(self.execute_operation(
            OperationType::PackageCreate,
            package_name,
            OperationContext::default(),
            1, // Just one step for creation
            |_sender, mut _progress| async move {
                // TODO: Implement actual creation logic
                OperationResult::Success("Create operation not yet implemented".to_string())
            },
        ))
    }
}
