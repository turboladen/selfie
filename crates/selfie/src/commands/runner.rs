//! Command execution abstractions and types
//!
//! This module provides the core abstractions for executing system commands
//! in a cross-platform manner. It implements the Command Runner port pattern
//! to allow different command execution strategies while maintaining a consistent interface.

use std::{
    borrow::Cow,
    future::Future,
    path::{Path, PathBuf},
    process::Output,
    sync::Arc,
    time::Duration,
};

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// A chunk of output from a running command
///
/// Represents either stdout or stderr output from a command execution.
/// This allows for streaming output processing and distinguishing between
/// standard output and error streams.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum OutputChunk {
    /// Standard output content
    Stdout(String),
    /// Standard error content
    Stderr(String),
}

/// Port for command execution, in buffered and streaming forms.
///
/// A non-zero exit is **not** an error for the `execute*` methods. It is
/// reported through [`CommandOutput::is_success`], so their `# Errors` sections
/// list only the ways a command fails to run to completion. Those ways are the
/// same for all of them: its working directory cannot be entered
/// ([`CommandError::WorkingDirectoryUnusable`], and then no part of it runs), it
/// cannot be started or dies part-way through, it times out, it is canceled
/// via `token`, or an output stream cannot be read to the end
/// ([`CommandError::OutputReadFailed`]). The last includes stderr on a command
/// whose correctness depends only on stdout — the runner reports what it could
/// not read, and does not decide which stream a caller cared about. Each
/// `execute*` method below names only what it adds to that set.
///
/// They all buffer the command's entire output in memory, and nothing bounds it
/// — [`execute_streaming`](CommandRunner::execute_streaming) accumulates the
/// output as well as relaying it. A size check applied by a caller, such as the
/// dotfile content cap, bounds what selfie compares and writes, not what it
/// allocates.
///
/// [`is_command_available`](CommandRunner::is_command_available) is the
/// exception to both: it runs no command.
#[cfg_attr(any(test, feature = "with_mocks"), mockall::automock)]
pub trait CommandRunner: Send + Sync {
    /// Whether `command` exists as a filesystem executable on `PATH`.
    ///
    /// Shell builtins are not detected. Use it to confirm a package manager is
    /// present — `brew`, `npm`, `apt` — before trying to install with it.
    fn is_command_available(&self, command: &str) -> impl Future<Output = bool> + Send;

    /// Run a command in `working_dir` to completion, buffering its output.
    ///
    /// For commands that produce little output and need no real-time feedback.
    /// The timeout is whatever the implementation defaults to; use
    /// [`execute_in_dir`](CommandRunner::execute_in_dir) to set one.
    ///
    /// # Errors
    ///
    /// [`CommandError`], for any of the failures listed on the trait.
    fn execute(
        &self,
        command: &str,
        working_dir: &Path,
        token: &CancellationToken,
    ) -> impl Future<Output = Result<CommandOutput, CommandError>> + Send;

    /// Like [`execute`](CommandRunner::execute), with an explicit timeout. The
    /// command is terminated if it does not finish within `timeout`. The error
    /// is [`CommandError::Timeout`] with [`TimeLimit::Total`].
    ///
    /// # Errors
    ///
    /// [`CommandError`], for any of the failures listed on the trait.
    fn execute_in_dir(
        &self,
        command: &str,
        working_dir: &Path,
        timeout: Duration,
        token: &CancellationToken,
    ) -> impl Future<Output = Result<CommandOutput, CommandError>> + Send;

    /// Run a command in `working_dir`, relaying stdout and stderr through
    /// `output_sender` as they arrive.
    ///
    /// For long-running commands that need real-time feedback. Chunks are
    /// best-effort: an implementation may drop one rather than block when the
    /// receiver falls behind, and drops it outright once the receiver is gone,
    /// so what arrives on the channel is not guaranteed to be the whole output.
    /// The returned [`CommandOutput`] holds all of it.
    ///
    /// `timeout` bounds silence, not run time: the command is terminated once
    /// it has written nothing to stdout or stderr for `timeout`, and a command
    /// that keeps writing is never terminated by it. The error is
    /// [`CommandError::Timeout`] with [`TimeLimit::Silence`].
    ///
    /// # Errors
    ///
    /// [`CommandError`], for any of the failures listed on the trait, plus
    /// failure to capture stdout or stderr from the child.
    fn execute_streaming(
        &self,
        command: &str,
        working_dir: &Path,
        timeout: Duration,
        output_sender: mpsc::Sender<OutputChunk>,
        token: &CancellationToken,
    ) -> impl Future<Output = Result<CommandOutput, CommandError>> + Send;

