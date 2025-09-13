//! Package operation event system
//!
//! This module provides the event-driven interface for package operations in the selfie library.
//! It implements a streaming event system that allows real-time monitoring of package operations
//! including progress updates, log messages, and results.
//!
//! # Architecture
//!
//! The event system follows a publisher-subscriber pattern where:
//! - Operations publish events through [`EventSender`]
//! - Consumers receive events through [`EventStream`]
//! - Events carry rich context including operation metadata and results
//!
//! # Event Types
//!
//! - [`PackageEvent::Started`] - Operation has begun
//! - [`PackageEvent::Progress`] - Progress update with step information
//! - [`PackageEvent::Completed`] - Operation finished with result
//! - [`PackageEvent::Trace`], [`PackageEvent::Debug`], [`PackageEvent::Warning`] - Log messages
//!
//! # Usage
//!
//! Events are emitted by package service operations and consumed by UI layers
//! to provide real-time feedback to users. The stream-based approach allows
//! for non-blocking operation monitoring and flexible UI implementations.

pub mod error;
pub mod metadata;

/// Represents the completion status of steps in an operation
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StepCount {
    pub completed: usize,
    pub total: usize,
}

impl StepCount {
    #[must_use]
    pub fn new(completed: usize, total: usize) -> Self {
        Self { completed, total }
    }
}

impl std::fmt::Display for StepCount {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "({}/{} steps)", self.completed, self.total)
    }
}

impl From<(usize, usize)> for StepCount {
    fn from((completed, total): (usize, usize)) -> Self {
        Self::new(completed, total)
    }
}

use std::{
    fmt::{self, Debug},
    pin::Pin,
    time::Instant,
};

use futures::Stream;
use tokio::sync::mpsc;
use uuid::Uuid;

use self::{error::StreamedError, metadata::OperationType};

/// Type alias for a stream of package events
///
/// This stream emits [`PackageEvent`] items as operations progress, allowing
/// consumers to react to operation updates in real-time. The stream is pinned
/// and boxed to enable dynamic dispatch and async iteration.
pub type EventStream = Pin<Box<dyn Stream<Item = PackageEvent> + Send>>;

/// Internal event sender for package operations
///
/// Provides a high-level interface for emitting package events with consistent
/// operation context. Automatically includes operation metadata in all events
/// and handles the underlying channel communication.
#[derive(Debug, Clone)]
pub(crate) struct EventSender {
    operation_info: OperationInfo,
    tx: mpsc::Sender<PackageEvent>,
}

impl EventSender {
    /// Create a new event sender with operation context
    ///
    /// # Arguments
    ///
    /// * `tx` - Channel sender for transmitting events
    /// * `operation_type` - Type of operation being performed
    /// * `package_name` - Name of the package being operated on
    /// * `environment` - Environment context for the operation
    /// * `context` - Additional operation context
    pub(crate) fn new_with_context(
        tx: mpsc::Sender<PackageEvent>,
        operation_type: OperationType,
        package_name: String,
        environment: String,
        context: OperationContext,
    ) -> Self {
        let operation_info = OperationInfo {
            id: Uuid::new_v4(),
            operation_type,
            package_name,
            environment,
            context,
            timestamp: Instant::now(),
        };

        Self { operation_info, tx }
    }

    /// Send an event through the channel
    ///
    /// Transmits the event to all consumers listening on the event stream.
    /// Channel send errors are silently ignored as they typically indicate
    /// that the consumer has disconnected.
    ///
    /// # Arguments
    ///
    /// * `event` - The package event to send
    pub(crate) async fn send(&self, event: PackageEvent) {
        let _ = self.tx.send(event).await;
    }

    /// Send a started event for the operation
    pub(crate) async fn send_started(&self) {
        let operation_info = self.touch_operation_info();

        tracing::trace!(
            operation_type = operation_info.operation_type.to_string(),
            package_name = &operation_info.package_name,
            environment = &operation_info.environment,
            "operation started",
        );

        self.send(PackageEvent::Started { operation_info }).await;
    }

    /// Send a progress update
    pub(crate) async fn send_progress(
        &self,
        step: usize,
        total_steps: usize,
        message: impl fmt::Display,
    ) {
        let operation_info = self.touch_operation_info();
        let msg = message.to_string();

        tracing::info!(
            operation_type = operation_info.operation_type.to_string(),
            package_name = &operation_info.package_name,
            environment = &operation_info.environment,
            message = &msg,
            "operation progress",
        );

        #[allow(clippy::cast_precision_loss)]
        self.send(PackageEvent::Progress {
            operation_info,
            step,
            total_steps,
            percent_complete: step as f32 / total_steps as f32,
            message: msg,
        })
        .await;
    }

