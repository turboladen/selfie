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

pub use self::error::StreamedError;
pub use self::metadata::OperationType;

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

/// Type alias for a stream of package events
///
/// This stream emits [`PackageEvent`] items as operations progress, allowing
/// consumers to react to operation updates in real-time. The stream is pinned
/// and boxed to enable dynamic dispatch and async iteration.
pub type EventStream = Pin<Box<dyn Stream<Item = PackageEvent> + Send>>;

/// Create an event stream from an async closure.
///
/// Spawns a tokio task that runs the closure with a channel sender, and returns
/// the receiving end as a pinned stream. This is the standard pattern for
/// creating event streams across all services (`PackageService`, `DotfileService`,
/// `SyncService`).
pub fn create_event_stream<F, Fut>(f: F) -> EventStream
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
    ///
    /// Currently unused — terminal errors flow through `OperationResult::Failure`
    /// via `send_completed`. Retained for future non-terminal error events.
    #[allow(dead_code)]
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

    /// Send a cancellation event
    pub(crate) async fn send_canceled(&self, reason: impl fmt::Display) {
        let operation_info = self.touch_operation_info();
        self.send(PackageEvent::Canceled {
            operation_info,
            reason: reason.to_string(),
        })
        .await;
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

    /// Send sorted filtered package list ready for display
    pub(crate) async fn send_package_list_ready(&self, packages: Vec<PackageListItem>) {
        let operation_info = self.touch_operation_info();
        self.send(PackageEvent::PackageListReady {
            operation_info,
            packages,
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

    /// Send audit result data
    pub(crate) async fn send_audit_result(&self, audit_result: AuditResultData) {
        let operation_info = self.touch_operation_info();
        self.send(PackageEvent::AuditResultCompleted {
            operation_info,
            audit_result,
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

    /// Send individual package list item data (for streaming)
    pub(crate) async fn send_package_list_item(&self, package_item: PackageListItem) {
        let operation_info = self.touch_operation_info();
        self.send(PackageEvent::PackageListItemCompleted {
            operation_info,
            package_item,
        })
        .await;
    }

    /// Send a recommend-started event
    pub(crate) async fn send_recommend_started(&self, recommend_name: impl fmt::Display) {
        let operation_info = self.touch_operation_info();
        self.send(PackageEvent::RecommendStarted {
            operation_info,
            recommend_name: recommend_name.to_string(),
        })
        .await;
    }

    /// Send a recommend-succeeded event
    pub(crate) async fn send_recommend_succeeded(&self, recommend_name: impl fmt::Display) {
        let operation_info = self.touch_operation_info();
        self.send(PackageEvent::RecommendSucceeded {
            operation_info,
            recommend_name: recommend_name.to_string(),
        })
        .await;
    }

    /// Send a recommend-failed event
    pub(crate) async fn send_recommend_failed(
        &self,
        recommend_name: impl fmt::Display,
        error: impl fmt::Display,
    ) {
        let operation_info = self.touch_operation_info();
        self.send(PackageEvent::RecommendFailed {
            operation_info,
            recommend_name: recommend_name.to_string(),
            error: error.to_string(),
        })
        .await;
    }

    /// Send individual spec list item data (for streaming)
    pub(crate) async fn send_spec_list_item(&self, spec_item: SpecListItem) {
        let operation_info = self.touch_operation_info();
        self.send(PackageEvent::SpecListItemCompleted {
            operation_info,
            spec_item,
        })
        .await;
    }

    /// Send spec list summary data
    pub(crate) async fn send_spec_list(&self, spec_list: SpecListData) {
        let operation_info = self.touch_operation_info();
        self.send(PackageEvent::SpecListLoaded {
            operation_info,
            spec_list,
        })
        .await;
    }

    /// Send removal dependency info event
    pub(crate) async fn send_removal_dependency_info(
        &self,
        package_name: String,
        dependent_packages: Vec<String>,
    ) {
        let operation_info = self.touch_operation_info();
        self.send(PackageEvent::RemovalDependencyInfo {
            operation_info,
            package_name,
            dependent_packages,
        })
        .await;
    }

    /// Send dotfile cleanup info event (during package removal)
    pub(crate) async fn send_dotfile_cleanup_info(
        &self,
        package_name: String,
        dotfile_targets: Vec<String>,
    ) {
        let operation_info = self.touch_operation_info();
        self.send(PackageEvent::DotfileCleanupInfo {
            operation_info,
            package_name,
            dotfile_targets,
        })
        .await;
    }

    /// Send a dotfile-deploying event
    pub(crate) async fn send_dotfile_deploying(
        &self,
        source: impl fmt::Display,
        target: impl fmt::Display,
    ) {
        let operation_info = self.touch_operation_info();
        self.send(PackageEvent::DotfileDeploying {
            operation_info,
            source: source.to_string(),
            target: target.to_string(),
        })
        .await;
    }

    /// Send a dotfile-deployed event
    pub(crate) async fn send_dotfile_deployed(
        &self,
        source: impl fmt::Display,
        target: impl fmt::Display,
    ) {
        let operation_info = self.touch_operation_info();
        self.send(PackageEvent::DotfileDeployed {
            operation_info,
            source: source.to_string(),
            target: target.to_string(),
        })
        .await;
    }

    /// Send a dotfile-skipped event
    pub(crate) async fn send_dotfile_skipped(
        &self,
        source: impl fmt::Display,
        target: impl fmt::Display,
        reason: impl fmt::Display,
    ) {
        let operation_info = self.touch_operation_info();
        self.send(PackageEvent::DotfileSkipped {
            operation_info,
            source: source.to_string(),
            target: target.to_string(),
            reason: reason.to_string(),
        })
        .await;
    }

    /// Send a dotfile-conflict event
    pub(crate) async fn send_dotfile_conflict(
        &self,
        source: impl fmt::Display,
        target: impl fmt::Display,
        diff: impl fmt::Display,
    ) {
        let operation_info = self.touch_operation_info();
        self.send(PackageEvent::DotfileConflict {
            operation_info,
            source: source.to_string(),
            target: target.to_string(),
            diff: diff.to_string(),
        })
        .await;
    }

    /// Send a dotfile-drift-detected event
    pub(crate) async fn send_dotfile_drift_detected(
        &self,
        target: impl fmt::Display,
        drift_type: impl fmt::Display,
    ) {
        let operation_info = self.touch_operation_info();
        self.send(PackageEvent::DotfileDriftDetected {
            operation_info,
            target: target.to_string(),
            drift_type: drift_type.to_string(),
        })
        .await;
    }

    /// Send a post-install note event
    pub(crate) async fn send_post_install_note(
        &self,
        package_name: impl fmt::Display,
        note: impl fmt::Display,
    ) {
        let operation_info = self.touch_operation_info();
        self.send(PackageEvent::PostInstallNote {
            operation_info,
            package_name: package_name.to_string(),
            note: note.to_string(),
        })
        .await;
    }

    /// Get a snapshot of the current operation info with a fresh timestamp.
    ///
    /// Used when constructing custom event variants (e.g., `SyncRepoStatus`)
    /// that carry their own `OperationInfo` field.
    pub(crate) fn operation_info(&self) -> OperationInfo {
        self.touch_operation_info()
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
    /// Package audit operation completed
    PackageAudited {
        package_name: String,
        environment: String,
        audit_result: AuditResult,
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
    /// Spec info retrieval operation completed (definition only, no runtime check)
    SpecInfoRetrieved {
        package_name: String,
        environment: String,
        steps_completed: StepCount,
    },
    /// Package status check operation completed (install status only)
    PackageStatusChecked {
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
    /// Package creation operation completed
    PackageCreated {
        package_name: String,
        file_path: std::path::PathBuf,
        environment: String,
        steps_completed: StepCount,
    },
    /// Package update operation completed
    PackageUpdated {
        package_name: String,
        environment: String,
        steps_completed: StepCount,
    },
    /// Package removal operation completed
    PackageRemoved {
        package_name: String,
        file_path: std::path::PathBuf,
        environment: String,
        dependent_packages: Vec<String>,
        steps_completed: StepCount,
    },
    /// Spec list generation operation completed
    SpecListGenerated {
        valid_count: usize,
        invalid_count: usize,
        environment: String,
        steps_completed: StepCount,
    },
    /// Bulk spec validation operation completed
    SpecsValidated {
        validated_count: usize,
        error_count: usize,
        warning_count: usize,
        environment: String,
        steps_completed: StepCount,
    },
    /// Dotfile apply operation completed
    DotfilesApplied {
        deployed_count: usize,
        /// Entries there was correctly nothing to do for: already in sync, or a
        /// dry run declining to act.
        ///
        /// Distinct from `refused_count`, and the distinction is the point. A
        /// refused entry counted here is indistinguishable from one that needed
        /// no work, which is how `selfie apply` came to report success for a run
        /// that deployed nothing.
        skipped_count: usize,
        conflict_count: usize,
        /// What selfie was asked to deploy and did not — refusals and failures
        /// alike.
        ///
        /// Usually an entry, but **not always one**: a package refused whole for
        /// a top-level key that hides a real field contributes 1 here and no
        /// entries at all, because its `dotfiles` list was swallowed by the very
        /// key being refused. So this counts *outcomes*, matching
        /// `steps_completed`, and does not equal a number of dotfile entries.
        ///
        /// Non-zero makes [`had_refusals`](Self::had_refusals) true, which is
        /// what every adapter reads to decide that the run did not do what was
        /// asked. Named for the common case: most of what lands here was
        /// *declined* by selfie rather than failing, and `perform_deploy` is
        /// explicit that a refusal is not a failure.
        refused_count: usize,
        environment: String,
        steps_completed: StepCount,
    },
    /// Dotfile drift check operation completed
    DotfileDriftChecked {
        drift_count: usize,
        total_count: usize,
        environment: String,
        steps_completed: StepCount,
    },
    /// Dotfile tracking operation completed
    DotfileTracked {
        /// Name of the spec (package or standalone dotfile)
        name: String,
        /// Where the file was copied to in the repo
        source_path: std::path::PathBuf,
        /// The original target path being tracked
        target_path: String,
        /// Whether the file was already tracked (no-op)
        was_already_tracked: bool,
        environment: String,
        steps_completed: StepCount,
    },
    /// Sync push completed — all commits created and pushed to remote
    SyncPushComplete {
        commits_pushed: usize,
        steps_completed: StepCount,
    },
    /// Sync pull completed — new commits pulled from remote
    SyncPullComplete {
        commits_pulled: usize,
        packages_updated: Vec<String>,
        packages_added: Vec<String>,
        packages_removed: Vec<String>,
        steps_completed: StepCount,
    },
    /// Sync pull found no new changes
    SyncPullUpToDate { steps_completed: StepCount },
    /// Sync push found no changes to commit
    SyncNothingToPush { steps_completed: StepCount },
    /// Generic success with a freeform message
    Generic(String),
}

/// Typed failure information for operations
#[derive(Debug, Clone)]
pub enum OperationFailure {
    /// Package-related issues (environment, loading, parsing)
    Package(crate::package::port::PackageError),
    /// Command execution issues
    CommandError(CommandFailure),
    /// Dependency resolution issues
    DependencyError(DependencyFailure),
    /// Package listing/directory issues
    PackageList(crate::package::port::PackageListError),
    /// Generic failure with a freeform message
    Generic(String),
}

/// Command execution failure details
#[derive(Debug, Clone)]
pub enum CommandFailure {
    /// A command ran and exited non-zero.
    ///
    /// Deliberately carries **no** `stdout`. selfie runs user-defined commands and
    /// cannot know which of them print a credential, so a general failure value has
    /// nowhere safe to put a command's whole output: this variant is cloned into
    /// [`PackageEvent::Completed`], which every adapter receives. `stderr` is
    /// forwarded because a failure has to stay diagnosable, and its
    /// [`BoundedText`](crate::commands::BoundedText) type is what bounds it: the
    /// newtype's field is private, so no struct-variant literal — here, in an
    /// adapter, or in a test — can put unbounded text in this field. That makes
    /// this the one stderr-forwarding site the compiler enforces rather than the
    /// convention it used to be. Do not add a `stdout` field back.
    ExecutionFailed {
        command: String,
        exit_code: Option<i32>,
        stderr: crate::commands::BoundedText,
    },
    CommandNotFound {
        command: String,
    },
    InvalidCommand {
        command: String,
        reason: String,
    },
}

/// Dependency resolution failure details
#[derive(Debug, Clone)]
pub enum DependencyFailure {
    /// A circular dependency was detected in the dependency graph
    CircularDependency {
        package_name: String,
        cycle: Vec<String>,
    },
    /// A required dependency was not found in the repository
    MissingDependency {
        package_name: String,
        dependency_name: String,
    },
}

impl std::fmt::Display for OperationFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OperationFailure::Package(e) => write!(f, "{e}"),
            OperationFailure::CommandError(cmd_err) => write!(f, "Command error: {cmd_err}"),
            OperationFailure::DependencyError(dep_err) => {
                write!(f, "Dependency error: {dep_err}")
            }
            OperationFailure::PackageList(list_err) => write!(f, "{list_err}"),
            OperationFailure::Generic(msg) => write!(f, "{msg}"),
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

impl std::fmt::Display for DependencyFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DependencyFailure::CircularDependency {
                package_name,
                cycle,
            } => write!(
                f,
                "Circular dependency detected for package `{package_name}`: {}",
                cycle.join(" -> ")
            ),
            DependencyFailure::MissingDependency {
                package_name,
                dependency_name,
            } => write!(
                f,
                "Package `{package_name}` depends on `{dependency_name}`, which was not found"
            ),
        }
    }
}

// Convenience: allow creating OperationFailure from strings
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
            OperationSuccess::PackageAudited {
                package_name,
                audit_result,
                steps_completed,
                ..
            } => write!(
                f,
                "Package '{package_name}' audit completed {audit_result} {steps_completed}"
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
            OperationSuccess::SpecInfoRetrieved {
                package_name,
                steps_completed,
                ..
            } => write!(
                f,
                "Package '{package_name}' spec info retrieved successfully {steps_completed}"
            ),
            OperationSuccess::PackageStatusChecked {
                package_name,
                steps_completed,
                ..
            } => write!(
                f,
                "Package '{package_name}' status checked successfully {steps_completed}"
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
            OperationSuccess::PackageCreated {
                package_name,
                file_path,
                steps_completed,
                ..
            } => write!(
                f,
                "Package '{package_name}' created at {} {steps_completed}",
                file_path.display()
            ),
            OperationSuccess::PackageUpdated {
                package_name,
                steps_completed,
                ..
            } => write!(
                f,
                "Package '{package_name}' updated successfully {steps_completed}"
            ),
            OperationSuccess::PackageRemoved {
                package_name,
                file_path,
                dependent_packages,
                steps_completed,
                ..
            } => {
                if dependent_packages.is_empty() {
                    write!(
                        f,
                        "Package '{package_name}' removed from {} {steps_completed}",
                        file_path.display()
                    )
                } else {
                    write!(
                        f,
                        "Package '{package_name}' removed from {} (had {} dependent package(s)) {steps_completed}",
                        file_path.display(),
                        dependent_packages.len()
                    )
                }
            }
            OperationSuccess::SpecListGenerated {
                valid_count,
                invalid_count,
                steps_completed,
                ..
            } => {
                let status = if *invalid_count > 0 {
                    format!("with {valid_count} valid spec(s) and {invalid_count} invalid spec(s)")
                } else {
                    format!("with {valid_count} valid spec(s)")
                };
                write!(f, "Spec listing completed {status} {steps_completed}")
            }
            OperationSuccess::SpecsValidated {
                validated_count,
                error_count,
                warning_count,
                steps_completed,
                ..
            } => {
                let status = if *error_count > 0 {
                    format!(
                        "{validated_count} package(s) validated, {error_count} with errors, {warning_count} with warnings"
                    )
                } else if *warning_count > 0 {
                    format!("{validated_count} package(s) validated, {warning_count} with warnings")
                } else {
                    format!("{validated_count} package(s) validated successfully")
                };
                write!(f, "Spec validation completed: {status} {steps_completed}")
            }
            OperationSuccess::DotfilesApplied {
                deployed_count,
                skipped_count,
                conflict_count,
                refused_count,
                steps_completed,
                ..
            } => {
                write!(
                    f,
                    "Dotfiles applied: {deployed_count} deployed, {skipped_count} skipped, {conflict_count} conflict(s), {refused_count} refused {steps_completed}"
                )
            }
            OperationSuccess::DotfileDriftChecked {
                drift_count,
                total_count,
                steps_completed,
                ..
            } => {
                write!(
                    f,
                    "Dotfile drift check: {drift_count} drifted out of {total_count} {steps_completed}"
                )
            }
            OperationSuccess::DotfileTracked {
                name,
                target_path,
                was_already_tracked: true,
                steps_completed,
                ..
            } => {
                write!(
                    f,
                    "Already tracking '{target_path}' in spec '{name}' {steps_completed}"
                )
            }
            OperationSuccess::DotfileTracked {
                name,
                target_path,
                steps_completed,
                ..
            } => {
                write!(
                    f,
                    "Now tracking '{target_path}' in spec '{name}' {steps_completed}"
                )
            }
            OperationSuccess::SyncPushComplete {
                commits_pushed,
                steps_completed,
            } => {
                let label = crate::pluralize(*commits_pushed, "commit", "commits");
                write!(
                    f,
                    "Pushed {commits_pushed} {label} to remote {steps_completed}"
                )
            }
            OperationSuccess::SyncPullComplete {
                commits_pulled,
                packages_updated,
                packages_added,
                packages_removed,
                steps_completed,
            } => {
                let label = crate::pluralize(*commits_pulled, "commit", "commits");
                let mut parts = Vec::new();
                if !packages_updated.is_empty() {
                    parts.push(format!("updated: {}", packages_updated.join(", ")));
                }
                if !packages_added.is_empty() {
                    parts.push(format!("added: {}", packages_added.join(", ")));
                }
                if !packages_removed.is_empty() {
                    parts.push(format!("removed: {}", packages_removed.join(", ")));
                }
                if parts.is_empty() {
                    write!(
                        f,
                        "Pulled {commits_pulled} {label} from remote {steps_completed}"
                    )
                } else {
                    write!(
                        f,
                        "Pulled {commits_pulled} {label} from remote ({}) {steps_completed}",
                        parts.join("; ")
                    )
                }
            }
            OperationSuccess::SyncPullUpToDate { steps_completed } => {
                write!(f, "Already up to date with remote {steps_completed}")
            }
            OperationSuccess::SyncNothingToPush { steps_completed } => {
                write!(
                    f,
                    "Nothing to push — working tree is clean {steps_completed}"
                )
            }
            OperationSuccess::Generic(msg) => write!(f, "{msg}"),
        }
    }
}

// Convenience: allow creating OperationSuccess from strings
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
        OperationFailure::Package(err)
    }
}

impl From<crate::commands::runner::CommandError> for OperationFailure {
    fn from(err: crate::commands::runner::CommandError) -> Self {
        match err {
            crate::commands::runner::CommandError::IoError { command, .. } => {
                OperationFailure::CommandError(CommandFailure::CommandNotFound { command })
            }
            crate::commands::runner::CommandError::Timeout { command, .. } => {
                OperationFailure::CommandError(CommandFailure::InvalidCommand {
                    command,
                    reason: "Command timed out".to_string(),
                })
            }
            // Listed rather than matched with `_` so that adding a
            // `CommandError` variant fails to build here. This arm renders the
            // error with `Display`, and a variant whose `Display` carried
            // command output would leak it into `PackageEvent::Completed` and
            // on to both the CLI and the MCP server's JSON. Every variant below
            // names the command, and beyond that only text selfie chose itself —
            // never process output. Check that before extending this list.
            //
            // `OutputReadFailed` was checked against that requirement when it was
            // added: alongside the command it renders the stream that failed and
            // an `io::Error`, which is the OS's own message for the failed read.
            // It carries no output bytes — whatever had been buffered when the
            // read failed is dropped rather than reported.
            //
            // It once also had to render a reader *task* that did not finish, and
            // substituted a fixed `&'static str` for the `JoinError` because a
            // panic payload is produced by the task holding the command's bytes
            // and can be derived from them. Those tasks are gone: `collect` in
            // `commands::shell` reads both pipes inline, so no `JoinError` is
            // reachable from this variant. See that function on why a `spawn`
            // must not be reintroduced there without restoring the guard.
            //
            // `ContentMarkersAbsent` was checked the same way: it renders the
            // command and nothing else. It cannot render what was captured,
            // because the whole reason it exists is that selfie could not tell
            // which part of that capture the command wrote.
            crate::commands::runner::CommandError::Cancelled { .. }
            | crate::commands::runner::CommandError::OutputReadFailed { .. }
            | crate::commands::runner::CommandError::ContentMarkersAbsent { .. }
            | crate::commands::runner::CommandError::StdoutSpawn(_)
            | crate::commands::runner::CommandError::StderrSpawn(_) => {
                OperationFailure::Generic(err.to_string())
            }
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

    /// Creates a package audit success result
    #[must_use]
    pub fn package_audited(
        package_name: String,
        environment: String,
        audit_result: AuditResult,
        steps_completed: StepCount,
    ) -> Self {
        OperationSuccess::PackageAudited {
            package_name,
            environment,
            audit_result,
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

    /// Create a `SpecInfoRetrieved` success variant
    #[must_use]
    pub fn spec_info_retrieved(
        package_name: String,
        environment: String,
        steps_completed: StepCount,
    ) -> Self {
        OperationSuccess::SpecInfoRetrieved {
            package_name,
            environment,
            steps_completed,
        }
    }

    /// Create a `PackageStatusChecked` success variant
    #[must_use]
    pub fn package_status_checked(
        package_name: String,
        environment: String,
        steps_completed: StepCount,
    ) -> Self {
        OperationSuccess::PackageStatusChecked {
            package_name,
            environment,
            steps_completed,
        }
    }

    /// Create a `SpecsValidated` success variant
    #[must_use]
    pub fn specs_validated(
        validated_count: usize,
        error_count: usize,
        warning_count: usize,
        environment: String,
        steps_completed: StepCount,
    ) -> Self {
        OperationSuccess::SpecsValidated {
            validated_count,
            error_count,
            warning_count,
            environment,
            steps_completed,
        }
    }

    /// Create a `SpecListGenerated` success variant
    #[must_use]
    pub fn spec_list_generated(
        valid_count: usize,
        invalid_count: usize,
        environment: String,
        steps_completed: StepCount,
    ) -> Self {
        OperationSuccess::SpecListGenerated {
            valid_count,
            invalid_count,
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

    /// Create a `PackageCreated` success variant
    #[must_use]
    pub fn package_created(
        package_name: String,
        file_path: std::path::PathBuf,
        environment: String,
        steps_completed: StepCount,
    ) -> Self {
        OperationSuccess::PackageCreated {
            package_name,
            file_path,
            environment,
            steps_completed,
        }
    }

    /// Create a `PackageUpdated` success variant
    #[must_use]
    pub fn package_updated(
        package_name: String,
        environment: String,
        steps_completed: StepCount,
    ) -> Self {
        OperationSuccess::PackageUpdated {
            package_name,
            environment,
            steps_completed,
        }
    }

    /// Create a `PackageRemoved` success variant
    #[must_use]
    pub fn package_removed(
        package_name: String,
        file_path: std::path::PathBuf,
        environment: String,
        dependent_packages: Vec<String>,
        steps_completed: StepCount,
    ) -> Self {
        OperationSuccess::PackageRemoved {
            package_name,
            file_path,
            environment,
            dependent_packages,
            steps_completed,
        }
    }

    /// Checks if this is a package check success
    #[must_use]
    pub fn is_package_check(&self) -> bool {
        matches!(self, OperationSuccess::PackageChecked { .. })
    }

    /// Checks if this is a package audit success
    #[must_use]
    pub fn is_package_audit(&self) -> bool {
        matches!(self, OperationSuccess::PackageAudited { .. })
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

    /// Checks if this is a package update success
    #[must_use]
    pub fn is_package_update(&self) -> bool {
        matches!(self, OperationSuccess::PackageUpdated { .. })
    }

    /// Checks if this is a package remove success
    #[must_use]
    pub fn is_package_remove(&self) -> bool {
        matches!(self, OperationSuccess::PackageRemoved { .. })
    }

    /// Gets the package name from the success result if available
    #[must_use]
    pub fn package_name(&self) -> Option<&str> {
        match self {
            OperationSuccess::PackageChecked { package_name, .. }
            | OperationSuccess::PackageAudited { package_name, .. }
            | OperationSuccess::PackageInstalled { package_name, .. }
            | OperationSuccess::PackageValidated { package_name, .. }
            | OperationSuccess::SpecInfoRetrieved { package_name, .. }
            | OperationSuccess::PackageStatusChecked { package_name, .. }
            | OperationSuccess::PackageCreated { package_name, .. }
            | OperationSuccess::PackageUpdated { package_name, .. }
            | OperationSuccess::PackageRemoved { package_name, .. } => Some(package_name),
            OperationSuccess::DotfileTracked { name, .. } => Some(name),
            OperationSuccess::PackageListGenerated { .. }
            | OperationSuccess::SpecListGenerated { .. }
            | OperationSuccess::SpecsValidated { .. }
            | OperationSuccess::DotfilesApplied { .. }
            | OperationSuccess::DotfileDriftChecked { .. }
            | OperationSuccess::SyncPushComplete { .. }
            | OperationSuccess::SyncPullComplete { .. }
            | OperationSuccess::SyncPullUpToDate { .. }
            | OperationSuccess::SyncNothingToPush { .. }
            | OperationSuccess::Generic(_) => None,
        }
    }

    /// Whether this success also refused to do something it was asked to do.
    ///
    /// The one place that question is answered, so the CLI's exit code and the
    /// MCP server's result envelope cannot disagree about it. An operation that
    /// completed while declining part of its work is still a completed
    /// operation — which is why this is a property of a success rather than a
    /// failure — but a caller checking only the exit code has to be told
    ///
    /// False for every other variant: no other operation counts refusals yet.
    /// Add a variant here when one does, rather than teaching an adapter to
    /// look for it.
    #[must_use]
    pub fn had_refusals(&self) -> bool {
        self.refused_count().is_some_and(|count| count > 0)
    }

    /// How many refusals this success carries, for operations that count them.
    ///
    /// `None` where the question does not apply, which is every variant but
    /// [`DotfilesApplied`](Self::DotfilesApplied) — distinct from `Some(0)`, a
    /// run that could have refused something and did not.
    ///
    /// Exists so an adapter can report the number rather than the fact. The MCP
    /// server puts it in its own JSON field: an assistant told only that
    /// something was refused has to parse the count out of a prose message,
    /// which is the failure mode structured output exists to avoid.
    #[must_use]
    pub fn refused_count(&self) -> Option<usize> {
        match self {
            OperationSuccess::DotfilesApplied { refused_count, .. } => Some(*refused_count),
            _ => None,
        }
    }

    /// Gets the environment from the success result if available
    #[must_use]
    pub fn environment(&self) -> Option<&str> {
        match self {
            OperationSuccess::PackageChecked { environment, .. }
            | OperationSuccess::PackageAudited { environment, .. }
            | OperationSuccess::PackageInstalled { environment, .. }
            | OperationSuccess::PackageValidated { environment, .. }
            | OperationSuccess::SpecInfoRetrieved { environment, .. }
            | OperationSuccess::PackageStatusChecked { environment, .. }
            | OperationSuccess::PackageListGenerated { environment, .. }
            | OperationSuccess::SpecListGenerated { environment, .. }
            | OperationSuccess::SpecsValidated { environment, .. }
            | OperationSuccess::PackageCreated { environment, .. }
            | OperationSuccess::PackageUpdated { environment, .. }
            | OperationSuccess::PackageRemoved { environment, .. }
            | OperationSuccess::DotfilesApplied { environment, .. }
            | OperationSuccess::DotfileDriftChecked { environment, .. }
            | OperationSuccess::DotfileTracked { environment, .. } => Some(environment),
            OperationSuccess::SyncPushComplete { .. }
            | OperationSuccess::SyncPullComplete { .. }
            | OperationSuccess::SyncPullUpToDate { .. }
            | OperationSuccess::SyncNothingToPush { .. }
            | OperationSuccess::Generic(_) => None,
        }
    }

    /// Gets the steps completed from the success result
    #[must_use]
    pub fn steps_completed(&self) -> Option<StepCount> {
        match self {
            OperationSuccess::PackageChecked {
                steps_completed, ..
            }
            | OperationSuccess::PackageAudited {
                steps_completed, ..
            }
            | OperationSuccess::PackageInstalled {
                steps_completed, ..
            }
            | OperationSuccess::PackageValidated {
                steps_completed, ..
            }
            | OperationSuccess::SpecInfoRetrieved {
                steps_completed, ..
            }
            | OperationSuccess::PackageStatusChecked {
                steps_completed, ..
            }
            | OperationSuccess::PackageListGenerated {
                steps_completed, ..
            }
            | OperationSuccess::PackageCreated {
                steps_completed, ..
            }
            | OperationSuccess::PackageUpdated {
                steps_completed, ..
            }
            | OperationSuccess::PackageRemoved {
                steps_completed, ..
            }
            | OperationSuccess::SpecListGenerated {
                steps_completed, ..
            }
            | OperationSuccess::SpecsValidated {
                steps_completed, ..
            }
            | OperationSuccess::DotfilesApplied {
                steps_completed, ..
            }
            | OperationSuccess::DotfileDriftChecked {
                steps_completed, ..
            }
            | OperationSuccess::DotfileTracked {
                steps_completed, ..
            }
            | OperationSuccess::SyncPushComplete {
                steps_completed, ..
            }
            | OperationSuccess::SyncPullComplete {
                steps_completed, ..
            }
            | OperationSuccess::SyncPullUpToDate {
                steps_completed, ..
            }
            | OperationSuccess::SyncNothingToPush {
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
        OperationFailure::Package(crate::package::port::PackageError::EnvironmentNotFound {
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
        OperationFailure::Package(crate::package::port::PackageError::NoCheckCommand {
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
        OperationFailure::Package(crate::package::port::PackageError::NoInstallCommand {
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
        OperationFailure::Package(crate::package::port::PackageError::PackageNotFound {
            name,
            packages_path,
            files_examined,
            search_patterns,
        })
    }

    /// Creates a command execution failed error.
    ///
    /// Takes no `stdout`: see [`CommandFailure::ExecutionFailed`]. Takes `stderr`
    /// as a `&str` and bounds it here, so a caller cannot supply an already-built
    /// value that skipped the bound.
    #[must_use]
    pub fn command_failed(command: String, exit_code: Option<i32>, stderr: &str) -> Self {
        OperationFailure::CommandError(CommandFailure::ExecutionFailed {
            command,
            exit_code,
            stderr: crate::commands::BoundedText::bound(stderr.as_bytes()),
        })
    }

    /// Checks if this is an environment-related error
    #[must_use]
    pub fn is_environment_error(&self) -> bool {
        matches!(
            self,
            OperationFailure::Package(
                crate::package::port::PackageError::EnvironmentNotFound { .. }
                    | crate::package::port::PackageError::NoCheckCommand { .. }
                    | crate::package::port::PackageError::NoInstallCommand { .. }
            )
        )
    }

    /// Checks if this is a package-related error
    #[must_use]
    pub fn is_package_error(&self) -> bool {
        matches!(
            self,
            OperationFailure::Package(
                crate::package::port::PackageError::PackageNotFound { .. }
                    | crate::package::port::PackageError::MultiplePackagesFound { .. }
                    | crate::package::port::PackageError::ParseError { .. }
                    | crate::package::port::PackageError::PackageAlreadyExists { .. }
            )
        )
    }

    /// Checks if this is a command-related error
    #[must_use]
    pub fn is_command_error(&self) -> bool {
        matches!(self, OperationFailure::CommandError(_))
    }

    /// Checks if this is a dependency-related error
    #[must_use]
    pub fn is_dependency_error(&self) -> bool {
        matches!(self, OperationFailure::DependencyError(_))
    }

    /// Gets the package error details if this is a package error
    #[must_use]
    pub fn package_error(&self) -> Option<&crate::package::port::PackageError> {
        match self {
            OperationFailure::Package(pkg_err) => Some(pkg_err),
            _ => None,
        }
    }

    /// Gets the dependency failure details if this is a dependency error
    #[must_use]
    pub fn dependency_failure(&self) -> Option<&DependencyFailure> {
        match self {
            OperationFailure::DependencyError(dep_err) => Some(dep_err),
            _ => None,
        }
    }

    /// Creates a circular dependency error
    #[must_use]
    pub fn circular_dependency(package_name: String, cycle: Vec<String>) -> Self {
        OperationFailure::DependencyError(DependencyFailure::CircularDependency {
            package_name,
            cycle,
        })
    }

    /// Creates a missing dependency error
    #[must_use]
    pub fn missing_dependency(package_name: String, dependency_name: String) -> Self {
        OperationFailure::DependencyError(DependencyFailure::MissingDependency {
            package_name,
            dependency_name,
        })
    }
}

impl From<crate::package::port::PackageRepoError> for OperationFailure {
    fn from(err: crate::package::port::PackageRepoError) -> Self {
        match err {
            crate::package::port::PackageRepoError::PackageError(pkg_err) => {
                OperationFailure::Package(*pkg_err)
            }
            crate::package::port::PackageRepoError::PackageListError(list_err) => {
                OperationFailure::PackageList(list_err)
            }
            crate::package::port::PackageRepoError::IoError(io_err) => {
                OperationFailure::Generic(format!("IO error: {io_err}"))
            }
            crate::package::port::PackageRepoError::FileSystemError(fs_err) => {
                OperationFailure::Generic(format!("File system error: {fs_err}"))
            }
            // Rendered by its own `Display`, which already names the offending
            // field paths — the message is worth stating in exactly one place.
            err @ crate::package::port::PackageRepoError::UnknownDotfileFields { .. } => {
                OperationFailure::Generic(err.to_string())
            }
        }
    }
}

impl From<crate::package::port::PackageListError> for OperationFailure {
    fn from(err: crate::package::port::PackageListError) -> Self {
        OperationFailure::PackageList(err)
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

    /// Sorted filtered package list ready for display (before status checks begin)
    PackageListReady {
        operation_info: OperationInfo,
        packages: Vec<PackageListItem>,
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

    /// Audit result completed
    AuditResultCompleted {
        operation_info: OperationInfo,
        audit_result: AuditResultData,
    },

    /// Validation result completed
    ValidationResultCompleted {
        operation_info: OperationInfo,
        validation_result: ValidationResultData,
    },

    /// Individual package list item completed (for streaming)
    PackageListItemCompleted {
        operation_info: OperationInfo,
        package_item: PackageListItem,
    },

    /// Information about dependent packages found during removal
    RemovalDependencyInfo {
        operation_info: OperationInfo,
        package_name: String,
        dependent_packages: Vec<String>,
    },

    /// Info about config files that may need cleanup after package removal
    DotfileCleanupInfo {
        operation_info: OperationInfo,
        package_name: String,
        dotfile_targets: Vec<String>,
    },

    /// Individual spec list item completed (for streaming)
    SpecListItemCompleted {
        operation_info: OperationInfo,
        spec_item: SpecListItem,
    },

    /// Spec list loaded (summary data)
    SpecListLoaded {
        operation_info: OperationInfo,
        spec_list: SpecListData,
    },

    /// A recommended (soft) dependency install is starting
    RecommendStarted {
        operation_info: OperationInfo,
        recommend_name: String,
    },

    /// A recommended (soft) dependency installed successfully
    RecommendSucceeded {
        operation_info: OperationInfo,
        recommend_name: String,
    },

    /// A recommended (soft) dependency failed to install (non-fatal)
    RecommendFailed {
        operation_info: OperationInfo,
        recommend_name: String,
        error: String,
    },

    /// A config file is about to be deployed
    DotfileDeploying {
        operation_info: OperationInfo,
        source: String,
        target: String,
    },

    /// A config file was deployed successfully
    DotfileDeployed {
        operation_info: OperationInfo,
        source: String,
        target: String,
    },

    /// A config file was skipped (already current or user declined)
    DotfileSkipped {
        operation_info: OperationInfo,
        source: String,
        target: String,
        reason: String,
    },

    /// A conflict was detected between repo and deployed version
    DotfileConflict {
        operation_info: OperationInfo,
        source: String,
        target: String,
        diff: String,
    },

    /// Drift detected between deployed file and repo source
    DotfileDriftDetected {
        operation_info: OperationInfo,
        target: String,
        drift_type: String,
    },

    /// Post-install note to display to user
    PostInstallNote {
        operation_info: OperationInfo,
        package_name: String,
        note: String,
    },

    /// Git repository status for sync status command
    SyncRepoStatus {
        operation_info: OperationInfo,
        repo_root: std::path::PathBuf,
        branch: Option<String>,
        modified_count: usize,
        staged_count: usize,
        untracked_count: usize,
        deleted_count: usize,
        ahead: usize,
        behind: usize,
    },

    /// Dotfile drift summary for sync status command
    SyncDriftSummary {
        operation_info: OperationInfo,
        drifted_targets: Vec<String>,
        total_deployed: usize,
    },

    /// A commit was created during sync push
    SyncCommitCreated {
        operation_info: OperationInfo,
        package_name: String,
        message: String,
    },
}

/// Structured data for package information
#[derive(Debug, Clone)]
pub struct PackageInfoData {
    pub name: String,
    pub description: Option<String>,
    pub homepage: Option<String>,
    pub environments: Vec<String>,
    pub current_environment: String,
    pub git_status: Option<super::git::GitFileStatus>,
}

/// Structured data for environment status
#[derive(Debug, Clone)]
pub struct EnvironmentStatusData {
    pub environment_name: String,
    pub is_current: bool,
    pub install_command: String,
    pub check_command: Option<String>,
    pub dependencies: Vec<String>,
    pub dependency_statuses: Vec<DependencyStatus>,
    pub recommends: Vec<String>,
    pub recommend_statuses: Vec<DependencyStatus>,
    pub status: Option<EnvironmentStatus>,
}

/// Status of a package in an environment
#[derive(Debug, Clone)]
pub enum EnvironmentStatus {
    Installed,
    NotInstalled,
    Unknown(String),
}

/// Installation status of a dependency package
#[derive(Debug, Clone)]
pub struct DependencyStatus {
    pub name: String,
    pub status: EnvironmentStatus,
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
    pub environments: Vec<String>,
    pub status: Option<CheckResult>,
}

/// Information about an invalid package
#[derive(Debug, Clone)]
pub struct InvalidPackageInfo {
    pub path: String,
    pub error: String,
}

/// Information about a spec (definition only, no runtime status)
#[derive(Debug, Clone)]
pub struct SpecListItem {
    pub name: String,
    pub description: Option<String>,
    pub environments: Vec<String>,
    pub git_status: Option<super::git::GitFileStatus>,
}

/// Structured data for spec list
#[derive(Debug, Clone)]
pub struct SpecListData {
    pub specs: Vec<SpecListItem>,
    pub invalid_packages: Vec<InvalidPackageInfo>,
    pub current_environment: String,
    pub package_directory: String,
    pub environment_stats: std::collections::HashMap<String, usize>,
    pub show_all: bool,
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
#[derive(Debug, Clone, strum::Display)]
pub enum CheckResult {
    #[strum(to_string = "successfully")]
    Success { stdout: String, stderr: String },
    #[strum(to_string = "with failures")]
    Failed {
        stdout: String,
        stderr: String,
        exit_code: Option<i32>,
    },
    #[strum(to_string = "but command not found")]
    CommandNotFound,
    #[strum(to_string = "but no check command defined")]
    NoCheckCommand,
    #[strum(to_string = "with errors")]
    Error(String),
}

/// Structured data for audit results
#[derive(Debug, Clone)]
pub struct AuditResultData {
    pub package_name: String,
    pub environment: String,
    pub audit_command: Option<String>,
    pub result: AuditResult,
}

/// Result of an audit operation
#[derive(Debug, Clone, strum::Display)]
pub enum AuditResult {
    #[strum(to_string = "clean")]
    Clean { sources: Vec<String> },
    #[strum(to_string = "with conflicts")]
    Conflicts {
        sources: Vec<String>,
        expected: Vec<String>,
    },
    #[strum(to_string = "not installed")]
    NotInstalled,
    #[strum(to_string = "no audit command defined")]
    NoAuditCommand,
    #[strum(to_string = "with errors")]
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
#[derive(Debug, Clone, strum::Display)]
pub enum ValidationStatus {
    #[strum(to_string = "successfully")]
    Valid,
    #[strum(to_string = "with warnings")]
    HasWarnings,
    #[strum(to_string = "with errors")]
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
    /// Source location (e.g., `"line 17 column 1"`) when available from parse errors.
    pub location: Option<String>,
}

/// Validation issue level
#[derive(Debug, Clone)]
pub enum ValidationLevel {
    Error,
    Warning,
    Info,
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

/// Structured update fields for modifying a package
#[derive(Debug, Clone, Default)]
pub struct PackageUpdateFields {
    /// Top-level: update package description
    pub description: Option<String>,
    /// Top-level: update package homepage
    pub homepage: Option<String>,
    /// Environment-scoped: update install command (requires environment).
    /// `Option<String>` because install is a required field — it can be replaced but not removed.
    pub install: Option<String>,
    /// Environment-scoped: update check command (requires environment).
    /// `Option<Option<String>>`: None=unchanged, Some(None)=remove, Some(Some(val))=set.
    pub check: Option<Option<String>>,
    /// Environment-scoped: update audit command (requires environment).
    /// `Option<Option<String>>`: None=unchanged, Some(None)=remove, Some(Some(val))=set.
    pub audit: Option<Option<String>>,
    /// Environment-scoped: update dependencies (requires environment)
    pub dependencies: Option<Vec<String>>,
    /// Environment-scoped: update recommends (requires environment)
    pub recommends: Option<Vec<String>>,
    /// Target environment for environment-scoped fields
    pub environment: Option<String>,
    /// Add a new environment configuration
    pub add_environment: Option<AddEnvironment>,
    /// Remove an environment configuration
    pub remove_environment: Option<String>,
}

/// Configuration for adding a new environment
#[derive(Debug, Clone)]
pub struct AddEnvironment {
    pub name: String,
    pub install: String,
    pub check: Option<String>,
    pub audit: Option<String>,
    pub dependencies: Vec<String>,
    pub recommends: Vec<String>,
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

    /// Compile-time exhaustiveness guard: if a new `PackageError` variant is added,
    /// this match will fail to compile, reminding you to update
    /// `OperationFailure::is_environment_error()` and `is_package_error()`.
    #[test]
    fn all_package_error_variants_are_categorized() {
        use crate::package::port::PackageError;

        fn categorize(err: &PackageError) -> &'static str {
            match err {
                // Environment-related (matched by is_environment_error)
                PackageError::EnvironmentNotFound { .. }
                | PackageError::NoCheckCommand { .. }
                | PackageError::NoInstallCommand { .. } => "environment",
                // Package-related (matched by is_package_error)
                PackageError::PackageNotFound { .. }
                | PackageError::MultiplePackagesFound { .. }
                | PackageError::ParseError { .. }
                | PackageError::PackageAlreadyExists { .. } => "package",
            }
        }
        // The exhaustive match above is the real test — it forces a compile
        // error when new variants are added to PackageError.
        let _ = categorize;
    }
}