    /// Execute a command whose stdout becomes a file's content.
    ///
    /// Like [`execute_in_dir`](CommandRunner::execute_in_dir), except that the
    /// captured stdout is the command's **own**: an implementation running
    /// commands through a shell must return nothing the shell, the profile it
    /// sources, or a process either started wrote to the stdout the command
    /// inherited. Separate method and separate type so the difference is a
    /// compile error rather than a convention.
    ///
    /// # Errors
    ///
    /// Returns [`CommandError`] for everything `execute_in_dir` does, plus
    /// [`CommandError::ContentMarkersAbsent`] when the command **succeeded** but
    /// its output could not be told from the shell's — content that might carry a
    /// foreign prefix is not content. A command that *failed* is reported as a
    /// failure instead, so its stderr survives.
    fn execute_for_content(
        &self,
        command: &str,
        working_dir: &Path,
        timeout: Duration,
        token: &CancellationToken,
    ) -> impl Future<Output = Result<ContentOutput, CommandError>> + Send;
}

/// Result of executing a command
///
/// Contains the complete output and metadata from a command execution,
/// including exit status, stdout, stderr, and execution duration.
#[derive(Debug, Clone, PartialEq)]
pub struct CommandOutput {
    /// The process output containing exit status and output streams
    pub(crate) output: Output,

    /// How long the command took to execute
    pub(crate) duration: Duration,
}

impl CommandOutput {
    /// Build a `CommandOutput` from its parts.
    ///
    /// For test doubles that stand in for a real runner. Gated behind
    /// `with_mocks` so that production code cannot fabricate the result of a
    /// command that never ran.
    #[cfg(feature = "with_mocks")]
    #[must_use]
    pub fn from_parts(
        status: std::process::ExitStatus,
        stdout: Vec<u8>,
        stderr: Vec<u8>,
        duration: Duration,
    ) -> Self {
        Self {
            output: Output {
                status,
                stdout,
                stderr,
            },
            duration,
        }
    }

    /// Get the command's exit code
    ///
    /// Returns the exit status code of the command, or -1 if the exit code
    /// cannot be determined (e.g., the process was terminated by a signal).
    #[must_use]
    pub fn exit_code(&self) -> i32 {
        self.output.status.code().unwrap_or(-1)
    }

    /// Get the raw stdout bytes
    ///
    /// Returns the complete stdout output as a byte slice. Use [`stdout_str`](Self::stdout_str)
    /// for UTF-8 string representation.
    #[must_use]
    pub fn stdout(&self) -> &[u8] {
        &self.output.stdout
    }

    /// Take ownership of the stdout bytes, consuming the output.
    ///
    /// For a caller that keeps stdout rather than reading it in place.
    /// [`stdout`](Self::stdout) plus `to_vec` leaves both copies alive at once,
    /// which on the dotfile provider path means the whole credential exists twice
    /// at peak — and a second buffer is a second thing to reason about for
    /// zeroization. Nothing bounds either copy: the runner buffers a command's
    /// entire output before any caller's size cap can run, so the saving is
    /// 2×N → N for arbitrary N, not a fixed amount.
    #[must_use]
    pub fn into_stdout(self) -> Vec<u8> {
        self.output.stdout
    }

    /// Get stdout as a UTF-8 string
    ///
    /// Converts stdout bytes to a string, replacing invalid UTF-8 sequences
    /// with replacement characters. Always succeeds.
    #[must_use]
    pub fn stdout_str(&self) -> Cow<'_, str> {
        String::from_utf8_lossy(&self.output.stdout)
    }

    /// Get the raw stderr bytes
    ///
    /// Returns the complete stderr output as a byte slice. Use [`stderr_str`](Self::stderr_str)
    /// for UTF-8 string representation.
    #[must_use]
    pub fn stderr(&self) -> &[u8] {
        &self.output.stderr
    }

    /// Get stderr as a UTF-8 string
    ///
    /// Converts stderr bytes to a string, replacing invalid UTF-8 sequences
    /// with replacement characters. Always succeeds.
    #[must_use]
    pub fn stderr_str(&self) -> Cow<'_, str> {
        String::from_utf8_lossy(&self.output.stderr)
    }

    /// Check if the command executed successfully
    ///
    /// Returns `true` if the command exited with status code 0, `false` otherwise.
    #[must_use]
    pub fn is_success(&self) -> bool {
        self.output.status.success()
    }
}