    /// Send a completion event with the operation result
    pub(crate) async fn send_completed(&self, result: OperationResult) {
        let operation_info = self.touch_operation_info();

        tracing::info!(
            operation_type = operation_info.operation_type.to_string(),
            package_name = &operation_info.package_name,
            environment = &operation_info.environment,
            success = matches!(result, OperationResult::Success(_)),
            "operation completed",
        );

        self.send(PackageEvent::Completed {
            operation_info,
            result,
        })
        .await;
    }

    /// Send a log message at the specified level
    ///
    /// Emits a log event with the appropriate tracing level and event type.
    /// This method handles the conversion from log levels to specific event variants.
    ///
    /// # Arguments
    ///
    /// * `level` - The log level for the message
    /// * `message` - The log message content
    pub(crate) async fn send_log(&self, level: LogLevel, message: impl fmt::Display) {
        let operation_info = self.touch_operation_info();
        let message = message.to_string();

        match level {
            LogLevel::Trace => {
                tracing::trace!(
                    operation_type = operation_info.operation_type.to_string(),
                    package_name = &operation_info.package_name,
                    environment = &operation_info.environment,
                    message = &message,
                );
                self.send(PackageEvent::Trace {
                    operation_info,
                    message,
                })
                .await;
            }
            LogLevel::Debug => {
                tracing::debug!(
                    operation_type = operation_info.operation_type.to_string(),
                    package_name = &operation_info.package_name,
                    environment = &operation_info.environment,
                    message = &message,
                );
                self.send(PackageEvent::Debug {
                    operation_info,
                    message,
                })
                .await;
            }
            LogLevel::Warning => {
                tracing::warn!(
                    operation_type = operation_info.operation_type.to_string(),
                    package_name = &operation_info.package_name,
                    environment = &operation_info.environment,
                    message = &message,
                );
                self.send(PackageEvent::Warning {
                    operation_info,
                    message,
                })
                .await;
            }
        }
    }

    /// Send informational output to the console
    pub(crate) async fn send_info(&self, output: ConsoleOutput) {
        let operation_info = self.touch_operation_info();

        tracing::info!(
            operation_type = operation_info.operation_type.to_string(),
            package_name = &operation_info.package_name,
            environment = &operation_info.environment,
            output = ?&output,
        );

        self.send(PackageEvent::Info {
            operation_info,
            output,
        })
        .await;
    }

    /// Send an error event
    pub(crate) async fn send_error<SE>(&self, error: SE, message: impl fmt::Display)
    where
        StreamedError: From<SE>,
    {
        let operation_info = self.touch_operation_info();
        let msg = message.to_string();
        let streamed_error = StreamedError::from(error);

        tracing::error!(
            operation_type = operation_info.operation_type.to_string(),
            package_name = &operation_info.package_name,
            environment = &operation_info.environment,
            message = &msg,
            error = %streamed_error,
        );

        self.send(PackageEvent::Error {
            operation_info,
            error: streamed_error,
            message: msg,
        })
        .await;
    }

    // Convenience methods for common logging levels
    pub(crate) async fn send_trace(&self, message: impl fmt::Display) {
        self.send_log(LogLevel::Trace, message).await;
    }

    pub(crate) async fn send_debug(&self, message: impl fmt::Display) {
        self.send_log(LogLevel::Debug, message).await;
    }

    pub(crate) async fn send_warning(&self, message: impl fmt::Display) {
        self.send_log(LogLevel::Warning, message).await;
    }

    /// Send package information data
    pub(crate) async fn send_package_info(&self, package_info: PackageInfoData) {
        let operation_info = self.touch_operation_info();
        self.send(PackageEvent::PackageInfoLoaded {
            operation_info,
            package_info,
        })
        .await;
    }

    /// Send environment status data
    pub(crate) async fn send_environment_status(&self, environment_status: EnvironmentStatusData) {
        let operation_info = self.touch_operation_info();
        self.send(PackageEvent::EnvironmentStatusChecked {
            operation_info,
            environment_status,
        })
        .await;
    }

    /// Send package list data
    pub(crate) async fn send_package_list(&self, package_list: PackageListData) {
        let operation_info = self.touch_operation_info();
        self.send(PackageEvent::PackageListLoaded {
            operation_info,
            package_list,
        })
        .await;
    }

    /// Send check result data
    pub(crate) async fn send_check_result(&self, check_result: CheckResultData) {
        let operation_info = self.touch_operation_info();
        self.send(PackageEvent::CheckResultCompleted {
            operation_info,
            check_result,
        })
        .await;
    }

    /// Send validation result data
    pub(crate) async fn send_validation_result(&self, validation_result: ValidationResultData) {
        let operation_info = self.touch_operation_info();
        self.send(PackageEvent::ValidationResultCompleted {
            operation_info,
            validation_result,
        })
        .await;
    }

    fn touch_operation_info(&self) -> OperationInfo {
        let mut info = self.operation_info.clone();
        info.timestamp = Instant::now();
        info
    }
}

