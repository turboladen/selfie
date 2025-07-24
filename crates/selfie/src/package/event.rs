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
        step: u32,
        total_steps: u32,
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
#[derive(Debug, Clone)]
pub enum OperationResult {
    Success(String),
    Failure(String),
}

/// Events that can be emitted during package operations
#[derive(Debug, Clone)]
pub enum PackageEvent {
    /// Operation has started
    Started { operation_info: OperationInfo },

    /// Progress update
    Progress {
        operation_info: OperationInfo,
        step: u32,
        total_steps: u32,
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
    Success,
    Failed {
        stdout: String,
        stderr: String,
        exit_code: Option<i32>,
    },
    CommandNotFound,
    NoCheckCommand,
    Error(String),
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