/// What a command produced when its stdout is destined for a file.
///
/// Distinct from [`CommandOutput`] because the two carry different guarantees
/// about the same bytes: this one's stdout is the command's own. It must not
/// wrap or expose a `CommandOutput`, or the unseparated capture is back within
/// reach of the resolve path.
///
/// Holds a credential. It is compared and written, never recorded or rendered.
pub struct ContentOutput {
    /// The command's own stdout.
    stdout: Vec<u8>,

    /// The command's stderr, forwarded on failure only.
    ///
    /// **Not separated**: a profile writes to stderr through the same inherited
    /// descriptor and selfie cannot tell those bytes apart.
    stderr: Vec<u8>,

    /// Whether the command exited zero.
    success: bool,

    /// Bytes that reached the capture descriptor before the command's output.
    ///
    /// Should be zero — the shell's own output goes elsewhere — so a non-zero
    /// value means something wrote where only the command should be able to.
    discarded_before: usize,

    /// Whether the end of the command's output was identified.
    ///
    /// False means selfie captured the command's output but could not establish
    /// where it stopped, so anything the shell wrote afterwards — an exit trap
    /// the command itself installed, for instance — is part of `stdout`. The
    /// content is still returned; the caller reports the uncertainty.
    tail_verified: bool,
}

/// Prints lengths, never content. **Never derive this**: a derived `Debug` is an
/// exit for the credential, opened by the first `{:?}` or `unwrap()` on a
/// `Result<ContentOutput, _>`.
impl std::fmt::Debug for ContentOutput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ContentOutput")
            .field("stdout", &format_args!("<{} bytes>", self.stdout.len()))
            .field("stderr", &format_args!("<{} bytes>", self.stderr.len()))
            .field("success", &self.success)
            .field("discarded_before", &self.discarded_before)
            .field("tail_verified", &self.tail_verified)
            .finish()
    }
}

impl ContentOutput {
    /// Build a `ContentOutput` from what a runner captured.
    ///
    /// Crate-internal: the shell adapter is the only production caller, being the
    /// only code that can separate a command's output from its shell's.
    pub(crate) fn from_capture(
        success: bool,
        stdout: Vec<u8>,
        stderr: Vec<u8>,
        discarded_before: usize,
        tail_verified: bool,
    ) -> Self {
        Self {
            stdout,
            stderr,
            success,
            discarded_before,
            tail_verified,
        }
    }

    /// Build a `ContentOutput` from its parts.
    ///
    /// For test doubles that stand in for a real runner, and gated for the same
    /// reason [`CommandOutput::from_parts`] is: production code must not be able
    /// to declare that a command's output was separated when no command ran.
    #[cfg(feature = "with_mocks")]
    #[must_use]
    pub fn from_parts(
        success: bool,
        stdout: Vec<u8>,
        stderr: Vec<u8>,
        discarded_before: usize,
        tail_verified: bool,
    ) -> Self {
        Self::from_capture(success, stdout, stderr, discarded_before, tail_verified)
    }

    /// Whether the command exited zero.
    #[must_use]
    pub fn is_success(&self) -> bool {
        self.success
    }

    /// Take the command's own stdout.
    ///
    /// Consuming rather than borrowing, so a credential is not left alive in two
    /// buffers at once — see [`CommandOutput::into_stdout`].
    #[must_use]
    pub fn into_stdout(self) -> Vec<u8> {
        self.stdout
    }

    /// The command's stderr.
    #[must_use]
    pub fn stderr(&self) -> &[u8] {
        &self.stderr
    }

    /// How many bytes reached the capture channel before the command's output.
    #[must_use]
    pub fn discarded_before(&self) -> usize {
        self.discarded_before
    }

    /// Whether the end of the command's own output was identified.
    #[must_use]
    pub fn tail_verified(&self) -> bool {
        self.tail_verified
    }
}

/// Which of a command's two output streams something happened to.
///
/// Carried by [`CommandError::OutputReadFailed`] so a failure names the pipe it
/// happened on. Deliberately a tag and nothing more — it holds no bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputStream {
    /// The command's standard output.
    Stdout,
    /// The command's standard error.
    Stderr,
}

impl std::fmt::Display for OutputStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Stdout => "stdout",
            Self::Stderr => "stderr",
        })
    }
}

/// A command that could not enter the directory it was to run in: it is
/// missing, not a directory, or not searchable. No part of the command ran.
///
/// Its sentence names the command, the directory and why.
#[derive(Debug, Clone)]
pub struct UnusableDirectory {
    command: String,
    working_directory: PathBuf,
    cause: Arc<std::io::Error>,
}