/// Information about the operation that generated an event
#[derive(Debug, Clone)]
pub struct OperationInfo {
    /// Unique ID for the operation
    pub id: Uuid,
    /// Type of operation
    pub operation_type: OperationType,
    /// Name of the package being operated on
    pub package_name: String,
    /// Environment context
    pub environment: String,
    /// Additional operation-specific context
    pub context: OperationContext,
    /// Timestamp when the event was created
    pub timestamp: Instant,
}

/// Additional context that operations might need
///
/// This provides a way to pass operation-specific data that doesn't belong
/// in the core `OperationInfo` but is useful for certain operations.
///
/// # Examples
///
/// For package validation with a specific file path:
/// ```rust,ignore
/// let context = OperationContext {
///     package_path: Some(PathBuf::from("/path/to/package.yml")),
///     target_environment: None,
/// };
/// ```
///
/// For cross-environment operations:
/// ```rust,ignore
/// let context = OperationContext {
///     package_path: None,
///     target_environment: Some("production".to_string()),
/// };
/// ```
#[derive(Debug, Clone, Default)]
pub struct OperationContext {
    /// Package file path (used by validate, create operations)
    pub package_path: Option<std::path::PathBuf>,
    /// Target environment for cross-environment operations
    pub target_environment: Option<String>,
}

/// Result of an operation
///
/// Example usage for clients handling typed success results
///
/// ```rust
/// use selfie::package::event::{OperationResult, OperationSuccess};
///
/// fn handle_operation_result(result: OperationResult) {
///     match result {
///         OperationResult::Success(success) => {
///             match success {
///                 OperationSuccess::PackageInstalled {
///                     package_name,
///                     was_already_installed: true,
///                     executable_path: Some(path),
///                     ..
///                 } => {
///                     println!("✅ {} was already installed at {}", package_name, path);
///                 }
///                 OperationSuccess::PackageInstalled {
///                     package_name,
///                     was_already_installed: false,
///                     steps_completed,
///                     ..
///                 } => {
///                     println!("✅ {} installed successfully ({}/{} steps)",
///                             package_name, steps_completed.completed, steps_completed.total);
///                 }
///                 OperationSuccess::PackageValidated {
///                     package_name,
///                     warning_count: Some(warnings),
///                     ..
///                 } => {
///                     println!("✅ {} validated with {} warnings", package_name, warnings);
///                 }
///                 OperationSuccess::PackageListGenerated {
///                     valid_count,
///                     invalid_count,
///                     environment,
///                     ..
///                 } => {
///                     if invalid_count > 0 {
///                         println!("📦 Found {} valid and {} invalid packages in {}",
///                                 valid_count, invalid_count, environment);
///                     } else {
///                         println!("📦 Found {} packages in {}", valid_count, environment);
///                     }
///                 }
///                 _ => println!("✅ Operation completed successfully"),
///             }
///         }
///         OperationResult::Failure(failure) => {
///             eprintln!("❌ Operation failed: {}", failure);
///         }
///     }
/// }
/// ```
///
#[derive(Debug, Clone)]
pub enum OperationResult {
    Success(OperationSuccess),
    Failure(OperationFailure),
}

/// Typed success information for operations
#[derive(Debug, Clone)]
pub enum OperationSuccess {
    /// Package check operation completed
    PackageChecked {
        package_name: String,
        environment: String,
        check_result: CheckResult,
        steps_completed: StepCount,
    },
    /// Package installation operation completed
    PackageInstalled {
        package_name: String,
        environment: String,
        was_already_installed: bool,
        executable_path: Option<String>,
        steps_completed: StepCount,
    },
    /// Package validation operation completed
    PackageValidated {
        package_name: String,
        environment: String,
        status: ValidationStatus,
        warning_count: Option<usize>,
        steps_completed: StepCount,
    },
    /// Package info retrieval operation completed
    PackageInfoRetrieved {
        package_name: String,
        environment: String,
        steps_completed: StepCount,
    },
    /// Package list generation operation completed
    PackageListGenerated {
        valid_count: usize,
        invalid_count: usize,
        environment: String,
        steps_completed: StepCount,
    },
    /// Generic success with just a message (for backward compatibility)
    Generic(String),
}

/// Typed failure information for operations
#[derive(Debug, Clone)]
pub enum OperationFailure {
    /// Environment configuration issues
    EnvironmentError(EnvironmentFailure),
    /// Package loading/parsing issues
    PackageError(PackageFailure),
    /// Command execution issues
    CommandError(CommandFailure),
    /// Generic failures with just a message (for backward compatibility)
    Generic(String),
}

/// Environment-related failure details
#[derive(Debug, Clone)]
pub enum EnvironmentFailure {
    NotFound {
        package_name: String,
        environment: String,
        available_environments: Vec<String>,
        package_file: std::path::PathBuf,
    },
    NoCheckCommand {
        package_name: String,
        environment: String,
        package_file: std::path::PathBuf,
        other_envs_with_check: Vec<String>,
    },
    NoInstallCommand {
        package_name: String,
        environment: String,
        package_file: std::path::PathBuf,
        other_envs_with_install: Vec<String>,
    },
}

