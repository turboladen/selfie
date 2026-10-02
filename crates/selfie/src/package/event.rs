//! How a package operation reports what it is doing.
//!
//! An operation emits [`PackageEvent`]s as it runs; a UI layer consumes the
//! stream and decides how to show them. That is what lets the CLI print progress
//! to a terminal while the MCP server collects the same events into JSON, with
//! neither choice reaching into the library.

pub mod metadata;

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

/// Whether a progress step waits on something outside selfie.
///
/// A consumer shows a waiting step until its [`PackageEvent::StepEnded`]; a
/// local one is detail a consumer may hide.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepKind {
    /// Work selfie does itself, such as reading spec files.
    Local,
    /// A configured command, a provider command or a network call that selfie
    /// has started and is waiting on. The step's message names what it waits
    /// on. Waiting steps may overlap, so each carries its own id, and exactly
    /// one [`PackageEvent::StepEnded`] with that id follows.
    Waiting(StepId),
}

/// Identifies one waiting step, from its [`StepKind::Waiting`] progress event
/// to its [`PackageEvent::StepEnded`]. Unique within the process, and a step
/// started later has a greater id.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct StepId(u64);

impl StepId {
    /// A fixed id, for building events in tests.
    #[cfg(any(test, feature = "with_mocks"))]
    #[must_use]
    pub const fn from_raw(raw: u64) -> Self {
        Self(raw)
    }

    fn next() -> Self {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        Self(NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed))
    }
}

/// How a waiting step ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepEnding {
    /// What was waited on finished and gave its answer, whatever that answer
    /// was: a check command that reports "not installed" still succeeded.
    Succeeded,
    /// What was waited on failed, timed out or could not start.
    Failed,
    /// The run was cancelled while it waited.
    Cancelled,
}

/// Which configured directory a dotfile's source is read from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BaseKind {
    /// The package directory, where package specs live.
    PackageDirectory,
    /// The dotfiles directory, where standalone dotfile specs live.
    DotfilesDirectory,
}

/// The directory a source is relative to, and which one it is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceBase {
    /// Which configured directory this is.
    pub kind: BaseKind,
    /// The directory, as configured.
    pub directory: std::path::PathBuf,
}

/// Where a dotfile's content comes from, as events report it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DotfileSource {
    /// A repository file, or a template rendered from one.
    File {
        /// The directory `path` is relative to, or `None` when the file lies under
        /// neither configured directory, and `path` is then the full path.
        base: Option<SourceBase>,
        /// The file, relative to `base`.
        path: std::path::PathBuf,
        /// A template's var names; empty for a plain file.
        vars: Vec<String>,
    },
    /// A command whose output is the content.
    Command(String),
    /// A source from a deploy record that names no base: the spelling the record
    /// holds, relative to a directory that is not known.
    Recorded(String),
}

impl DotfileSource {
    /// The file's full path, when the source is a file.
    #[must_use]
    pub fn absolute(&self) -> Option<std::path::PathBuf> {
        match self {
            Self::File {
                base: Some(base),
                path,
                ..
            } => Some(base.directory.join(path)),
            Self::File {
                base: None, path, ..
            } => Some(path.clone()),
            Self::Command(_) | Self::Recorded(_) => None,
        }
    }
}

/// The source in full: a file's full path with any var names, the command, or
/// the recorded spelling.
impl fmt::Display for DotfileSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.write(f, Self::absolute)
    }
}