impl UnusableDirectory {
    /// Records that `command` could not enter `working_directory`, for `cause`.
    #[must_use]
    pub fn new(
        command: impl Into<String>,
        working_directory: impl Into<PathBuf>,
        cause: Arc<std::io::Error>,
    ) -> Self {
        Self {
            command: command.into(),
            working_directory: working_directory.into(),
            cause,
        }
    }

    /// The command that could not run.
    #[must_use]
    pub fn command(&self) -> &str {
        &self.command
    }

    /// The directory it could not enter.
    #[must_use]
    pub fn working_directory(&self) -> &Path {
        &self.working_directory
    }

    /// Why the directory could not be entered, such as a missing directory or
    /// a denied search.
    #[must_use]
    pub fn cause(&self) -> &std::io::Error {
        &self.cause
    }
}

crate::parts::display_from_parts!(UnusableDirectory);

impl crate::parts::Composed for UnusableDirectory {
    // The command is the spec's own and the directory a path, so each is a part
    // of its own. Either can be long, and so can the cause, so the three share
    // the bound one diagnostic keeps.
    fn parts(&self) -> crate::parts::Parts<'_> {
        let cause = crate::parts::cause_words(&*self.cause);
        let [command, directory, why] = crate::parts::shares(
            [
                self.command.len(),
                // Measured as shown: a byte that is not UTF-8 shows as U+FFFD.
                self.working_directory.to_string_lossy().len(),
                cause.len(),
            ],
            MAX_BOUNDED_BYTES,
        );
        crate::parts::Parts::new()
            .text("Cannot run '")
            .spec_command_within(&self.command, command)
            .text("' in ")
            .path_within(&self.working_directory, directory)
            .text(": ")
            .cause_within(&cause, why)
    }
}

/// Which `command_timeout` limit stopped a command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimeLimit {
    /// The command ran for `command_timeout` in total.
    Total,
    /// The command wrote nothing to stdout or stderr for `command_timeout`.
    Silence,
}

/// A command selfie stopped at one of the two `command_timeout` limits, named by [`TimeLimit`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TimedOut {
    command: String,
    timeout: Duration,
    limit: TimeLimit,
}

impl TimedOut {
    /// Records that `command` was stopped after `timeout`, by `limit`.
    #[must_use]
    pub fn new(command: impl Into<String>, timeout: Duration, limit: TimeLimit) -> Self {
        Self {
            command: command.into(),
            timeout,
            limit,
        }
    }

    /// The command that was stopped.
    #[must_use]
    pub fn command(&self) -> &str {
        &self.command
    }

    /// How long it ran before it was stopped.
    #[must_use]
    pub fn timeout(&self) -> Duration {
        self.timeout
    }
}

crate::parts::display_from_parts!(TimedOut);

impl crate::parts::Composed for TimedOut {
    fn parts(&self) -> crate::parts::Parts<'_> {
        timed_out_parts(&self.command, self.timeout, self.limit)
    }
}

// The one place this sentence is written: `TimedOut` and `CommandError::Timeout`
// both render it, so every path a timeout reaches the user by says the same.
// Every runner selfie builds takes its timeout from `command_timeout`. The
// command is the spec's own, and its part bounds it, since it is unbounded and
// every adapter renders the sentence.
fn timed_out_parts(command: &str, timeout: Duration, limit: TimeLimit) -> crate::parts::Parts<'_> {
    // `command_timeout` is whole seconds, so only a test builds anything else.
    let (amount, unit) = if timeout.subsec_nanos() == 0 {
        (timeout.as_secs(), "second")
    } else {
        (
            u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX),
            "millisecond",
        )
    };
    let plural = if amount == 1 { "" } else { "s" };
    let what = match limit {
        TimeLimit::Total => "timed out after",
        TimeLimit::Silence => "produced no output for",
    };
    crate::parts::Parts::new()
        .text("`")
        .spec_command(command)
        .text(format!(
            "` {what} {amount} {unit}{plural}, the limit `command_timeout` sets"
        ))
}

/// Errors that can occur during command execution
///
/// Represents all possible failure modes when executing system commands,
/// providing detailed context for debugging and error handling.
#[derive(Debug, Clone)]
pub enum CommandError {
    /// Command execution exceeded its time limit, which `limit` names
    Timeout {
        command: String,
        timeout: Duration,
        limit: TimeLimit,
        working_directory: PathBuf,
    },

    /// IO error occurred while running the command
    IoError {
        command: String,
        working_directory: PathBuf,
        cause: Arc<std::io::Error>,
    },