/// Package-related failure details
#[derive(Debug, Clone)]
pub enum PackageFailure {
    NotFound {
        name: String,
        packages_path: std::path::PathBuf,
        files_examined: usize,
        search_patterns: Vec<String>,
    },
    ParseError {
        name: String,
        packages_path: std::path::PathBuf,
        failed_file: std::path::PathBuf,
        file_size_bytes: u64,
        source_error: String,
    },
    MultiplePackagesFound {
        name: String,
        packages_path: std::path::PathBuf,
        conflicting_paths: Vec<std::path::PathBuf>,
        files_examined: usize,
        search_patterns: Vec<String>,
    },
}

/// Command execution failure details
#[derive(Debug, Clone)]
pub enum CommandFailure {
    ExecutionFailed {
        command: String,
        exit_code: Option<i32>,
        stdout: String,
        stderr: String,
    },
    CommandNotFound {
        command: String,
    },
    InvalidCommand {
        command: String,
        reason: String,
    },
}

impl std::fmt::Display for OperationFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OperationFailure::EnvironmentError(env_err) => {
                write!(f, "Environment configuration error: {env_err}")
            }
            OperationFailure::PackageError(pkg_err) => write!(f, "Package error: {pkg_err}"),
            OperationFailure::CommandError(cmd_err) => write!(f, "Command error: {cmd_err}"),
            OperationFailure::Generic(msg) => write!(f, "{msg}"),
        }
    }
}

impl std::fmt::Display for EnvironmentFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EnvironmentFailure::NotFound {
                package_name,
                environment,
                ..
            } => write!(
                f,
                "Environment `{environment}` not found in package `{package_name}`"
            ),
            EnvironmentFailure::NoCheckCommand {
                package_name,
                environment,
                ..
            } => write!(
                f,
                "No check command defined for package `{package_name}` in environment `{environment}`"
            ),
            EnvironmentFailure::NoInstallCommand {
                package_name,
                environment,
                ..
            } => write!(
                f,
                "No install command defined for package `{package_name}` in environment `{environment}`"
            ),
        }
    }
}

impl std::fmt::Display for PackageFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PackageFailure::NotFound { name, .. } => write!(f, "Package `{name}` not found"),
            PackageFailure::ParseError { name, .. } => write!(f, "Parse error in package `{name}`"),
            PackageFailure::MultiplePackagesFound { name, .. } => {
                write!(f, "Multiple packages found with name `{name}`")
            }
        }
    }
}

impl std::fmt::Display for CommandFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CommandFailure::ExecutionFailed {
                command, exit_code, ..
            } => {
                if let Some(code) = exit_code {
                    write!(f, "Command `{command}` failed with exit code {code}")
                } else {
                    write!(f, "Command `{command}` failed")
                }
            }
            CommandFailure::CommandNotFound { command } => {
                write!(f, "Command `{command}` not found")
            }
            CommandFailure::InvalidCommand { command, reason } => {
                write!(f, "Invalid command `{command}`: {reason}")
            }
        }
    }
}

// Backward compatibility: allow creating OperationFailure from strings
impl From<String> for OperationFailure {
    fn from(msg: String) -> Self {
        OperationFailure::Generic(msg)
    }
}

impl From<&str> for OperationFailure {
    fn from(msg: &str) -> Self {
        OperationFailure::Generic(msg.to_string())
    }
}

impl std::fmt::Display for OperationSuccess {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OperationSuccess::PackageChecked {
                package_name,
                check_result,
                steps_completed,
                ..
            } => write!(
                f,
                "Package '{package_name}' check completed {check_result} {steps_completed}"
            ),
            OperationSuccess::PackageInstalled {
                package_name,
                was_already_installed,
                executable_path,
                steps_completed,
                ..
            } => {
                let status = if *was_already_installed {
                    match executable_path {
                        Some(path) => format!("was already installed at: {path}"),
                        None => "was already installed".to_string(),
                    }
                } else {
                    "installation completed successfully".to_string()
                };
                write!(f, "Package '{package_name}' {status} {steps_completed}")
            }
            OperationSuccess::PackageValidated {
                package_name,
                status,
                warning_count,
                steps_completed,
                ..
            } => {
                let status_msg = match status {
                    ValidationStatus::HasWarnings => {
                        format!("with {} warning(s)", warning_count.unwrap_or(0))
                    }
                    _ => status.to_string(),
                };
                write!(
                    f,
                    "Package '{package_name}' validation completed {status_msg} {steps_completed}"
                )
            }
            OperationSuccess::PackageInfoRetrieved {
                package_name,
                steps_completed,
                ..
            } => write!(
                f,
                "Package '{package_name}' information retrieved successfully {steps_completed}"
            ),
            OperationSuccess::PackageListGenerated {
                valid_count,
                invalid_count,
                steps_completed,
                ..
            } => {
                let status = if *invalid_count > 0 {
                    format!(
                        "with {valid_count} valid package(s) and {invalid_count} invalid package(s)"
                    )
                } else {
                    format!("with {valid_count} valid package(s)")
                };
                write!(f, "Package listing completed {status} {steps_completed}")
            }
            OperationSuccess::Generic(msg) => write!(f, "{msg}"),
        }
    }
}