impl DotfileSource {
    /// The source as a line under its base directory's heading reads it: a
    /// file's path relative to its base, or in full when it has none; otherwise
    /// as [`Display`](fmt::Display) shows it.
    #[must_use]
    pub fn relative(&self) -> impl fmt::Display + '_ {
        struct Relative<'a>(&'a DotfileSource);
        impl fmt::Display for Relative<'_> {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.write(f, |source| match source {
                    DotfileSource::File {
                        base: Some(_),
                        path,
                        ..
                    } => Some(path.clone()),
                    _ => source.absolute(),
                })
            }
        }
        Relative(self)
    }

    fn write(
        &self,
        f: &mut fmt::Formatter<'_>,
        path_of: impl Fn(&Self) -> Option<std::path::PathBuf>,
    ) -> fmt::Result {
        match self {
            Self::File { vars, .. } => {
                let path = path_of(self).unwrap_or_default();
                let vars: Vec<&str> = vars.iter().map(String::as_str).collect();
                crate::package::write_file_source(f, &path.display(), &vars)
            }
            Self::Command(command) => crate::package::write_command_source(f, command),
            Self::Recorded(spelling) => f.write_str(spelling),
        }
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
    /// Create an event sender that stamps every event with this operation's
    /// context.
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

    /// Send an event to every consumer on the stream.
    ///
    /// A send error is ignored: it means the consumer has disconnected, which is
    /// not a failure of the operation being reported.
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
        kind: StepKind,
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
        let percent_complete = if total_steps == 0 {
            0.0
        } else {
            step as f32 / total_steps as f32
        };
        self.send(PackageEvent::Progress {
            operation_info,
            step,
            total_steps,
            percent_complete,
            kind,
            message: msg,
        })
        .await;
    }

    /// Send a waiting step that is not one of a numbered sequence: its `step`
    /// and `total_steps` are both 0. The caller must end it with
    /// [`send_step_ended`](Self::send_step_ended).
    #[must_use = "a waiting step must be ended with `send_step_ended`"]
    pub(crate) async fn send_waiting(&self, message: impl fmt::Display) -> StepId {
        self.send_waiting_progress(0, 0, message).await
    }

    /// Send a numbered waiting step. The caller must end it with
    /// [`send_step_ended`](Self::send_step_ended).
    #[must_use = "a waiting step must be ended with `send_step_ended`"]
    pub(crate) async fn send_waiting_progress(
        &self,
        step: usize,
        total_steps: usize,
        message: impl fmt::Display,
    ) -> StepId {
        let id = StepId::next();
        self.send_progress(step, total_steps, StepKind::Waiting(id), message)
            .await;
        id
    }

    /// End the waiting step `step`.
    pub(crate) async fn send_step_ended(&self, step: StepId, ending: StepEnding) {
        let operation_info = self.touch_operation_info();
        self.send(PackageEvent::StepEnded {
            operation_info,
            step,
            ending,
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

    /// Emit `message` as both a tracing record and the event variant matching
    /// `level`.
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

    /// Send a line of the output of the command waiting step `step` runs.
    pub(crate) async fn send_info(&self, step: StepId, output: ConsoleOutput) {
        let operation_info = self.touch_operation_info();

        tracing::info!(
            operation_type = operation_info.operation_type.to_string(),
            package_name = &operation_info.package_name,
            environment = &operation_info.environment,
            output = ?&output,
        );

        self.send(PackageEvent::Info {
            operation_info,
            step,
            output,
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

    /// Report a package file that could not be parsed.
    ///
    /// The failure travels whole; nothing here renders a sentence.
    pub(crate) async fn send_spec_skipped(&self, error: crate::package::port::PackageParseError) {
        let operation_info = self.touch_operation_info();
        self.send(PackageEvent::SpecSkipped {
            operation_info,
            error,
        })
        .await;
    }

    /// Report packages refused whole: one event for each distinct kind and
    /// reason, in the order each pair first appears, naming every package
    /// refused for it.
    pub(crate) async fn send_packages_refused(
        &self,
        refusals: impl IntoIterator<Item = PackageRefusal>,
    ) {
        for (kind, reason, packages) in group_refusals(refusals) {
            self.send_refused_group(kind, reason, packages).await;
        }
    }

    /// Report `packages`, all refused whole as `kind` for `reason`, as one
    /// event.
    pub(crate) async fn send_refused_group(
        &self,
        kind: RefusalKind,
        reason: String,
        packages: Vec<RefusedPackage>,
    ) {
        let operation_info = self.touch_operation_info();
        let names: Vec<&str> = packages.iter().map(|p| p.name.as_str()).collect();
        tracing::warn!(
            operation_type = operation_info.operation_type.to_string(),
            environment = &operation_info.environment,
            kind = ?kind,
            packages = ?names,
            reason = &reason,
            "packages refused",
        );
        self.send(PackageEvent::PackagesRefused {
            operation_info,
            kind,
            reason,
            packages,
        })
        .await;
    }

    /// Report the recommended packages a cancel left untried.
    pub(crate) async fn send_recommends_untried(&self, names: Vec<String>) {
        let operation_info = self.touch_operation_info();
        self.send(PackageEvent::RecommendsUntried {
            operation_info,
            names,
        })
        .await;
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

    /// Send the dotfile listing.
    pub(crate) async fn send_dotfile_list(&self, dotfile_list: DotfileListData) {
        let operation_info = self.touch_operation_info();
        self.send(PackageEvent::DotfileListLoaded {
            operation_info,
            dotfile_list,
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
        source: &DotfileSource,
        target: impl fmt::Display,
    ) {
        let operation_info = self.touch_operation_info();
        self.send(PackageEvent::DotfileDeploying {
            operation_info,
            source: source.clone(),
            target: target.to_string(),
        })
        .await;
    }

    /// Send a dotfile-deployed event
    ///
    /// `backup` is where the target's former content was copied, or `None` when
    /// nothing was kept.
    pub(crate) async fn send_dotfile_deployed(
        &self,
        source: &DotfileSource,
        target: impl fmt::Display,
        backup: Option<&std::path::Path>,
    ) {
        let operation_info = self.touch_operation_info();
        self.send(PackageEvent::DotfileDeployed {
            operation_info,
            source: source.clone(),
            target: target.to_string(),
            backup: backup.map(|path| path.display().to_string()),
        })
        .await;
    }

    /// Send a dotfile-skipped event
    pub(crate) async fn send_dotfile_skipped(
        &self,
        source: &DotfileSource,
        target: impl fmt::Display,
        reason: SkipReason,
    ) {
        let operation_info = self.touch_operation_info();
        self.send(PackageEvent::DotfileSkipped {
            operation_info,
            source: source.clone(),
            target: target.to_string(),
            reason,
        })
        .await;
    }

    /// Send a dotfile-conflict event
    pub(crate) async fn send_dotfile_conflict(
        &self,
        source: &DotfileSource,
        target: impl fmt::Display,
        diff: impl fmt::Display,
    ) {
        let operation_info = self.touch_operation_info();
        self.send(PackageEvent::DotfileConflict {
            operation_info,
            source: source.clone(),
            target: target.to_string(),
            diff: diff.to_string(),
        })
        .await;
    }

    /// Send a dotfile-orphaned event
    pub(crate) async fn send_dotfile_orphaned(
        &self,
        source: &DotfileSource,
        target: impl fmt::Display,
        package: Option<&str>,
    ) {
        let operation_info = self.touch_operation_info();
        self.send(PackageEvent::DotfileOrphaned {
            operation_info,
            source: source.clone(),
            target: target.to_string(),
            package: package.map(str::to_string),
        })
        .await;
    }

    /// Send a dotfile-drift-detected event
    pub(crate) async fn send_dotfile_drift_detected(
        &self,
        target: impl fmt::Display,
        drift_type: DriftType,
    ) {
        let operation_info = self.touch_operation_info();
        self.send(PackageEvent::DotfileDriftDetected {
            operation_info,
            target: target.to_string(),
            drift_type,
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

/// Operation-specific data that does not belong on [`OperationInfo`].
#[derive(Debug, Clone, Default)]
pub struct OperationContext {
    /// Package file path (used by validate, create operations)
    pub package_path: Option<std::path::PathBuf>,
    /// Target environment for cross-environment operations
    pub target_environment: Option<String>,
}

/// Result of an operation.
///
/// Each [`OperationSuccess`] variant carries the fields a caller needs to render
/// it, so match on the variant rather than parsing a message:
///
/// ```rust
/// use selfie::package::event::{OperationResult, OperationSuccess};
///
/// fn render(result: OperationResult) -> String {
///     match result {
///         OperationResult::Success(OperationSuccess::PackageInstalled {
///             package_name,
///             was_already_installed,
///             ..
///         }) if was_already_installed => format!("{package_name} was already installed"),
///         OperationResult::Success(_) => "done".to_string(),
///         OperationResult::Failure(failure) => format!("failed: {failure}"),
///     }
/// }
/// ```
#[derive(Debug, Clone)]
pub enum OperationResult {
    Success(OperationSuccess),
    Failure(OperationFailure),
}

impl OperationResult {
    /// How the operation scores; see [`Outcome`].
    #[must_use]
    pub fn outcome(&self) -> Outcome {
        // The one verdict: the CLI's exit code and the MCP envelope both come
        // from here, so neither decides for itself what a result means.
        match self {
            OperationResult::Success(success) => success.outcome(),
            OperationResult::Failure(_) => Outcome::Failed,
        }
    }
}

/// How an operation ended: clean, a finding, or a failure.
///
/// A cancelled operation has no outcome: it ends with
/// [`PackageEvent::Canceled`] rather than a result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// Did everything it was asked, and found nothing to report.
    Clean,
    /// Finished, and its answer is a finding: something it was asked to look
    /// for, such as drift or an audit conflict, is there.
    Found,
    /// Did not do everything it was asked: it failed, or refused part of the
    /// work.
    Failed,
}

/// Typed success information for operations
#[derive(Debug, Clone)]
pub enum OperationSuccess {
    /// Package check operation completed
    PackageChecked {
        package_name: String,
        environment: String,
        verdict: CheckVerdict,
        steps_completed: StepCount,
    },
    /// Package audit operation completed
    PackageAudited {
        package_name: String,
        environment: String,
        audit_result: AuditResult,
        steps_completed: StepCount,
    },
    /// Every package with an entry for this environment audited, and what the
    /// audits found.
    PackagesAudited {
        /// Packages audited, including those with no audit command.
        audited_count: usize,
        /// Packages installed from a source they do not expect.
        conflict_count: usize,
        /// Packages nothing provides.
        not_installed_count: usize,
        /// Packages whose audit could not run.
        error_count: usize,
        /// Spec files left out because they could not be loaded or selfie will
        /// not read them.
        refused_count: usize,
        environment: String,
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
        /// Errors found; nonzero exactly when `status` is `HasErrors`.
        error_count: usize,
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
        /// Specs the listing reported as refused. Reported, not failed: a listing
        /// that shows a refused spec has done what it was asked, so these are
        /// not counted by [`refused_count`](OperationSuccess::refused_count).
        refused_specs: usize,
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
        /// Specs the listing reported as refused. Reported, not failed: a listing
        /// that shows a refused spec has done what it was asked, so these are
        /// not counted by [`refused_count`](OperationSuccess::refused_count).
        refused_specs: usize,
        environment: String,
        steps_completed: StepCount,
    },
    /// Bulk spec validation operation completed
    SpecsValidated {
        validated_count: usize,
        /// Specs validated with errors.
        error_count: usize,
        /// Spec files that could not be read or parsed, and so were not
        /// validated. Each is an error.
        unparsable_count: usize,
        /// Names several spec files claim, and directories that could not be
        /// listed, so that their specs were not validated. Each is an error.
        uncollected_count: usize,
        /// Specs validated with warnings.
        warning_count: usize,
        /// Warnings about the run that belong to no validated spec, such as a
        /// spec file that could not be used or a missing dotfiles directory.
        other_warning_count: usize,
        environment: String,
        steps_completed: StepCount,
    },
    /// Dotfile apply operation completed
    DotfilesApplied {
        deployed_count: usize,
        /// Entries there was correctly nothing to do for: already in sync, or a
        /// dry run declining to act.
        ///
        /// Distinct from `refused_count`: an entry counted here needed no work,
        /// which is not the same as one selfie declined to touch.
        skipped_count: usize,
        conflict_count: usize,
        /// What selfie was asked to deploy and did not — refusals and failures
        /// alike.
        ///
        /// Usually an entry, but **not always one**: a package refused whole for
        /// a top-level key that hides a real field contributes 1 here and no
        /// entries at all, because its `dotfiles` list was swallowed by the very
        /// key being refused. A dotfiles directory that exists and could not be
        /// listed also contributes 1 and no entries. So this counts *outcomes*, matching
        /// `steps_completed`, and does not equal a number of dotfile entries.
        ///
        /// Non-zero makes [`outcome`](OperationSuccess::outcome) Failed: the run
        /// did not do what was asked. Named for the common case: most of what lands here was
        /// *declined* by selfie rather than failing, and `perform_deploy` is
        /// explicit that a refusal is not a failure.
        refused_count: usize,
        /// Recorded targets no entry deploys to any more whose files are still
        /// there. Reported, never removed, and not a refusal.
        orphan_count: usize,
        environment: String,
        steps_completed: StepCount,
    },
    /// Dotfile drift check operation completed
    DotfileDriftChecked {
        drift_count: usize,
        /// How many entries drift compared against their source. Refused and
        /// unverified entries are counted apart and never here.
        total_count: usize,
        /// What drift refused to check, each for a reason apply would refuse it
        /// too: a spec that could not be loaded; a name several spec files claim;
        /// a package refused whole; an entry that cannot deploy, whose target
        /// selfie will not write to or cannot read, or whose source escapes the
        /// package directory or cannot be read; or a dotfiles directory that exists
        /// and could not be listed.
        ///
        /// Its own field rather than part of `total_count`: `sync status`
        /// renders that total as "N deployed", so a refusal counted there would
        /// report an entry nobody checked as one that is in place. Apply's total
        /// does include its refusals, because that one feeds a step count and
        /// records outcomes rather than entries.
        refused_count: usize,
        /// Secret-bearing entries drift reported without checking, since their
        /// content comes from running commands. Not a refusal, and not a sign the
        /// check is incomplete: such an entry is unverifiable by design.
        // Counting one as a refusal would fail every drift check on a machine that
        // has one (ADR-0003).
        unverified_count: usize,
        /// Recorded targets no entry deploys to any more whose files are still
        /// there. Not drift and not a refusal.
        orphan_count: usize,
        /// Recorded targets the orphan check could not judge, because something
        /// it warned about kept it from seeing every entry. Not a refusal.
        unjudged_count: usize,
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
    /// The process holds privilege it must not write dotfiles with.
    ///
    /// Typed rather than folded into [`Generic`](Self::Generic) so a test can
    /// assert the refusal happened without matching on its wording, and so an
    /// adapter can render the two halves of
    /// [`SudoRefusal`](crate::privilege::SudoRefusal) in its own channels.
    Privilege(crate::privilege::SudoRefusal),
    /// A command was given a spec selfie will not read.
    ///
    /// Typed rather than folded into [`Generic`](Self::Generic) for the reason
    /// [`Privilege`](Self::Privilege) is: a test can assert the refusal happened
    /// without matching on its wording, and an adapter reporting structure
    /// rather than prose has the package and the reason as separate fields.
    UnreadableSpec {
        package_name: String,
        reason: String,
    },
    /// A spec that would not pass `spec validate`, so nothing was written.
    ///
    /// `issues` lists every issue the validation found, errors first, then
    /// warnings and notices, in the shape a validation result carries.
    // Typed so an adapter can render each issue in its own channel, and an MCP
    // client gets the fields rather than a sentence to parse.
    InvalidSpec {
        package_name: String,
        issues: Vec<ValidationIssueData>,
    },
    /// A command named a package it could not find.
    // Typed rather than folded into `Generic` so an adapter can tell a typo from
    // a spec that failed to load without parsing the sentence.
    NoSuchPackage {
        name: String,
        reason: NoSuchPackageReason,
    },
    /// Generic failure with a freeform message
    Generic(String),
}

/// Why a named package could not be found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NoSuchPackageReason {
    /// No spec file has the name.
    NotFound,
    /// No spec file that could be listed has the name, and a dotfiles directory
    /// could not be listed, so the package may be in it.
    MaybeInUnlistableDirectory,
    /// No spec file that could be listed has the name, and a dotfiles directory
    /// could not be classified at all, so nothing is known about what is at its
    /// path.
    ///
    /// Separate from [`MaybeInUnlistableDirectory`](Self::MaybeInUnlistableDirectory)
    /// because that one asserts a directory is there holding entries selfie cannot
    /// see. A symlink loop establishes no such thing, and saying so sends the user
    /// to look inside a directory that may not exist.
    MaybeInUncheckableDirectory,
    /// A spec file has the name and could not be loaded.
    NotLoaded,
    /// More than one spec file in one directory claims the name, so none is
    /// used.
    Ambiguous {
        /// The files claiming it, sorted.
        conflicting_paths: Vec<std::path::PathBuf>,
    },
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
    /// this the one stderr-forwarding site the compiler enforces. Do not add a
    /// `stdout` field back.
    ExecutionFailed {
        command: String,
        exit_code: Option<i32>,
        stderr: crate::commands::BoundedText,
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
    /// A package in the graph carries a spec selfie refuses to read
    ///
    /// Resolution stops rather than skipping the package, so nothing installs
    /// from a graph selfie knows to be incomplete.
    // `environments:` is the mapping a shadowing key hides, and that mapping is
    // where dependencies are declared -- so continuing would build a graph short
    // by an unknown number of edges and say nothing about it.
    UnreadableSpec {
        package_name: String,
        /// The package that pulled this one into the graph, when one did.
        ///
        /// `None` when the user named this package themselves.
        required_by: Option<String>,
        reason: String,
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
            // Both halves, because this is the one-string rendering an adapter
            // falls back to when it has nowhere separate to put a suggestion.
            OperationFailure::Privilege(refusal) => {
                write!(f, "{}. {}", refusal.message(), refusal.suggestion())
            }
            OperationFailure::UnreadableSpec {
                package_name,
                reason,
            } => write!(f, "Cannot use package `{package_name}`: {reason}"),
            OperationFailure::InvalidSpec {
                package_name,
                issues,
            } => {
                let errors = issues
                    .iter()
                    .filter(|issue| matches!(issue.level, ValidationLevel::Error))
                    .count();
                write!(
                    f,
                    "Refusing to create '{package_name}': it would not pass spec validate ({errors} \
                     {}), so nothing was written",
                    crate::pluralize(errors, "error", "errors")
                )
            }
            OperationFailure::NoSuchPackage { name, reason } => match reason {
                NoSuchPackageReason::NotFound => write!(f, "No package named '{name}' was found"),
                NoSuchPackageReason::MaybeInUnlistableDirectory => write!(
                    f,
                    "No package named '{name}' was found. A dotfiles directory could not be \
                     listed, so it may be there."
                ),
                NoSuchPackageReason::MaybeInUncheckableDirectory => write!(
                    f,
                    "No package named '{name}' was found. A dotfiles directory could not be \
                     checked, so whether it is there is unknown."
                ),
                NoSuchPackageReason::NotLoaded => write!(
                    f,
                    "Package '{name}' could not be loaded, so nothing was applied"
                ),
                NoSuchPackageReason::Ambiguous { conflicting_paths } => f.write_str(
                    &crate::package::port::ambiguous_files_sentence(name, conflicting_paths),
                ),
            },
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
            DependencyFailure::UnreadableSpec {
                package_name,
                required_by: Some(parent),
                reason,
            } => write!(
                f,
                "Package `{parent}` depends on `{package_name}`, which carries a spec selfie will \
                 not read, so its dependencies are unknown: {reason}"
            ),
            DependencyFailure::UnreadableSpec {
                package_name,
                required_by: None,
                reason,
            } => write!(
                f,
                "Package `{package_name}` carries a spec selfie will not read, so its dependencies \
                 are unknown: {reason}"
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

/// A listing's counts as "3 valid package(s) and 1 refused package(s)", naming
/// the invalid and refused counts only when either is not zero.
#[must_use]
pub fn listing_counts(valid: usize, invalid: usize, refused: usize, noun: &str) -> String {
    let mut parts = vec![format!("{valid} valid {noun}(s)")];
    if invalid > 0 {
        parts.push(format!("{invalid} invalid {noun}(s)"));
    }
    if refused > 0 {
        parts.push(format!("{refused} refused {noun}(s)"));
    }
    let last = parts.pop().unwrap_or_default();
    if parts.is_empty() {
        last
    } else {
        format!("{} and {last}", parts.join(", "))
    }
}

// Said only when there is one, so a summary with no orphans carries no orphan
// clause.
fn orphaned_clause(orphan_count: usize) -> String {
    if orphan_count == 0 {
        String::new()
    } else {
        format!(", {orphan_count} orphaned")
    }
}

impl std::fmt::Display for OperationSuccess {
    // Each message names the environment when the answer depends on it, and
    // never a step count: the count is `steps_completed`, a field for whoever
    // wants it.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OperationSuccess::PackageChecked {
                package_name,
                environment,
                verdict,
                ..
            } => write!(
                f,
                "Package '{package_name}' check completed {verdict} in environment '{environment}'"
            ),
            OperationSuccess::PackagesAudited {
                audited_count,
                conflict_count,
                not_installed_count,
                error_count,
                refused_count,
                environment,
                ..
            } => write!(
                f,
                "Audit completed for {audited_count} package(s) in environment '{environment}': \
                 {conflict_count} with conflicts, {not_installed_count} not installed, \
                 {error_count} could not be audited, {refused_count} spec(s) left out"
            ),
            OperationSuccess::PackageAudited {
                package_name,
                environment,
                audit_result,
                ..
            } => write!(
                f,
                "Package '{package_name}' audit completed {audit_result} in environment '{environment}'"
            ),
            OperationSuccess::PackageInstalled {
                package_name,
                environment,
                was_already_installed,
                executable_path,
                ..
            } => {
                // The path goes last, so the environment clause does not read as
                // part of it.
                let (status, at) = if *was_already_installed {
                    (
                        "was already installed",
                        executable_path.as_ref().map(|path| format!(", at {path}")),
                    )
                } else {
                    ("installation completed successfully", None)
                };
                write!(
                    f,
                    "Package '{package_name}' {status} in environment '{environment}'{}",
                    at.unwrap_or_default()
                )
            }
            OperationSuccess::PackageValidated {
                package_name,
                environment,
                status,
                error_count,
                warning_count,
                ..
            } => match status {
                ValidationStatus::HasErrors => write!(
                    f,
                    "Package '{package_name}' validation failed with {error_count} error(s) and {} warning(s) in environment '{environment}'",
                    warning_count.unwrap_or(0)
                ),
                ValidationStatus::HasWarnings => write!(
                    f,
                    "Package '{package_name}' validation completed with {} warning(s) in environment '{environment}'",
                    warning_count.unwrap_or(0)
                ),
                ValidationStatus::Valid => write!(
                    f,
                    "Package '{package_name}' validation completed {status} in environment '{environment}'"
                ),
            },
            OperationSuccess::SpecInfoRetrieved { package_name, .. } => write!(
                f,
                "Package '{package_name}' spec info retrieved successfully"
            ),
            OperationSuccess::PackageStatusChecked {
                package_name,
                environment,
                ..
            } => write!(
                f,
                "Package '{package_name}' status checked successfully in environment '{environment}'"
            ),
            OperationSuccess::PackageListGenerated {
                valid_count,
                invalid_count,
                refused_specs,
                environment,
                ..
            } => {
                let status =
                    listing_counts(*valid_count, *invalid_count, *refused_specs, "package");
                write!(
                    f,
                    "Package listing completed with {status} in environment '{environment}'"
                )
            }
            OperationSuccess::PackageCreated {
                package_name,
                file_path,
                ..
            } => write!(
                f,
                "Package '{package_name}' created at {}",
                file_path.display()
            ),
            OperationSuccess::PackageUpdated { package_name, .. } => {
                write!(f, "Package '{package_name}' updated successfully")
            }
            OperationSuccess::PackageRemoved {
                package_name,
                file_path,
                dependent_packages,
                ..
            } => {
                if dependent_packages.is_empty() {
                    write!(
                        f,
                        "Package '{package_name}' removed from {}",
                        file_path.display()
                    )
                } else {
                    write!(
                        f,
                        "Package '{package_name}' removed from {} (had {} dependent package(s))",
                        file_path.display(),
                        dependent_packages.len()
                    )
                }
            }
            OperationSuccess::SpecListGenerated {
                valid_count,
                invalid_count,
                refused_specs,
                environment,
                ..
            } => {
                let status = listing_counts(*valid_count, *invalid_count, *refused_specs, "spec");
                write!(
                    f,
                    "Spec listing completed with {status} in environment '{environment}'"
                )
            }
            OperationSuccess::SpecsValidated {
                validated_count,
                error_count,
                unparsable_count,
                uncollected_count,
                warning_count,
                other_warning_count,
                environment,
                ..
            } => {
                let mut status = if *error_count + *unparsable_count + *uncollected_count > 0 {
                    format!(
                        "{validated_count} package(s) validated, {error_count} with errors, {warning_count} with warnings, {unparsable_count} unparsable, {uncollected_count} ambiguous or unlistable"
                    )
                } else if *warning_count > 0 {
                    format!("{validated_count} package(s) validated, {warning_count} with warnings")
                } else {
                    format!("{validated_count} package(s) validated successfully")
                };
                if *other_warning_count > 0 {
                    status.push_str(&format!(", {other_warning_count} other warning(s)"));
                }
                write!(
                    f,
                    "Spec validation completed in environment '{environment}': {status}"
                )
            }
            OperationSuccess::DotfilesApplied {
                deployed_count,
                skipped_count,
                conflict_count,
                refused_count,
                orphan_count,
                environment,
                ..
            } => {
                write!(
                    f,
                    "Dotfiles applied in environment '{environment}': {deployed_count} deployed, \
                     {skipped_count} skipped, {conflict_count} conflict(s), {refused_count} refused{}",
                    orphaned_clause(*orphan_count)
                )
            }
            OperationSuccess::DotfileDriftChecked {
                drift_count,
                total_count,
                refused_count,
                unverified_count,
                orphan_count,
                unjudged_count,
                environment,
                ..
            } => {
                let unjudged = if *unjudged_count == 0 {
                    String::new()
                } else {
                    format!(", {unjudged_count} not checked for orphans")
                };
                write!(
                    f,
                    "Dotfile drift check in environment '{environment}': {drift_count} drifted out \
                     of {total_count}, {refused_count} refused, {unverified_count} not \
                     verifiable{}{unjudged}",
                    orphaned_clause(*orphan_count)
                )
            }
            OperationSuccess::DotfileTracked {
                name,
                target_path,
                was_already_tracked: true,
                ..
            } => write!(f, "Already tracking '{target_path}' in spec '{name}'"),
            OperationSuccess::DotfileTracked {
                name, target_path, ..
            } => write!(f, "Now tracking '{target_path}' in spec '{name}'"),
            OperationSuccess::SyncPushComplete { commits_pushed, .. } => {
                let label = crate::pluralize(*commits_pushed, "commit", "commits");
                write!(f, "Pushed {commits_pushed} {label} to remote")
            }
            OperationSuccess::SyncPullComplete {
                commits_pulled,
                packages_updated,
                packages_added,
                packages_removed,
                ..
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
                    write!(f, "Pulled {commits_pulled} {label} from remote")
                } else {
                    write!(
                        f,
                        "Pulled {commits_pulled} {label} from remote ({})",
                        parts.join("; ")
                    )
                }
            }
            OperationSuccess::SyncPullUpToDate { .. } => {
                write!(f, "Already up to date with remote")
            }
            OperationSuccess::SyncNothingToPush { .. } => {
                write!(f, "Nothing to push — working tree is clean")
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
            crate::commands::runner::CommandError::Timeout { command, .. } => {
                OperationFailure::CommandError(CommandFailure::InvalidCommand {
                    command,
                    reason: "Command timed out".to_string(),
                })
            }
            // Listed rather than matched with `_`, so adding a `CommandError`
            // variant fails to build here. This arm renders the error with
            // `Display`, and a variant whose `Display` carried command output
            // would leak it into `PackageEvent::Completed` and on to the CLI and
            // the MCP server's JSON. Every variant below names the command and
            // otherwise only text selfie chose. Check that before extending.
            //
            // `OutputReadFailed` renders the failed stream and an `io::Error`,
            // never output bytes. `ContentMarkersAbsent` renders the command and
            // nothing else -- it exists because the capture could not be split.
            crate::commands::runner::CommandError::Cancelled { .. }
            // `IoError` comes from a command that started, so it was found; it
            // renders the command and an `io::Error`.
            | crate::commands::runner::CommandError::IoError { .. }
            // Renders the command, the directory and an `io::Error`.
            | crate::commands::runner::CommandError::WorkingDirectoryUnusable { .. }
            // Names the program that would not start, which is not the command.
            | crate::commands::runner::CommandError::SpawnFailed { .. }
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
        verdict: CheckVerdict,
        steps_completed: StepCount,
    ) -> Self {
        OperationSuccess::PackageChecked {
            package_name,
            environment,
            verdict,
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
        error_count: usize,
        warning_count: Option<usize>,
        steps_completed: StepCount,
    ) -> Self {
        OperationSuccess::PackageValidated {
            package_name,
            environment,
            status,
            error_count,
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
    #[allow(clippy::too_many_arguments)]
    pub fn specs_validated(
        validated_count: usize,
        error_count: usize,
        unparsable_count: usize,
        uncollected_count: usize,
        warning_count: usize,
        other_warning_count: usize,
        environment: String,
        steps_completed: StepCount,
    ) -> Self {
        OperationSuccess::SpecsValidated {
            validated_count,
            error_count,
            unparsable_count,
            uncollected_count,
            warning_count,
            other_warning_count,
            environment,
            steps_completed,
        }
    }

    /// Create a `SpecListGenerated` success variant
    #[must_use]
    pub fn spec_list_generated(
        valid_count: usize,
        invalid_count: usize,
        refused_specs: usize,
        environment: String,
        steps_completed: StepCount,
    ) -> Self {
        OperationSuccess::SpecListGenerated {
            valid_count,
            invalid_count,
            refused_specs,
            environment,
            steps_completed,
        }
    }

    /// Create a `PackageListGenerated` success variant
    #[must_use]
    pub fn package_list_generated(
        valid_count: usize,
        invalid_count: usize,
        refused_specs: usize,
        environment: String,
        steps_completed: StepCount,
    ) -> Self {
        OperationSuccess::PackageListGenerated {
            valid_count,
            invalid_count,
            refused_specs,
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
            OperationSuccess::PackagesAudited { .. }
            | OperationSuccess::PackageListGenerated { .. }
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
    /// The one place that question is answered. [`outcome`](Self::outcome)
    /// scores such a success Failed, and that is what decides an exit code or a
    /// result envelope; ask this only to tell a refusal apart from other
    /// failures. An operation that completed while declining part of its work is
    /// still a completed operation, which is why this is a property of a success.
    ///
    /// True only for the variants [`refused_count`](Self::refused_count) answers for;
    /// every other variant is false. Give a variant a refusal count when it gains
    /// refusals, rather than teaching an adapter to look for them.
    #[must_use]
    pub fn had_refusals(&self) -> bool {
        self.refused_count().is_some_and(|count| count > 0)
    }

    /// How many refusals this success carries, for operations that count them.
    ///
    /// `None` where the question does not apply. Three variants answer it:
    /// [`DotfilesApplied`](Self::DotfilesApplied),
    /// [`DotfileDriftChecked`](Self::DotfileDriftChecked) and
    /// [`PackagesAudited`](Self::PackagesAudited). `Some(0)` is distinct from
    /// `None`, being a run that could have refused something and did not.
    ///
    /// Exists so an adapter can report the number rather than the fact. The MCP
    /// server puts it in its own JSON field: an assistant told only that
    /// something was refused has to parse the count out of a prose message,
    /// which is the failure mode structured output exists to avoid.
    #[must_use]
    pub fn refused_count(&self) -> Option<usize> {
        // Every variant listed, with no catch-all: a variant added later is then a
        // compile error here rather than a silent `None`, which is how an operation
        // that counts refusals would otherwise reach an adapter reporting none.
        match self {
            OperationSuccess::DotfilesApplied { refused_count, .. }
            | OperationSuccess::DotfileDriftChecked { refused_count, .. }
            | OperationSuccess::PackagesAudited { refused_count, .. } => Some(*refused_count),
            OperationSuccess::PackageChecked { .. }
            | OperationSuccess::PackageAudited { .. }
            | OperationSuccess::PackageInstalled { .. }
            | OperationSuccess::PackageValidated { .. }
            | OperationSuccess::PackageRemoved { .. }
            | OperationSuccess::PackageCreated { .. }
            | OperationSuccess::SpecInfoRetrieved { .. }
            | OperationSuccess::PackageStatusChecked { .. }
            | OperationSuccess::SpecsValidated { .. }
            | OperationSuccess::PackageUpdated { .. }
            | OperationSuccess::DotfileTracked { .. }
            | OperationSuccess::SyncPushComplete { .. }
            | OperationSuccess::SyncPullComplete { .. }
            | OperationSuccess::SyncPullUpToDate { .. }
            | OperationSuccess::SyncNothingToPush { .. }
            | OperationSuccess::Generic(_) => None,
            // A listing reports the specs it refuses and still did what it was
            // asked, so they are not refusals of work: counting them here would
            // exit a listing non-zero for showing a broken file.
            OperationSuccess::PackageListGenerated { .. }
            | OperationSuccess::SpecListGenerated { .. } => None,
        }
    }

    /// How many entries a drift check reported without verifying, for the one
    /// operation that counts them. `None` for every other operation.
    #[must_use]
    pub fn unverified_count(&self) -> Option<usize> {
        match self {
            OperationSuccess::DotfileDriftChecked {
                unverified_count, ..
            } => Some(*unverified_count),
            // Every variant listed, as in `refused_count`, so a variant that comes to
            // count unverified entries is a compile error here rather than a silent
            // `None`.
            OperationSuccess::DotfilesApplied { .. }
            | OperationSuccess::PackageChecked { .. }
            | OperationSuccess::PackageAudited { .. }
            | OperationSuccess::PackagesAudited { .. }
            | OperationSuccess::PackageInstalled { .. }
            | OperationSuccess::PackageValidated { .. }
            | OperationSuccess::PackageRemoved { .. }
            | OperationSuccess::PackageCreated { .. }
            | OperationSuccess::SpecInfoRetrieved { .. }
            | OperationSuccess::PackageStatusChecked { .. }
            | OperationSuccess::PackageListGenerated { .. }
            | OperationSuccess::SpecListGenerated { .. }
            | OperationSuccess::SpecsValidated { .. }
            | OperationSuccess::PackageUpdated { .. }
            | OperationSuccess::DotfileTracked { .. }
            | OperationSuccess::SyncPushComplete { .. }
            | OperationSuccess::SyncPullComplete { .. }
            | OperationSuccess::SyncPullUpToDate { .. }
            | OperationSuccess::SyncNothingToPush { .. }
            | OperationSuccess::Generic(_) => None,
        }
    }

    /// How many orphaned targets whose files are still there the operation
    /// reported, for the two that look for them. `None` for every other
    /// operation.
    #[must_use]
    pub fn orphan_count(&self) -> Option<usize> {
        match self {
            OperationSuccess::DotfilesApplied { orphan_count, .. }
            | OperationSuccess::DotfileDriftChecked { orphan_count, .. } => Some(*orphan_count),
            // Every variant listed, as in `refused_count`.
            OperationSuccess::PackageChecked { .. }
            | OperationSuccess::PackageAudited { .. }
            | OperationSuccess::PackagesAudited { .. }
            | OperationSuccess::PackageInstalled { .. }
            | OperationSuccess::PackageValidated { .. }
            | OperationSuccess::PackageRemoved { .. }
            | OperationSuccess::PackageCreated { .. }
            | OperationSuccess::SpecInfoRetrieved { .. }
            | OperationSuccess::PackageStatusChecked { .. }
            | OperationSuccess::PackageListGenerated { .. }
            | OperationSuccess::SpecListGenerated { .. }
            | OperationSuccess::SpecsValidated { .. }
            | OperationSuccess::PackageUpdated { .. }
            | OperationSuccess::DotfileTracked { .. }
            | OperationSuccess::SyncPushComplete { .. }
            | OperationSuccess::SyncPullComplete { .. }
            | OperationSuccess::SyncPullUpToDate { .. }
            | OperationSuccess::SyncNothingToPush { .. }
            | OperationSuccess::Generic(_) => None,
        }
    }

    /// How this success scores; see [`Outcome`].
    ///
    /// [`Outcome::Failed`] when the operation refused part of its work, and
    /// [`Outcome::Found`] when it finished and found what it was asked to look
    /// for. A refusal outranks a finding: an answer with a hole in it is not a
    /// complete answer, whatever else it found.
    #[must_use]
    pub fn outcome(&self) -> Outcome {
        // Refusals are answered once, by `had_refusals`, for every variant.
        if self.had_refusals() {
            return Outcome::Failed;
        }
        // Every variant listed, as in `refused_count`, so a variant added later has
        // to say how it scores rather than inherit `Clean`.
        match self {
            // Records the orphan check could not judge leave part of the question
            // unanswered, which the run warned about: a finding, not a refusal.
            OperationSuccess::DotfileDriftChecked {
                drift_count,
                orphan_count,
                unjudged_count,
                ..
            } => {
                if *drift_count > 0 || *orphan_count > 0 || *unjudged_count > 0 {
                    Outcome::Found
                } else {
                    Outcome::Clean
                }
            }
            // Apply is asked to deploy, not to find anything. A conflict left for
            // the user and an orphan are reported, and the README promises neither
            // makes the exit code non-zero.
            OperationSuccess::DotfilesApplied { .. } => Outcome::Clean,
            OperationSuccess::PackageAudited { audit_result, .. } => match audit_result {
                AuditResult::Clean { .. } => Outcome::Clean,
                AuditResult::Conflicts { .. } | AuditResult::NotInstalled => Outcome::Found,
                // Neither can answer the question: one audit did not run, and the
                // other has nothing to run.
                AuditResult::Error(_) | AuditResult::NoAuditCommand => Outcome::Failed,
            },
            // A package with no audit command is left out of every count: across
            // many packages, not having one is ordinary.
            OperationSuccess::PackagesAudited {
                conflict_count,
                not_installed_count,
                error_count,
                ..
            } => {
                if *error_count > 0 {
                    Outcome::Failed
                } else if *conflict_count > 0 || *not_installed_count > 0 {
                    Outcome::Found
                } else {
                    Outcome::Clean
                }
            }
            OperationSuccess::PackageChecked { verdict, .. } => match verdict {
                CheckVerdict::Installed => Outcome::Clean,
                CheckVerdict::NotInstalled { .. } => Outcome::Found,
            },
            OperationSuccess::PackageValidated { status, .. } => status.outcome(),
            // A spec that could not be read, or a name several files claim, is an
            // error like a spec with errors: the run could not validate it.
            OperationSuccess::SpecsValidated {
                error_count,
                unparsable_count,
                uncollected_count,
                warning_count,
                other_warning_count,
                ..
            } => {
                if *error_count + *unparsable_count + *uncollected_count > 0 {
                    Outcome::Failed
                } else if *warning_count > 0 || *other_warning_count > 0 {
                    Outcome::Found
                } else {
                    Outcome::Clean
                }
            }
            OperationSuccess::PackageInstalled { .. }
            | OperationSuccess::SpecInfoRetrieved { .. }
            | OperationSuccess::PackageStatusChecked { .. }
            | OperationSuccess::PackageListGenerated { .. }
            | OperationSuccess::PackageCreated { .. }
            | OperationSuccess::PackageUpdated { .. }
            | OperationSuccess::PackageRemoved { .. }
            | OperationSuccess::SpecListGenerated { .. }
            | OperationSuccess::DotfileTracked { .. }
            | OperationSuccess::SyncPushComplete { .. }
            | OperationSuccess::SyncPullComplete { .. }
            | OperationSuccess::SyncPullUpToDate { .. }
            | OperationSuccess::SyncNothingToPush { .. }
            | OperationSuccess::Generic(_) => Outcome::Clean,
        }
    }

    /// Gets the environment from the success result if available
    #[must_use]
    pub fn environment(&self) -> Option<&str> {
        match self {
            OperationSuccess::PackageChecked { environment, .. }
            | OperationSuccess::PackageAudited { environment, .. }
            | OperationSuccess::PackagesAudited { environment, .. }
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
            | OperationSuccess::PackagesAudited {
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
                    | crate::package::port::PackageError::UnreadableFile { .. }
                    | crate::package::port::PackageError::UnusableName { .. }
                    | crate::package::port::PackageError::PackageAlreadyExists { .. }
                    | crate::package::port::PackageError::PackagePathOccupied { .. }
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

    /// Creates a failure for a package whose spec selfie refuses to read
    #[must_use]
    pub fn unreadable_spec(
        package_name: String,
        required_by: Option<String>,
        reason: String,
    ) -> Self {
        OperationFailure::DependencyError(DependencyFailure::UnreadableSpec {
            package_name,
            required_by,
            reason,
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
            // All rendered by their own `Display`, and none wrapped in a prefix.
            // The unknown-key refusals already name the offending field paths,
            // and `UncheckedTopLevel` carries the parse failure that stands in
            // for them; `UnwritablePath` is worded for the direction it refuses
            // and must not pick up the "File system error: " frame above, which is
            // what would reintroduce the target-facing phrasing it exists to
            // avoid. Each message is worth stating in exactly one place.
            err @ (crate::package::port::PackageRepoError::UnknownDotfileFields { .. }
            | crate::package::port::PackageRepoError::UnknownTopLevelFields { .. }
            | crate::package::port::PackageRepoError::UncheckedTopLevel { .. }
            | crate::package::port::PackageRepoError::UnknownEnvironmentFields { .. }
            | crate::package::port::PackageRepoError::UnwritablePath { .. }) => {
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
        /// Whether the step waits on something outside selfie.
        kind: StepKind,
        message: String,
    },

    /// A waiting step ended.
    StepEnded {
        operation_info: OperationInfo,
        step: StepId,
        ending: StepEnding,
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
        /// The waiting step whose command wrote it.
        step: StepId,
        output: ConsoleOutput,
    },

    /// Warning message
    Warning {
        operation_info: OperationInfo,
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

    /// Every dotfile entry selfie could read across both directories.
    DotfileListLoaded {
        operation_info: OperationInfo,
        dotfile_list: DotfileListData,
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

    /// A package file was found but could not be turned into a package
    ///
    /// Carries the failure itself rather than a rendered sentence, so a consumer
    /// renders it the way its own surface wants.
    // A terminal wants one line; a structured consumer wants the reason, the kind
    // and the location as separate fields. Neither shape can be recovered from the
    // other, so the event carries neither.
    //
    // Its own variant rather than a typed `Warning`. That variant's message field
    // is shared by dozens of unrelated callers, almost none of them about parse
    // failures, so typing it would reach far past this problem.
    SpecSkipped {
        operation_info: OperationInfo,
        error: crate::package::port::PackageParseError,
    },

    /// Packages selfie refused whole, all for one reason.
    ///
    /// Sent once per distinct reason, before any package is acted on, so a
    /// consumer can say the reason once and name every package it refused.
    PackagesRefused {
        operation_info: OperationInfo,
        /// What selfie objected to, for a consumer that branches on it.
        kind: RefusalKind,
        /// The objection as a clause, which a consumer puts after the package
        /// names and a colon.
        reason: String,
        /// Every package refused for this reason, in the order they were met.
        packages: Vec<RefusedPackage>,
    },

    /// Recommended packages a cancel kept from being tried, in the order the
    /// package lists them: not started, or stopped before installing the next
    /// of their packages. One whose own command was interrupted is reported
    /// failed instead.
    RecommendsUntried {
        operation_info: OperationInfo,
        names: Vec<String>,
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
        source: DotfileSource,
        target: String,
    },

    /// A config file was deployed successfully
    DotfileDeployed {
        operation_info: OperationInfo,
        source: DotfileSource,
        target: String,
        /// Where the content the target held before this run wrote to it was
        /// copied, so a consumer can tell the user how to get it back.
        ///
        /// `None` when nothing was kept: the target did not exist, it already
        /// held what was written, or the entry is secret-bearing and so is never
        /// copied. A target two entries deploy to in one run is copied once, and
        /// both events name that one copy.
        backup: Option<String>,
    },

    /// A config file was left as it is, with nothing wrong: already current, a
    /// dry run, or content selfie does not check without running commands.
    DotfileSkipped {
        operation_info: OperationInfo,
        source: DotfileSource,
        target: String,
        reason: SkipReason,
    },

    /// A conflict was detected between repo and deployed version
    DotfileConflict {
        operation_info: OperationInfo,
        source: DotfileSource,
        target: String,
        diff: String,
    },

    /// A target selfie deployed that no entry deploys to any more, whose file is
    /// still there. selfie leaves the file alone.
    DotfileOrphaned {
        operation_info: OperationInfo,
        /// The source the target was last deployed from, relative to the base
        /// directory it was recorded against, or as its spec spelled it for a
        /// record that names no base.
        source: DotfileSource,
        target: String,
        /// The spec name of the package that last deployed it, or `None` where
        /// the record does not say.
        package: Option<String>,
    },

    /// Drift detected between deployed file and repo source
    DotfileDriftDetected {
        operation_info: OperationInfo,
        target: String,
        /// How the target and its source have moved since selfie last deployed
        /// it. Never [`DriftType::None`]: a target with no drift sends no event.
        drift_type: DriftType,
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
        /// What the drift run could not check at all: specs it could not load,
        /// names several spec files claim, packages, entries, or a dotfiles
        /// directory that exists and could not be listed.
        ///
        /// Without it this summary reports a clean run for a package `apply`
        /// refuses, which is the answer that sends a reader to run the command
        /// that will not run.
        refused_count: usize,
        /// How many warnings named work the drift check could not complete: each
        /// one the check relayed, such as a configured dotfiles directory that
        /// does not exist or a deploy state file it could not read, and the
        /// warning status itself sends when the check failed outright.
        // Its own field, apart from `refused_count`, which counts work selfie
        // refused: without it this summary reports a clean run under a warning
        // that named a problem the count does not capture.
        warned: usize,
        /// How many secret-bearing entries the drift check reported without
        /// verifying, since checking one would run its commands. Not counted in
        /// `total_deployed` or `refused_count`.
        unverified_count: usize,
        /// Recorded targets no entry deploys to any more whose files are still
        /// there. Not drift and not a refusal.
        orphan_count: usize,
        /// Recorded targets the drift check could not judge for orphans.
        unjudged_count: usize,
        /// How the drift check itself scored: [`Outcome::Failed`] when it refused
        /// something, failed, or ended without a result.
        drift_outcome: Outcome,
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
    /// Why `selfie apply` would refuse this spec in the current environment,
    /// when it would. `environments` and `dotfiles` are then empty, because
    /// neither can be trusted, and `apply_commands` is zero.
    pub refusal: Option<String>,
    /// Why apply would refuse this spec in another environment it declares, when
    /// it would there and not here.
    pub refusal_elsewhere: Option<String>,
    /// Every dotfile entry the spec declares, shared and per environment.
    pub dotfiles: Vec<ScopedDotfile>,
    /// How many commands `selfie apply` would run in the current environment to
    /// produce this spec's dotfile content: one per `command` entry and one per
    /// template var. Zero when apply would refuse the spec.
    pub apply_commands: usize,
}

/// A dotfile entry and the environment that declares it.
#[derive(Debug, Clone)]
pub struct ScopedDotfile {
    /// The declaring environment, or `None` for a shared entry.
    pub environment: Option<String>,
    pub entry: crate::package::DotfileEntry,
    /// Whether apply refuses the package in the declaring environment, so the
    /// entry would not deploy there.
    pub refused: bool,
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
    pub invalid_packages: Vec<crate::package::port::PackageParseError>,
    /// Packages that parsed and that selfie will not read in the environments the
    /// listing shows: the current one, or any of them under `--all`. Listed
    /// whatever the environment filter, since a refused file cannot say which
    /// environments it declares.
    pub refused: Vec<RefusedSpec>,
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
    pub invalid_packages: Vec<crate::package::port::PackageParseError>,
    /// Specs that parsed and that selfie will not read in the environments the
    /// listing shows: the current one, or any of them under `--all` and in a
    /// search. Listed whatever the filter, as unparsable specs are.
    pub refused: Vec<RefusedSpec>,
    pub current_environment: String,
    pub package_directory: String,
    pub environment_stats: std::collections::HashMap<String, usize>,
    pub show_all: bool,
}

/// Every dotfile entry selfie could read, and where it read them from.
///
/// Carries the packages rather than flattened rows: an adapter renders a
/// dotfile entry from [`content_source`](crate::package::DotfileEntry::content_source),
/// and the terminal wants one sentence where a structured consumer wants the
/// kind and its parts as fields. Flattening here would pick one of those.
///
/// Specs that did not parse are not in this list and are not counted as absent:
/// they leave as [`SpecSkipped`](PackageEvent::SpecSkipped) while the listing
/// runs.
#[derive(Debug, Clone)]
pub struct DotfileListData {
    /// Packages declaring at least one dotfile entry.
    pub packages: Vec<crate::package::Package>,
    /// Packages whose file selfie will not read at the top level.
    ///
    /// Separate from [`packages`](Self::packages) because their entry list is
    /// not short, it is untrustworthy: the key that shadows `dotfiles:` leaves
    /// selfie reading an empty list from a file that declares several. Showing
    /// them as ordinary packages with nothing in them is what sends a user
    /// looking for a dotfile the listing says does not exist.
    pub refused: Vec<RefusedSpec>,
    /// Where the package specs were read from.
    pub package_directory: String,
    /// Where the standalone dotfile specs were read from.
    pub dotfiles_directory: String,
}

/// A package the listing could not trust, and why.
#[derive(Debug, Clone)]
pub struct RefusedSpec {
    /// The package's declared name.
    pub package_name: String,
    /// The file it was read from.
    pub path: std::path::PathBuf,
    /// What selfie objected to, for a consumer that branches on it.
    pub kind: RefusalKind,
    /// What selfie objected to, as a clause for display.
    pub reason: String,
}

impl RefusedSpec {
    pub(crate) fn new(
        package: &crate::package::Package,
        refusal: &crate::package::SpecRefusal,
    ) -> Self {
        Self {
            package_name: package.name().to_string(),
            path: package.path().to_path_buf(),
            kind: refusal.kind(),
            reason: refusal.to_string(),
        }
    }
}

/// Why selfie refused a whole package.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefusalKind {
    /// The file carries top-level keys a package does not accept.
    UnknownTopLevelKeys,
    /// An environment mapping carries keys an environment does not accept.
    UnknownEnvironmentKeys,
    /// The file's top-level keys could not be read back, so an unrecognized one
    /// cannot be ruled out.
    UncheckedTopLevel,
    /// The spec declares no environment.
    NoEnvironments,
    /// Several spec files in one directory claim the package's name, so none of
    /// them is used.
    AmbiguousName,
}

/// A package a [`PackageEvent::PackagesRefused`] names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefusedPackage {
    /// The package's name.
    pub name: String,
    /// The spec file it was read from, or every file claiming the name for
    /// [`RefusalKind::AmbiguousName`].
    pub paths: Vec<std::path::PathBuf>,
}

/// How a deployed target and its source have moved since selfie last deployed
/// it.
///
/// [`Display`](fmt::Display) words it for a person: "repo changed".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DriftType {
    /// The target holds what selfie last deployed, and the source has not
    /// changed since.
    None,
    /// The source changed since the last deploy; the target did not.
    RepoChanged,
    /// The target changed since the last deploy; the source did not.
    TargetChanged,
    /// The source and the target both changed since the last deploy.
    BothChanged,
    /// Selfie has no record of deploying to the target.
    NotTracked,
}

impl std::fmt::Display for DriftType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DriftType::None => write!(f, "none"),
            DriftType::RepoChanged => write!(f, "repo changed"),
            DriftType::TargetChanged => write!(f, "target changed"),
            DriftType::BothChanged => write!(f, "both changed"),
            DriftType::NotTracked => write!(f, "not tracked"),
        }
    }
}

/// Why a dotfile was left as it is.
///
/// [`Display`](fmt::Display) words the reason as a clause for a person.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkipReason {
    /// The target holds what selfie last deployed, and the source has not
    /// changed since.
    UpToDate,
    /// The target already holds what would be written.
    InSync,
    /// A secret-bearing target already held what would be written; only its
    /// permissions were narrowed to owner-only.
    PermissionsTightened,
    /// A dry run: the repository file would have been written.
    DryRun,
    /// A dry run of a secret-bearing entry: nothing ran, so its content and
    /// whether it differs are unknown.
    SecretDryRun {
        /// How many commands the deploy would run.
        commands: usize,
        /// What the deploy would do about a symlink at the target.
        link: LinkAtTarget,
    },
    /// A secret-bearing entry `dotfiles drift` reports without checking, since
    /// checking would run its commands.
    Unverifiable,
}

/// Whether a symlink is at a secret-bearing entry's target, which its deploy
/// would replace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkAtTarget {
    /// No symlink is at the target.
    NoLink,
    /// A symlink is at the target, and this is its destination as the link
    /// spells it.
    To(std::path::PathBuf),
    /// A symlink whose destination could not be read.
    DestinationUnknown,
}

impl fmt::Display for SkipReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UpToDate => f.write_str("already up to date"),
            Self::InSync => f.write_str("already in sync"),
            Self::PermissionsTightened => {
                f.write_str("already in sync (permissions tightened to owner-only)")
            }
            Self::DryRun => f.write_str("dry run"),
            Self::SecretDryRun {
                commands,
                link: LinkAtTarget::NoLink,
            } => write!(
                f,
                "dry run: would run {commands} command(s); content not resolved, so no comparison \
                 is possible"
            ),
            Self::SecretDryRun { commands, link } => {
                let destination = match link {
                    LinkAtTarget::To(path) => format!(" to '{}'", path.display()),
                    LinkAtTarget::NoLink | LinkAtTarget::DestinationUnknown => String::new(),
                };
                write!(
                    f,
                    "dry run: would run {commands} command(s), then replace the symlink{destination} \
                     with a regular file readable only by you"
                )
            }
            Self::Unverifiable => {
                f.write_str("provider-sourced (not verifiable without resolving)")
            }
        }
    }
}

/// Carries one package refused whole, before refusals are grouped by reason.
pub(crate) struct PackageRefusal {
    pub(crate) kind: RefusalKind,
    pub(crate) reason: String,
    pub(crate) package: RefusedPackage,
}

impl From<&RefusedSpec> for PackageRefusal {
    fn from(spec: &RefusedSpec) -> Self {
        Self {
            kind: spec.kind,
            reason: spec.reason.clone(),
            package: RefusedPackage {
                name: spec.package_name.clone(),
                paths: vec![spec.path.clone()],
            },
        }
    }
}

/// `refusals` grouped by kind and reason, in the order each pair first appears,
/// with each group's packages in the order they came.
fn group_refusals(
    refusals: impl IntoIterator<Item = PackageRefusal>,
) -> Vec<(RefusalKind, String, Vec<RefusedPackage>)> {
    let mut groups: Vec<(RefusalKind, String, Vec<RefusedPackage>)> = Vec::new();
    for refusal in refusals {
        match groups
            .iter_mut()
            .find(|(kind, reason, _)| *kind == refusal.kind && *reason == refusal.reason)
        {
            Some((_, _, packages)) => packages.push(refusal.package),
            None => groups.push((refusal.kind, refusal.reason, vec![refusal.package])),
        }
    }
    groups
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

/// What a check that ran found.
///
/// Carries no stdout, since a check command may print a credential. The full
/// output travels only in [`PackageEvent::CheckResultCompleted`].
#[derive(Debug, Clone)]
pub enum CheckVerdict {
    /// The check command succeeded.
    Installed,
    /// The check command ran and exited non-zero.
    NotInstalled {
        command: String,
        exit_code: Option<i32>,
        stderr: crate::commands::BoundedText,
    },
}

impl std::fmt::Display for CheckVerdict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CheckVerdict::Installed => f.write_str("successfully"),
            CheckVerdict::NotInstalled {
                exit_code: Some(code),
                ..
            } => write!(f, "and found it not installed (exit code {code})"),
            CheckVerdict::NotInstalled { .. } => f.write_str("and found it not installed"),
        }
    }
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

impl ValidationStatus {
    /// How a validation with this status scores; see [`Outcome`].
    #[must_use]
    pub fn outcome(&self) -> Outcome {
        match self {
            ValidationStatus::Valid => Outcome::Clean,
            ValidationStatus::HasWarnings => Outcome::Found,
            ValidationStatus::HasErrors => Outcome::Failed,
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

    // The CLI prints these sentences for a skipped entry, so each is pinned.
    #[test]
    fn a_skip_reason_reads_as_a_sentence() {
        let cases = [
            (SkipReason::UpToDate, "already up to date"),
            (SkipReason::InSync, "already in sync"),
            (
                SkipReason::PermissionsTightened,
                "already in sync (permissions tightened to owner-only)",
            ),
            (SkipReason::DryRun, "dry run"),
            (
                SkipReason::SecretDryRun {
                    commands: 2,
                    link: LinkAtTarget::NoLink,
                },
                "dry run: would run 2 command(s); content not resolved, so no comparison is possible",
            ),
            (
                SkipReason::SecretDryRun {
                    commands: 1,
                    link: LinkAtTarget::To("/etc/real".into()),
                },
                "dry run: would run 1 command(s), then replace the symlink to '/etc/real' with a \
                 regular file readable only by you",
            ),
            (
                SkipReason::SecretDryRun {
                    commands: 1,
                    link: LinkAtTarget::DestinationUnknown,
                },
                "dry run: would run 1 command(s), then replace the symlink with a regular file \
                 readable only by you",
            ),
            (
                SkipReason::Unverifiable,
                "provider-sourced (not verifiable without resolving)",
            ),
        ];
        for (reason, sentence) in cases {
            assert_eq!(reason.to_string(), sentence, "{reason:?}");
        }
    }

    #[test]
    fn test_step_count_usize_usage() {
        // Test direct usize construction
        let step_count = StepCount::new(7usize, 10usize);
        assert_eq!(step_count.completed, 7);
        assert_eq!(step_count.total, 10);

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
            verdict: CheckVerdict::Installed,
            steps_completed: step_count,
        };

        assert_eq!(
            format!("{success}"),
            "Package 'test-package' check completed successfully in environment 'test'"
        );
    }

    // An install that found the package names where, after the environment, so
    // the clause is not read as part of the path.
    #[test]
    fn an_already_installed_package_names_its_path_last() {
        let message = OperationSuccess::PackageInstalled {
            package_name: "bat".to_string(),
            environment: "mac".to_string(),
            was_already_installed: true,
            executable_path: Some("/opt/bin/bat".to_string()),
            steps_completed: StepCount::new(4, 4),
        }
        .to_string();

        assert_eq!(
            message,
            "Package 'bat' was already installed in environment 'mac', at /opt/bin/bat"
        );
    }

    // Every variant's message, with a step count no other field could produce:
    // none may carry the count, and exactly the variants whose answer depends on
    // the environment name it.
    #[test]
    fn a_message_names_the_environment_it_depends_on_and_no_step_count() {
        let steps = StepCount::new(7, 9);
        let name = || "pkg".to_string();
        let env = || "env-x".to_string();
        let path = || std::path::PathBuf::from("/p/pkg.yaml");
        // (message, names the environment)
        let cases: Vec<(OperationSuccess, bool)> = vec![
            (
                OperationSuccess::PackageChecked {
                    package_name: name(),
                    environment: env(),
                    verdict: CheckVerdict::Installed,
                    steps_completed: steps,
                },
                true,
            ),
            (
                OperationSuccess::PackagesAudited {
                    audited_count: 1,
                    conflict_count: 2,
                    not_installed_count: 3,
                    error_count: 4,
                    refused_count: 5,
                    environment: env(),
                    steps_completed: steps,
                },
                true,
            ),
            (
                OperationSuccess::PackageAudited {
                    package_name: name(),
                    environment: env(),
                    audit_result: AuditResult::NoAuditCommand,
                    steps_completed: steps,
                },
                true,
            ),
            (
                OperationSuccess::PackageInstalled {
                    package_name: name(),
                    environment: env(),
                    was_already_installed: true,
                    executable_path: Some("/bin/pkg".to_string()),
                    steps_completed: steps,
                },
                true,
            ),
            (
                OperationSuccess::PackageValidated {
                    package_name: name(),
                    environment: env(),
                    status: ValidationStatus::HasErrors,
                    error_count: 2,
                    warning_count: Some(1),
                    steps_completed: steps,
                },
                true,
            ),
            // Each status writes its own message, so each gets a row.
            (
                OperationSuccess::PackageValidated {
                    package_name: name(),
                    environment: env(),
                    status: ValidationStatus::HasWarnings,
                    error_count: 0,
                    warning_count: Some(1),
                    steps_completed: steps,
                },
                true,
            ),
            (
                OperationSuccess::PackageValidated {
                    package_name: name(),
                    environment: env(),
                    status: ValidationStatus::Valid,
                    error_count: 0,
                    warning_count: None,
                    steps_completed: steps,
                },
                true,
            ),
            (
                OperationSuccess::SpecInfoRetrieved {
                    package_name: name(),
                    environment: env(),
                    steps_completed: steps,
                },
                false,
            ),
            (
                OperationSuccess::PackageStatusChecked {
                    package_name: name(),
                    environment: env(),
                    steps_completed: steps,
                },
                true,
            ),
            (
                OperationSuccess::PackageListGenerated {
                    valid_count: 1,
                    invalid_count: 2,
                    refused_specs: 3,
                    environment: env(),
                    steps_completed: steps,
                },
                true,
            ),
            (
                OperationSuccess::PackageCreated {
                    package_name: name(),
                    file_path: path(),
                    environment: env(),
                    steps_completed: steps,
                },
                false,
            ),
            (
                OperationSuccess::PackageUpdated {
                    package_name: name(),
                    environment: env(),
                    steps_completed: steps,
                },
                false,
            ),
            (
                OperationSuccess::PackageRemoved {
                    package_name: name(),
                    file_path: path(),
                    environment: env(),
                    dependent_packages: vec!["dep".to_string()],
                    steps_completed: steps,
                },
                false,
            ),
            (
                OperationSuccess::SpecListGenerated {
                    valid_count: 1,
                    invalid_count: 2,
                    refused_specs: 3,
                    environment: env(),
                    steps_completed: steps,
                },
                true,
            ),
            (
                OperationSuccess::SpecsValidated {
                    validated_count: 1,
                    error_count: 2,
                    unparsable_count: 3,
                    uncollected_count: 4,
                    warning_count: 5,
                    other_warning_count: 6,
                    environment: env(),
                    steps_completed: steps,
                },
                true,
            ),
            (
                OperationSuccess::DotfilesApplied {
                    deployed_count: 1,
                    skipped_count: 2,
                    conflict_count: 3,
                    refused_count: 4,
                    orphan_count: 5,
                    environment: env(),
                    steps_completed: steps,
                },
                true,
            ),
            (
                OperationSuccess::DotfileDriftChecked {
                    drift_count: 1,
                    total_count: 2,
                    refused_count: 3,
                    unverified_count: 4,
                    orphan_count: 5,
                    unjudged_count: 6,
                    environment: env(),
                    steps_completed: steps,
                },
                true,
            ),
            (
                OperationSuccess::DotfileTracked {
                    name: name(),
                    source_path: path(),
                    target_path: "~/.pkg".to_string(),
                    was_already_tracked: false,
                    environment: env(),
                    steps_completed: steps,
                },
                false,
            ),
            (
                OperationSuccess::SyncPushComplete {
                    commits_pushed: 2,
                    steps_completed: steps,
                },
                false,
            ),
            (
                OperationSuccess::SyncPullUpToDate {
                    steps_completed: steps,
                },
                false,
            ),
            (
                OperationSuccess::SyncNothingToPush {
                    steps_completed: steps,
                },
                false,
            ),
        ];

        for (success, names_environment) in cases {
            let message = success.to_string();
            assert!(
                !message.contains("steps") && !message.contains("7/9"),
                "{message}"
            );
            assert_eq!(
                message.contains("in environment 'env-x'"),
                names_environment,
                "{message}"
            );
        }
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
            "Package 'test-package' installation completed successfully in environment 'test'"
        );
    }

    // Compile-time exhaustiveness guard: if a new `PackageError` variant is added,
    // this match will fail to compile, reminding you to update
    // `OperationFailure::is_environment_error()` and `is_package_error()`.
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
                | PackageError::UnreadableFile { .. }
                | PackageError::UnusableName { .. }
                | PackageError::PackageAlreadyExists { .. }
                | PackageError::PackagePathOccupied { .. } => "package",
            }
        }
        // The exhaustive match above is the real test — it forces a compile
        // error when new variants are added to PackageError.
        let _ = categorize;
    }

    fn drift(drift: usize, orphan: usize, refused: usize, unverified: usize) -> OperationSuccess {
        OperationSuccess::DotfileDriftChecked {
            drift_count: drift,
            total_count: 3,
            refused_count: refused,
            unverified_count: unverified,
            orphan_count: orphan,
            unjudged_count: 0,
            environment: "test".to_string(),
            steps_completed: StepCount::new(1, 1),
        }
    }

    fn applied(conflict: usize, orphan: usize, refused: usize) -> OperationSuccess {
        OperationSuccess::DotfilesApplied {
            deployed_count: 1,
            skipped_count: 0,
            conflict_count: conflict,
            refused_count: refused,
            orphan_count: orphan,
            environment: "test".to_string(),
            steps_completed: StepCount::new(1, 1),
        }
    }

    fn audited(result: AuditResult) -> OperationSuccess {
        OperationSuccess::PackageAudited {
            package_name: "p".to_string(),
            environment: "test".to_string(),
            audit_result: result,
            steps_completed: StepCount::new(1, 1),
        }
    }

    fn validated(status: ValidationStatus) -> OperationSuccess {
        OperationSuccess::PackageValidated {
            package_name: "p".to_string(),
            environment: "test".to_string(),
            status,
            error_count: 0,
            warning_count: None,
            steps_completed: StepCount::new(1, 1),
        }
    }

    fn specs_validated(errors: usize, warnings: usize) -> OperationSuccess {
        OperationSuccess::SpecsValidated {
            validated_count: 3,
            error_count: errors,
            unparsable_count: 0,
            uncollected_count: 0,
            warning_count: warnings,
            other_warning_count: 0,
            environment: "test".to_string(),
            steps_completed: StepCount::new(1, 1),
        }
    }

    fn audited_all(
        conflicts: usize,
        not_installed: usize,
        errors: usize,
        refused: usize,
    ) -> OperationSuccess {
        OperationSuccess::PackagesAudited {
            audited_count: 4,
            conflict_count: conflicts,
            not_installed_count: not_installed,
            error_count: errors,
            refused_count: refused,
            environment: "test".to_string(),
            steps_completed: StepCount::new(1, 1),
        }
    }

    // Each counted variant along every axis that scores it, plus a mixed case
    // per variant so the precedence is pinned, not just each axis alone.
    #[test]
    fn each_success_scores_by_what_it_found() {
        let cases = [
            ("drift clean", drift(0, 0, 0, 0), Outcome::Clean),
            ("drift unverified only", drift(0, 0, 0, 2), Outcome::Clean),
            ("drift drifted", drift(1, 0, 0, 0), Outcome::Found),
            ("drift orphan", drift(0, 1, 0, 0), Outcome::Found),
            ("drift refused", drift(0, 0, 1, 0), Outcome::Failed),
            (
                "drift with unjudged records",
                OperationSuccess::DotfileDriftChecked {
                    drift_count: 0,
                    total_count: 3,
                    refused_count: 0,
                    unverified_count: 0,
                    orphan_count: 0,
                    unjudged_count: 2,
                    environment: "test".to_string(),
                    steps_completed: StepCount::new(1, 1),
                },
                Outcome::Found,
            ),
            (
                "drift refused and drifted",
                drift(2, 1, 1, 0),
                Outcome::Failed,
            ),
            ("apply clean", applied(0, 0, 0), Outcome::Clean),
            ("apply conflict", applied(1, 0, 0), Outcome::Clean),
            ("apply orphan", applied(0, 1, 0), Outcome::Clean),
            ("apply refused", applied(0, 0, 1), Outcome::Failed),
            (
                "apply refused and conflicted",
                applied(1, 1, 1),
                Outcome::Failed,
            ),
            (
                "audit clean",
                audited(AuditResult::Clean {
                    sources: vec!["brew".to_string()],
                }),
                Outcome::Clean,
            ),
            (
                "audit conflict",
                audited(AuditResult::Conflicts {
                    sources: vec!["npm".to_string()],
                    expected: vec!["brew".to_string()],
                }),
                Outcome::Found,
            ),
            (
                "audit not installed",
                audited(AuditResult::NotInstalled),
                Outcome::Found,
            ),
            (
                "audit error",
                audited(AuditResult::Error("boom".to_string())),
                Outcome::Failed,
            ),
            (
                "audit no command",
                audited(AuditResult::NoAuditCommand),
                Outcome::Failed,
            ),
            (
                "validated",
                validated(ValidationStatus::Valid),
                Outcome::Clean,
            ),
            (
                "validated with warnings",
                validated(ValidationStatus::HasWarnings),
                Outcome::Found,
            ),
            (
                "validated with errors",
                validated(ValidationStatus::HasErrors),
                Outcome::Failed,
            ),
            ("all validated", specs_validated(0, 0), Outcome::Clean),
            (
                "all validated with warnings about the run",
                OperationSuccess::specs_validated(
                    3,
                    0,
                    0,
                    0,
                    0,
                    1,
                    "test".to_string(),
                    StepCount::new(1, 1),
                ),
                Outcome::Found,
            ),
            // A spec that could not be read, or a name several files claim, fails
            // the run like a spec with errors.
            (
                "a spec could not be read",
                OperationSuccess::specs_validated(
                    3,
                    0,
                    1,
                    0,
                    0,
                    0,
                    "test".to_string(),
                    StepCount::new(1, 1),
                ),
                Outcome::Failed,
            ),
            (
                "a name several files claim",
                OperationSuccess::specs_validated(
                    3,
                    0,
                    0,
                    1,
                    0,
                    0,
                    "test".to_string(),
                    StepCount::new(1, 1),
                ),
                Outcome::Failed,
            ),
            (
                "all validated with warnings",
                specs_validated(0, 2),
                Outcome::Found,
            ),
            (
                "all validated with errors",
                specs_validated(1, 2),
                Outcome::Failed,
            ),
            ("audit all clean", audited_all(0, 0, 0, 0), Outcome::Clean),
            (
                "audit all conflict",
                audited_all(1, 0, 0, 0),
                Outcome::Found,
            ),
            (
                "audit all not installed",
                audited_all(0, 1, 0, 0),
                Outcome::Found,
            ),
            ("audit all error", audited_all(0, 0, 1, 0), Outcome::Failed),
            (
                "audit all refused",
                audited_all(0, 0, 0, 1),
                Outcome::Failed,
            ),
            (
                "audit all refused and conflicted",
                audited_all(1, 1, 0, 1),
                Outcome::Failed,
            ),
            (
                "check installed",
                OperationSuccess::package_checked(
                    "p".to_string(),
                    "test".to_string(),
                    CheckVerdict::Installed,
                    StepCount::new(1, 1),
                ),
                Outcome::Clean,
            ),
            (
                "check not installed",
                OperationSuccess::package_checked(
                    "p".to_string(),
                    "test".to_string(),
                    CheckVerdict::NotInstalled {
                        command: "false".to_string(),
                        exit_code: Some(1),
                        stderr: crate::commands::BoundedText::bound(b""),
                    },
                    StepCount::new(1, 1),
                ),
                Outcome::Found,
            ),
            (
                "generic",
                OperationSuccess::Generic("done".to_string()),
                Outcome::Clean,
            ),
        ];
        for (name, success, expected) in cases {
            assert_eq!(success.outcome(), expected, "{name}");
            assert_eq!(
                OperationResult::Success(success).outcome(),
                expected,
                "{name}, through OperationResult"
            );
        }
    }

    #[test]
    fn every_failure_is_failed() {
        let failure = OperationResult::Failure(OperationFailure::Generic("no".to_string()));
        assert_eq!(failure.outcome(), Outcome::Failed);
    }

    fn refusal(kind: RefusalKind, reason: &str, name: &str) -> PackageRefusal {
        PackageRefusal {
            kind,
            reason: reason.to_string(),
            package: RefusedPackage {
                name: name.to_string(),
                paths: vec![std::path::PathBuf::from(format!("/p/{name}.yml"))],
            },
        }
    }

    fn names(packages: &[RefusedPackage]) -> Vec<&str> {
        packages.iter().map(|p| p.name.as_str()).collect()
    }

    // Packages refused for one reason form one group, whatever lies between
    // them, and groups come in the order each reason first appears. The reason
    // shared by three packages comes second, so a grouping that put the larger
    // group first would fail.
    #[test]
    fn refusals_group_by_reason_in_first_seen_order() {
        let groups = group_refusals([
            refusal(
                RefusalKind::UnknownTopLevelKeys,
                "unknown field `audt`",
                "a",
            ),
            refusal(
                RefusalKind::UnknownTopLevelKeys,
                "unknown field `version`",
                "b",
            ),
            refusal(
                RefusalKind::UnknownTopLevelKeys,
                "unknown field `version`",
                "c",
            ),
            refusal(
                RefusalKind::UnknownTopLevelKeys,
                "unknown field `audt`",
                "d",
            ),
            refusal(
                RefusalKind::UnknownTopLevelKeys,
                "unknown field `version`",
                "e",
            ),
        ]);

        assert_eq!(groups.len(), 2, "{groups:?}");
        assert_eq!(groups[0].1, "unknown field `audt`");
        assert_eq!(names(&groups[0].2), ["a", "d"]);
        assert_eq!(groups[1].1, "unknown field `version`");
        assert_eq!(names(&groups[1].2), ["b", "c", "e"]);
    }

    // One sentence under two kinds is two groups, so a consumer branching on the
    // kind never sees a package filed under another's.
    #[test]
    fn one_reason_under_two_kinds_is_two_groups() {
        let groups = group_refusals([
            refusal(RefusalKind::UnknownTopLevelKeys, "same words", "a"),
            refusal(RefusalKind::NoEnvironments, "same words", "b"),
        ]);

        assert_eq!(groups.len(), 2, "{groups:?}");
        assert_eq!(groups[0].0, RefusalKind::UnknownTopLevelKeys);
        assert_eq!(groups[1].0, RefusalKind::NoEnvironments);
    }
}