    /// The program that runs the command, normally a shell, could not be
    /// started. No part of the command ran.
    SpawnFailed {
        command: String,
        program: String,
        working_directory: PathBuf,
        cause: Arc<std::io::Error>,
    },

    /// The directory the command was to run in could not be entered. No part of
    /// the command ran.
    WorkingDirectoryUnusable(UnusableDirectory),

    /// One of the command's output pipes could not be read to the end.
    ///
    /// Whatever had been buffered when the read failed is **not** the command's
    /// output, so it is dropped rather than returned. Nothing distinguishes "the
    /// command produced this" from "the pipe failed and this is what we got",
    /// and callers act on that output: one uses it as an executable path, one
    /// writes it to a credentials file, and two read a verdict out of it.
    ///
    /// Reported for stderr as well as stdout, including on commands whose
    /// correctness depends only on stdout.
    // Being lenient about stderr would need either a per-stream flag on
    // `CommandOutput` — the ignorable-by-default shape this variant exists to
    // avoid — or a selfie-authored marker spliced into bytes every consumer
    // treats as the process's own, which would then reach the CLI and the MCP
    // server's JSON as if the command had emitted it.
    //
    // Carries no output bytes by construction, which
    // `OperationFailure::from(CommandError)` depends on and this variant's
    // `Display` has to satisfy.
    OutputReadFailed {
        command: String,
        working_directory: PathBuf,
        stream: OutputStream,
        cause: Arc<std::io::Error>,
    },

    /// Command was canceled via a cancellation token
    Canceled {
        command: String,
        working_directory: PathBuf,
    },

    /// A content command's own output could not be told from the shell's.
    ///
    /// Raised by [`execute_for_content`](CommandRunner::execute_for_content) when
    /// the markers that delimit the command's output are missing from what was
    /// captured — a shell that never ran the command, a profile that redirected
    /// the shell's output wholesale, or a shell whose syntax the implementation
    /// guessed wrong. Fails closed: the capture may carry a foreign prefix, and
    /// there is no way to find out which bytes are the command's.
    ///
    /// Carries no output bytes, like every other variant here.
    ContentMarkersAbsent {
        command: String,
        working_directory: PathBuf,
    },

    /// The command asked for input on its terminal and nothing could answer.
    ///
    /// Raised when a command writes a question to `/dev/tty` and the runner has
    /// no [`PromptHandler`](crate::commands::PromptHandler), or its handler
    /// declined. The command is ended, so nothing it started afterward ran.
    /// Carries the question and no command output.
    InputRequired {
        command: String,
        working_directory: PathBuf,
        prompt: BoundedText,
    },

    /// Failed to capture stdout during streaming execution
    StdoutSpawn { command: String },

    /// Failed to capture stderr during streaming execution
    StderrSpawn { command: String },
}

crate::parts::display_from_parts!(error CommandError);

impl crate::parts::Composed for CommandError {
    // Every sentence names the spec's own command, a part of its own that
    // bounds it, and otherwise only selfie's words and an `io::Error`, so none
    // carries the command's output. Where the command shares a sentence with a
    // cause or a program, which can be long too, they split one bound.
    fn parts(&self) -> crate::parts::Parts<'_> {
        use crate::parts::{Parts, shares};

        match self {
            Self::Timeout {
                command,
                timeout,
                limit,
                ..
            } => timed_out_parts(command, *timeout, *limit),
            Self::IoError { command, cause, .. } => {
                let cause = crate::parts::cause_words(&**cause);
                let [ran, why] = shares([command.len(), cause.len()], MAX_BOUNDED_BYTES);
                Parts::new()
                    .text("IO Error executing command '")
                    .spec_command_within(command, ran)
                    .text("': ")
                    .cause_within(&cause, why)
            }
            Self::SpawnFailed {
                command,
                program,
                cause,
                ..
            } => {
                let cause = crate::parts::cause_words(&**cause);
                let [shell, ran, why] = shares(
                    [program.len(), command.len(), cause.len()],
                    MAX_BOUNDED_BYTES,
                );
                Parts::new()
                    .text("Could not start '")
                    .path_within(Path::new(program), shell)
                    .text("' to run '")
                    .spec_command_within(command, ran)
                    .text("': ")
                    .cause_within(&cause, why)
            }
            Self::WorkingDirectoryUnusable(unusable) => unusable.parts(),
            Self::OutputReadFailed {
                command,
                stream,
                cause,
                ..
            } => {
                let cause = crate::parts::cause_words(&**cause);
                let [ran, why] = shares([command.len(), cause.len()], MAX_BOUNDED_BYTES);
                Parts::new()
                    .text(format!("Failed reading {stream} of command '"))
                    .spec_command_within(command, ran)
                    .text("': ")
                    .cause_within(&cause, why)
            }
            Self::Canceled { command, .. } => Parts::new()
                .text("Command canceled: ")
                .spec_command(command),
            Self::ContentMarkersAbsent { command, .. } => Parts::new()
                .text("Could not separate the output of command '")
                .spec_command(command)
                .text("' from the shell's"),
            Self::InputRequired {
                command, prompt, ..
            } => {
                let [ran, asked] =
                    shares([command.len(), prompt.as_str().len()], MAX_BOUNDED_BYTES);
                Parts::new()
                    .text("Command '")
                    .spec_command_within(command, ran)
                    .text("' asked for input on its terminal, and nothing answered it: ")
                    .cause_within(prompt.as_str(), asked)
            }
            Self::StdoutSpawn { command } => Parts::new()
                .text("Failed spawning stdout during command: ")
                .spec_command(command),
            Self::StderrSpawn { command } => Parts::new()
                .text("Failed spawning stderr during command: ")
                .spec_command(command),
        }
    }
}