// Backward compatibility: allow creating OperationSuccess from strings
impl From<String> for OperationSuccess {
    fn from(msg: String) -> Self {
        OperationSuccess::Generic(msg)
    }
}

impl From<&str> for OperationSuccess {
    fn from(msg: &str) -> Self {
        OperationSuccess::Generic(msg.to_string())
    }
}

// Conversions from existing error types to typed failures
impl From<crate::package::port::PackageError> for OperationFailure {
    fn from(err: crate::package::port::PackageError) -> Self {
        match err {
            crate::package::port::PackageError::EnvironmentNotFound {
                package_name,
                environment,
                available_environments,
                package_file,
            } => OperationFailure::EnvironmentError(EnvironmentFailure::NotFound {
                package_name,
                environment,
                available_environments,
                package_file,
            }),
            crate::package::port::PackageError::NoCheckCommand {
                package_name,
                environment,
                package_file,
                other_envs_with_check,
            } => OperationFailure::EnvironmentError(EnvironmentFailure::NoCheckCommand {
                package_name,
                environment,
                package_file,
                other_envs_with_check,
            }),
            crate::package::port::PackageError::NoInstallCommand {
                package_name,
                environment,
                package_file,
                other_envs_with_install,
            } => OperationFailure::EnvironmentError(EnvironmentFailure::NoInstallCommand {
                package_name,
                environment,
                package_file,
                other_envs_with_install,
            }),
            crate::package::port::PackageError::PackageNotFound {
                name,
                packages_path,
                files_examined,
                search_patterns,
            } => OperationFailure::PackageError(PackageFailure::NotFound {
                name,
                packages_path,
                files_examined,
                search_patterns,
            }),
            crate::package::port::PackageError::ParseError {
                name,
                packages_path,
                failed_file,
                file_size_bytes,
                source,
            } => OperationFailure::PackageError(PackageFailure::ParseError {
                name,
                packages_path,
                failed_file,
                file_size_bytes,
                source_error: source.to_string(),
            }),
            crate::package::port::PackageError::MultiplePackagesFound {
                name,
                packages_path,
                conflicting_paths,
                files_examined,
                search_patterns,
            } => OperationFailure::PackageError(PackageFailure::MultiplePackagesFound {
                name,
                packages_path,
                conflicting_paths,
                files_examined,
                search_patterns,
            }),
        }
    }
}

impl From<crate::commands::runner::CommandError> for OperationFailure {
    fn from(err: crate::commands::runner::CommandError) -> Self {
        match err {
            crate::commands::runner::CommandError::NonZeroExit {
                command,
                exit_code,
                stdout,
                stderr,
                ..
            } => OperationFailure::CommandError(CommandFailure::ExecutionFailed {
                command,
                exit_code: Some(exit_code),
                stdout,
                stderr,
            }),
            crate::commands::runner::CommandError::IoError { command, .. } => {
                OperationFailure::CommandError(CommandFailure::CommandNotFound { command })
            }
            crate::commands::runner::CommandError::Timeout { command, .. } => {
                OperationFailure::CommandError(CommandFailure::InvalidCommand {
                    command,
                    reason: "Command timed out".to_string(),
                })
            }
            _ => OperationFailure::Generic(err.to_string()),
        }
    }
}

impl OperationSuccess {
    /// Creates a package check success result
    #[must_use]
    pub fn package_checked(
        package_name: String,
        environment: String,
        check_result: CheckResult,
        steps_completed: StepCount,
    ) -> Self {
        OperationSuccess::PackageChecked {
            package_name,
            environment,
            check_result,
            steps_completed,
        }
    }

    /// Create a `PackageInstalled` success variant
    #[must_use]
    pub fn package_installed(
        package_name: String,
        environment: String,
        was_already_installed: bool,
        executable_path: Option<String>,
        steps_completed: StepCount,
    ) -> Self {
        OperationSuccess::PackageInstalled {
            package_name,
            environment,
            was_already_installed,
            executable_path,
            steps_completed,
        }
    }

    /// Create a `PackageValidated` success variant
    #[must_use]
    pub fn package_validated(
        package_name: String,
        environment: String,
        status: ValidationStatus,
        warning_count: Option<usize>,
        steps_completed: StepCount,
    ) -> Self {
        OperationSuccess::PackageValidated {
            package_name,
            environment,
            status,
            warning_count,
            steps_completed,
        }
    }

    /// Create a `PackageInfoRetrieved` success variant
    #[must_use]
    pub fn package_info_retrieved(
        package_name: String,
        environment: String,
        steps_completed: StepCount,
    ) -> Self {
        OperationSuccess::PackageInfoRetrieved {
            package_name,
            environment,
            steps_completed,
        }
    }

    /// Create a `PackageListGenerated` success variant
    #[must_use]
    pub fn package_list_generated(
        valid_count: usize,
        invalid_count: usize,
        environment: String,
        steps_completed: StepCount,
    ) -> Self {
        OperationSuccess::PackageListGenerated {
            valid_count,
            invalid_count,
            environment,
            steps_completed,
        }
    }

    /// Checks if this is a package check success
    #[must_use]
    pub fn is_package_check(&self) -> bool {
        matches!(self, OperationSuccess::PackageChecked { .. })
    }

    /// Checks if this is a package installation success
    #[must_use]
    pub fn is_package_install(&self) -> bool {
        matches!(self, OperationSuccess::PackageInstalled { .. })
    }

    /// Checks if this is a package validation success
    #[must_use]
    pub fn is_package_validation(&self) -> bool {
        matches!(self, OperationSuccess::PackageValidated { .. })
    }

    /// Gets the package name from the success result if available
    #[must_use]
    pub fn package_name(&self) -> Option<&str> {
        match self {
            OperationSuccess::PackageChecked { package_name, .. }
            | OperationSuccess::PackageInstalled { package_name, .. }
            | OperationSuccess::PackageValidated { package_name, .. }
            | OperationSuccess::PackageInfoRetrieved { package_name, .. } => Some(package_name),
            OperationSuccess::PackageListGenerated { .. } | OperationSuccess::Generic(_) => None,
        }
    }

    /// Gets the environment from the success result if available
    #[must_use]
    pub fn environment(&self) -> Option<&str> {
        match self {
            OperationSuccess::PackageChecked { environment, .. }
            | OperationSuccess::PackageInstalled { environment, .. }
            | OperationSuccess::PackageValidated { environment, .. }
            | OperationSuccess::PackageInfoRetrieved { environment, .. }
            | OperationSuccess::PackageListGenerated { environment, .. } => Some(environment),
            OperationSuccess::Generic(_) => None,
        }
    }

    /// Gets the steps completed from the success result
    #[must_use]
    pub fn steps_completed(&self) -> Option<StepCount> {
        match self {
            OperationSuccess::PackageChecked {
                steps_completed, ..
            }
            | OperationSuccess::PackageInstalled {
                steps_completed, ..
            }
            | OperationSuccess::PackageValidated {
                steps_completed, ..
            }
            | OperationSuccess::PackageInfoRetrieved {
                steps_completed, ..
            }
            | OperationSuccess::PackageListGenerated {
                steps_completed, ..
            } => Some(*steps_completed),
            OperationSuccess::Generic(_) => None,
        }
    }
}

impl OperationFailure {
    /// Creates an environment not found error
    #[must_use]
    pub fn environment_not_found(
        package_name: String,
        environment: String,
        available_environments: Vec<String>,
        package_file: std::path::PathBuf,
    ) -> Self {
        OperationFailure::EnvironmentError(EnvironmentFailure::NotFound {
            package_name,
            environment,
            available_environments,
            package_file,
        })
    }

    /// Creates a no check command error
    #[must_use]
    pub fn no_check_command(
        package_name: String,
        environment: String,
        package_file: std::path::PathBuf,
        other_envs_with_check: Vec<String>,
    ) -> Self {
        OperationFailure::EnvironmentError(EnvironmentFailure::NoCheckCommand {
            package_name,
            environment,
            package_file,
            other_envs_with_check,
        })
    }

    /// Creates a no install command error
    #[must_use]
    pub fn no_install_command(
        package_name: String,
        environment: String,
        package_file: std::path::PathBuf,
        other_envs_with_install: Vec<String>,
    ) -> Self {
        OperationFailure::EnvironmentError(EnvironmentFailure::NoInstallCommand {
            package_name,
            environment,
            package_file,
            other_envs_with_install,
        })
    }

    /// Creates a package not found error
    #[must_use]
    pub fn package_not_found(
        name: String,
        packages_path: std::path::PathBuf,
        files_examined: usize,
        search_patterns: Vec<String>,
    ) -> Self {
        OperationFailure::PackageError(PackageFailure::NotFound {
            name,
            packages_path,
            files_examined,
            search_patterns,
        })
    }

    /// Creates a command execution failed error
    #[must_use]
    pub fn command_failed(
        command: String,
        exit_code: Option<i32>,
        stdout: String,
        stderr: String,
    ) -> Self {
        OperationFailure::CommandError(CommandFailure::ExecutionFailed {
            command,
            exit_code,
            stdout,
            stderr,
        })
    }

    /// Checks if this is an environment-related error
    #[must_use]
    pub fn is_environment_error(&self) -> bool {
        matches!(self, OperationFailure::EnvironmentError(_))
    }