/// How many **input bytes** a [`BoundedText`] keeps, across both ends together.
///
/// Also stated as a number in `docs/package-files.md`, because a user reading a
/// bounded failure needs to know how much of it they are seeing. Change both
/// together.
// `pub(crate)` deliberately: `BoundedText` is the public surface, and exporting
// the number would pin 2000 as API for no caller that needs it. The cost is
// that `selfie-cli` and `selfie-mcp` can call `BoundedText::bound` but cannot
// assert the bound without hardcoding 2000, so a test of the limit itself
// belongs in this crate.
pub(crate) const MAX_BOUNDED_BYTES: usize = 2000;

/// How many input bytes survive at each end when [`BoundedText::bound`] elides.
///
/// Derived from [`MAX_BOUNDED_BYTES`] rather than written as a number so the two
/// cannot drift: head plus tail is the total, so keeping both ends splits the
/// bound instead of doubling it.
const BOUNDED_END_BYTES: usize = MAX_BOUNDED_BYTES / 2;

/// Text selfie does not control, bounded before it enters a diagnostic.
///
/// A command invoked with a verbose or debug flag can echo secret material to
/// stderr, so the paths that forward such text bound it here rather than each
/// deciding a limit. It is named for the text and not for stderr because the
/// inputs differ: a command's stderr bytes at one site, at another a rendered
/// error.
///
/// The private field makes [`BoundedText::bound`] the only way to build one, so
/// a field of this type, such as `CommandFailure::ExecutionFailed`'s `stderr`,
/// cannot be given unbounded text. A caller whose field is a `String`
/// (`AuditResult::Error`) builds one and renders it into its sentence, and
/// there the bound is a convention someone has to remember.
///
/// Bounding a rendered message is not the same as bounding the command:
/// `ExecutionFailed`'s `command` field is a plain `String` and stays unbounded.
// Lives beside `CommandError` because the dotfile resolve path and the general
// failure path forward the same bytes and must treat them the same way. Within
// this module the tuple constructor is in scope; every other module goes through
// `bound`.
//
// `Debug` is derived on purpose, unlike `ResolvedContent`. The leak tests scan an
// event's `Debug` output for a secret literal, so a hand-written `Debug` printing
// `<N bytes>` would hide forwarded stderr from that scan. Replacing the derive
// fails `a_failing_check_still_reports_why_it_failed`. It does not guard that
// test's neighbor, which carries its own inline positive control.
#[derive(Debug, Clone)]
pub struct BoundedText(String);

impl BoundedText {
    /// Bound `bytes`, keeping **both ends** and eliding the middle.
    ///
    /// Keeps the first and the last `BOUNDED_END_BYTES` and replaces what is
    /// between them with a marker naming how many bytes went. Both ends, because
    /// a failing command puts its diagnosis at the end — `brew` prints pages of
    /// `==> Downloading` and then one `Error:` line — while the head is where a
    /// command names what it was doing.
    ///
    /// **The bound is on the input byte count, not on the length of the string
    /// this returns.** Invalid UTF-8 decodes lossily and each bad byte becomes a
    /// 3-byte `U+FFFD`, so `MAX_BOUNDED_BYTES` bytes of binary input yield about
    /// three times that many bytes of text. `as_str().len() <=
    /// MAX_BOUNDED_BYTES` does not hold; what is bounded is how much of the
    /// command's output survives.
    ///
    /// An input barely over the bound is returned whole, since eliding it would
    /// spend more bytes on the marker than the elision saved.
    // Valid UTF-8 is cut between characters, so no character becomes U+FFFD.
    // Anything else is cut as bytes and then decoded, since it has no character
    // boundaries to keep; the tail's cut can land mid-character at the start of
    // what it keeps.
    //
    // The whole-input comparison is against the decoded length, because for
    // invalid UTF-8 the decoded string is already longer than the input no
    // matter what this does.
    #[must_use]
    pub fn bound(bytes: &[u8]) -> Self {
        Self::keeping_ends(bytes, BOUNDED_END_BYTES)
    }