    /// Checks if this is a package-related error
    #[must_use]
    pub fn is_package_error(&self) -> bool {
        matches!(self, OperationFailure::PackageError(_))
    }

    /// Checks if this is a command-related error
    #[must_use]
    pub fn is_command_error(&self) -> bool {
        matches!(self, OperationFailure::CommandError(_))
    }

    /// Gets the environment failure details if this is an environment error
    #[must_use]
    pub fn environment_failure(&self) -> Option<&EnvironmentFailure> {
        match self {
            OperationFailure::EnvironmentError(env_err) => Some(env_err),
            _ => None,
        }
    }
}

impl From<crate::package::port::PackageRepoError> for OperationFailure {
    fn from(err: crate::package::port::PackageRepoError) -> Self {
        match err {
            crate::package::port::PackageRepoError::PackageError(pkg_err) => (*pkg_err).into(),
            crate::package::port::PackageRepoError::PackageListError(list_err) => {
                OperationFailure::Generic(list_err.to_string())
            }
            crate::package::port::PackageRepoError::IoError(io_err) => {
                OperationFailure::Generic(format!("IO error: {io_err}"))
            }
            crate::package::port::PackageRepoError::FileSystemError(fs_err) => {
                OperationFailure::Generic(format!("File system error: {fs_err}"))
            }
        }
    }
}

/// Events that can be emitted during package operations
#[derive(Debug, Clone)]
pub enum PackageEvent {
    /// Operation has started
    Started { operation_info: OperationInfo },

    /// Progress update
    Progress {
        operation_info: OperationInfo,
        step: usize,
        total_steps: usize,
        percent_complete: f32,
        message: String,
    },

    /// Operation completed
    Completed {
        operation_info: OperationInfo,
        result: OperationResult,
    },

    /// Operation was canceled
    Canceled {
        operation_info: OperationInfo,
        reason: String,
    },

    /// Trace-level message
    Trace {
        operation_info: OperationInfo,
        message: String,
    },

    /// Debug-level message
    Debug {
        operation_info: OperationInfo,
        message: String,
    },

    /// Informational message with console output
    Info {
        operation_info: OperationInfo,
        output: ConsoleOutput,
    },

    /// Warning message
    Warning {
        operation_info: OperationInfo,
        message: String,
    },

    /// Error occurred but operation continues
    Error {
        operation_info: OperationInfo,
        error: StreamedError,
        message: String,
    },

    /// Package information loaded
    PackageInfoLoaded {
        operation_info: OperationInfo,
        package_info: PackageInfoData,
    },

    /// Environment status checked
    EnvironmentStatusChecked {
        operation_info: OperationInfo,
        environment_status: EnvironmentStatusData,
    },

    /// Package list loaded
    PackageListLoaded {
        operation_info: OperationInfo,
        package_list: PackageListData,
    },

    /// Check result completed
    CheckResultCompleted {
        operation_info: OperationInfo,
        check_result: CheckResultData,
    },

    /// Validation result completed
    ValidationResultCompleted {
        operation_info: OperationInfo,
        validation_result: ValidationResultData,
    },
}

/// Structured data for package information
#[derive(Debug, Clone)]
pub struct PackageInfoData {
    pub name: String,
    pub version: String,
    pub description: Option<String>,
    pub homepage: Option<String>,
    pub environments: Vec<String>,
    pub current_environment: String,
}

/// Structured data for environment status
#[derive(Debug, Clone)]
pub struct EnvironmentStatusData {
    pub environment_name: String,
    pub is_current: bool,
    pub install_command: String,
    pub check_command: Option<String>,
    pub dependencies: Vec<String>,
    pub status: Option<EnvironmentStatus>,
}

/// Status of a package in an environment
#[derive(Debug, Clone)]
pub enum EnvironmentStatus {
    Installed,
    NotInstalled,
    Unknown(String),
}

/// Structured data for package list
#[derive(Debug, Clone)]
pub struct PackageListData {
    pub valid_packages: Vec<PackageListItem>,
    pub invalid_packages: Vec<InvalidPackageInfo>,
    pub current_environment: String,
    pub package_directory: String,
    pub environment_stats: std::collections::HashMap<String, usize>,
}

/// Information about a package in the list
#[derive(Debug, Clone)]
pub struct PackageListItem {
    pub name: String,
    pub version: String,
    pub environments: Vec<String>,
    pub status: Option<CheckResult>,
}

/// Information about an invalid package
#[derive(Debug, Clone)]
pub struct InvalidPackageInfo {
    pub path: String,
    pub error: String,
}

/// Structured data for check results
#[derive(Debug, Clone)]
pub struct CheckResultData {
    pub package_name: String,
    pub environment: String,
    pub check_command: Option<String>,
    pub result: CheckResult,
}