    // `bound` for text, such as a spec's command or a path decoded lossily,
    // that is one of the values a sentence's bound is shared among. It cuts on
    // character boundaries, so no character is split into U+FFFD, and each end
    // keeps at most `share / 2` of its bytes, plus one marker when it cuts.
    pub(crate) fn text_within(text: &str, share: usize) -> Self {
        let end = share / 2;
        if text.len() <= 2 * end {
            return Self(text.to_string());
        }
        let mut head = end;
        while !text.is_char_boundary(head) {
            head -= 1;
        }
        let mut tail = text.len() - end;
        while !text.is_char_boundary(tail) {
            tail += 1;
        }
        Self::elided(
            Cow::Borrowed(text),
            &text[..head],
            tail - head,
            &text[tail..],
        )
    }

    // Keeps `end` input bytes at each end, the same number at both. Valid
    // UTF-8 is cut between characters, as `text_within` cuts it.
    fn keeping_ends(bytes: &[u8], end: usize) -> Self {
        if let Ok(text) = std::str::from_utf8(bytes) {
            return Self::text_within(text, 2 * end);
        }
        // Borrowed, not allocated, whenever the input is valid UTF-8.
        let whole = String::from_utf8_lossy(bytes);

        if bytes.len() <= 2 * end {
            return Self(whole.into_owned());
        }

        let head = String::from_utf8_lossy(&bytes[..end]);
        let tail = String::from_utf8_lossy(&bytes[bytes.len() - end..]);
        Self::elided(whole, &head, bytes.len() - 2 * end, &tail)
    }

    // `head`, the marker naming the `elided` bytes between, then `tail`; or
    // `whole`, when the marker would cost more bytes than the cut saved.
    fn elided(whole: Cow<'_, str>, head: &str, elided: usize, tail: &str) -> Self {
        let cut = format!("{head}… ({elided} bytes elided) …{tail}");
        if cut.len() < whole.len() {
            Self(cut)
        } else {
            Self(whole.into_owned())
        }
    }

    /// `text` rendered and bounded, as [`bound`](Self::bound) bounds its bytes.
    #[must_use]
    pub fn of(text: &impl std::fmt::Display) -> Self {
        Self::bound(text.to_string().as_bytes())
    }

    /// Borrow the bounded text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Take the bounded text, for a diagnostic whose field is a `String`.
    #[must_use]
    pub fn into_string(self) -> String {
        self.0
    }
}

impl std::fmt::Display for BoundedText {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Rendering then bounding is the same bound as on the bytes.
    #[test]
    fn of_bounds_what_it_renders() {
        let long = "Z".repeat(MAX_BOUNDED_BYTES * 3);

        let bounded = BoundedText::of(&long);

        assert_eq!(
            bounded.as_str(),
            BoundedText::bound(long.as_bytes()).as_str()
        );
        assert!(bounded.as_str().contains("bytes elided"));
    }

    #[test]
    fn bound_does_not_split_a_multibyte_character_at_the_head_cut() {
        // A multi-byte character straddling a cut would panic a string slice.
        // Places one across the head cut: its first byte is the last one kept.
        let mut input = vec![b'a'; BOUNDED_END_BYTES - 1];
        input.extend_from_slice("é".as_bytes());
        input.extend(std::iter::repeat_n(b'b', MAX_BOUNDED_BYTES * 2));

        let bounded = BoundedText::bound(&input);

        assert!(bounded.as_str().contains("bytes elided"));
        assert!(
            !bounded.as_str().contains('\u{FFFD}'),
            "{}",
            bounded.as_str()
        );
    }