/// Result of a check operation
#[derive(Debug, Clone)]
pub enum CheckResult {
    Success {
        stdout: String,
        stderr: String,
    },
    Failed {
        stdout: String,
        stderr: String,
        exit_code: Option<i32>,
    },
    CommandNotFound,
    NoCheckCommand,
    Error(String),
}

impl std::fmt::Display for CheckResult {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CheckResult::Success { .. } => write!(f, "successfully"),
            CheckResult::Failed { .. } => write!(f, "with failures"),
            CheckResult::CommandNotFound => write!(f, "but command not found"),
            CheckResult::NoCheckCommand => write!(f, "but no check command defined"),
            CheckResult::Error(_) => write!(f, "with errors"),
        }
    }
}

/// Structured data for validation results
#[derive(Debug, Clone)]
pub struct ValidationResultData {
    pub package_name: String,
    pub environment: String,
    pub status: ValidationStatus,
    pub issues: Vec<ValidationIssueData>,
}

/// Overall validation status
#[derive(Debug, Clone)]
pub enum ValidationStatus {
    Valid,
    HasWarnings,
    HasErrors,
}

impl std::fmt::Display for ValidationStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ValidationStatus::Valid => write!(f, "successfully"),
            ValidationStatus::HasWarnings => write!(f, "with warnings"),
            ValidationStatus::HasErrors => write!(f, "with errors"),
        }
    }
}

/// Individual validation issue
#[derive(Debug, Clone)]
pub struct ValidationIssueData {
    pub category: String,
    pub field: String,
    pub message: String,
    pub level: ValidationLevel,
    pub suggestion: Option<String>,
}

/// Validation issue level
#[derive(Debug, Clone)]
pub enum ValidationLevel {
    Error,
    Warning,
}

/// Log levels for the `EventSender` log method
#[derive(Debug, Clone, Copy)]
pub enum LogLevel {
    Trace,
    Debug,
    Warning,
}

#[derive(Debug, Clone)]
pub enum ConsoleOutput {
    Stdout(String),
    Stderr(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_step_count_display() {
        let step_count = StepCount::new(3, 5);
        assert_eq!(format!("{step_count}"), "(3/5 steps)");
    }

    #[test]
    fn test_step_count_usize_usage() {
        // Test direct usize construction
        let step_count = StepCount::new(7usize, 10usize);
        assert_eq!(step_count.completed, 7);
        assert_eq!(step_count.total, 10);
        assert_eq!(format!("{step_count}"), "(7/10 steps)");

        // Test From<(usize, usize)> conversion
        let step_count_from_usize: StepCount = (3usize, 5usize).into();
        assert_eq!(step_count_from_usize.completed, 3);
        assert_eq!(step_count_from_usize.total, 5);

        // Test with large usize values (that would fit in usize but not necessarily u32 on 64-bit)
        let large_step_count = StepCount::new(1_000_000, 2_000_000);
        assert_eq!(large_step_count.completed, 1_000_000);
        assert_eq!(large_step_count.total, 2_000_000);
    }

    #[test]
    fn test_check_result_display() {
        assert_eq!(
            format!(
                "{}",
                CheckResult::Success {
                    stdout: String::new(),
                    stderr: String::new()
                }
            ),
            "successfully"
        );
        assert_eq!(
            format!(
                "{}",
                CheckResult::Failed {
                    stdout: String::new(),
                    stderr: String::new(),
                    exit_code: Some(1)
                }
            ),
            "with failures"
        );
        assert_eq!(
            format!("{}", CheckResult::CommandNotFound),
            "but command not found"
        );
        assert_eq!(
            format!("{}", CheckResult::NoCheckCommand),
            "but no check command defined"
        );
        assert_eq!(
            format!("{}", CheckResult::Error("test".to_string())),
            "with errors"
        );
    }

    #[test]
    fn test_validation_status_display() {
        assert_eq!(format!("{}", ValidationStatus::Valid), "successfully");
        assert_eq!(
            format!("{}", ValidationStatus::HasWarnings),
            "with warnings"
        );
        assert_eq!(format!("{}", ValidationStatus::HasErrors), "with errors");
    }

    #[test]
    fn test_operation_success_display() {
        let step_count = StepCount::new(2, 3);

        let success = OperationSuccess::PackageChecked {
            package_name: "test-package".to_string(),
            environment: "test".to_string(),
            check_result: CheckResult::Success {
                stdout: String::new(),
                stderr: String::new(),
            },
            steps_completed: step_count,
        };

        assert_eq!(
            format!("{success}"),
            "Package 'test-package' check completed successfully (2/3 steps)"
        );
    }

    #[test]
    fn test_operation_success_package_installed() {
        let step_count = StepCount::new(1, 1);

        let success = OperationSuccess::PackageInstalled {
            package_name: "test-package".to_string(),
            environment: "test".to_string(),
            was_already_installed: false,
            executable_path: None,
            steps_completed: step_count,
        };

        assert_eq!(
            format!("{success}"),
            "Package 'test-package' installation completed successfully (1/1 steps)"
        );
    }
}