    #[test]
    fn bound_does_not_split_a_multibyte_character_at_the_tail_cut() {
        // The second cut, which the head-only shape did not have. Here the
        // character's *second* byte is the first one the tail keeps, so the
        // decode starts mid-character rather than ending mid-character.
        let mut input = vec![b'a'; MAX_BOUNDED_BYTES * 2];
        input.extend_from_slice("é".as_bytes());
        input.extend(std::iter::repeat_n(b'b', BOUNDED_END_BYTES - 1));

        let bounded = BoundedText::bound(&input);

        assert!(bounded.as_str().contains("bytes elided"));
        assert!(
            !bounded.as_str().contains('\u{FFFD}'),
            "{}",
            bounded.as_str()
        );
    }

    #[test]
    fn bound_leaves_short_input_alone() {
        assert_eq!(
            BoundedText::bound(b"not logged in").as_str(),
            "not logged in"
        );
    }

    #[test]
    fn bound_keeps_the_end_where_a_failing_command_says_why() {
        // The reason this type keeps both ends. A head-only cut kept `brew`'s
        // download chatter and dropped the `Error:` line it prints last, which
        // is the one thing the reader needed. This test fails against that
        // shape, which is the point of it existing.
        const REASON: &str = "Error: No available formula with the name \"foo\"";

        let mut input = Vec::new();
        while input.len() < MAX_BOUNDED_BYTES * 4 {
            input.extend_from_slice(b"==> Downloading https://ghcr.io/v2/homebrew/core/foo\n");
        }
        input.extend_from_slice(REASON.as_bytes());

        let bounded = BoundedText::bound(&input);

        assert!(
            bounded.as_str().contains(REASON),
            "the failure's reason was elided: {}",
            bounded.as_str()
        );
        assert!(
            bounded.as_str().starts_with("==> Downloading"),
            "the head should survive too: {}",
            bounded.as_str()
        );
    }

    #[test]
    fn bound_cuts_a_long_input_from_the_middle() {
        // The cut is the point of the type; without this the tests above pass
        // against an implementation that marks the output as elided without
        // cutting it. The marker carries no 'x', so the count measures only
        // surviving input, and it must come to the bound rather than twice it -
        // keeping both ends splits the budget, it does not double it.
        let input = vec![b'x'; MAX_BOUNDED_BYTES * 3];

        let bounded = BoundedText::bound(&input);

        assert_eq!(
            bounded.as_str().chars().filter(|c| *c == 'x').count(),
            MAX_BOUNDED_BYTES,
            "exactly the bound should survive, split across the two ends"
        );
        assert!(
            bounded.as_str().starts_with('x') && bounded.as_str().ends_with('x'),
            "both ends should be kept, so neither is the marker"
        );
    }

    #[test]
    fn bound_names_how_many_bytes_it_elided() {
        // "(truncated)" told a reader nothing about scale. Losing 40 bytes and
        // losing 400 KB should not look the same.
        let input = vec![b'x'; MAX_BOUNDED_BYTES * 3];

        let bounded = BoundedText::bound(&input);

        assert!(
            bounded
                .as_str()
                .contains(&format!("… ({} bytes elided) …", MAX_BOUNDED_BYTES * 2)),
            "{}",
            bounded.as_str()
        );
    }

    #[test]
    fn bound_returns_an_input_barely_over_the_bound_whole() {
        // Eliding one byte would spend ~20 on the marker, making the "bounded"
        // string longer than the input it bounded. Below that crossover the
        // input is returned as-is.
        let input = vec![b'x'; MAX_BOUNDED_BYTES + 1];

        let bounded = BoundedText::bound(&input);

        assert_eq!(
            bounded.as_str().chars().filter(|c| *c == 'x').count(),
            input.len()
        );
        assert!(!bounded.as_str().contains("elided"));
        assert!(bounded.as_str().len() <= String::from_utf8_lossy(&input).len());
    }

    #[test]
    fn bound_counts_input_bytes_not_output_length() {
        // Locks the caveat on `bound`: the limit is on what goes in. Every byte
        // here is invalid UTF-8 and decodes to a 3-byte U+FFFD, so the input is
        // at the bound and untouched while the string is three times longer.
        // A reader who assumes `as_str().len() <= MAX_BOUNDED_BYTES` is wrong,
        // and this is where they find out.
        let input = vec![0xff; MAX_BOUNDED_BYTES];

        let bounded = BoundedText::bound(&input);

        assert!(
            !bounded.as_str().ends_with("… (truncated)"),
            "input was at the bound, so nothing should have been cut"
        );
        assert_eq!(bounded.as_str().chars().count(), MAX_BOUNDED_BYTES);
        assert_eq!(bounded.as_str().len(), MAX_BOUNDED_BYTES * 3);
    }
}
