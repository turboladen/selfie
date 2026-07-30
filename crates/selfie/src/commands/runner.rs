//! Command execution abstractions and types
//!
//! This module provides the core abstractions for executing system commands
//! in a cross-platform manner. It implements the Command Runner port pattern
//! to allow different command execution strategies while maintaining a consistent interface.

use std::{
    borrow::Cow,
    fmt,
    future::Future,
    path::{Path, PathBuf},
    process::Output,
    sync::Arc,
    time::Duration,
};

use thiserror::Error;
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

impl fmt::Display for OutputChunk {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Stdout(s) | Self::Stderr(s) => f.write_str(s),
        }
    }
}

/// Port for command execution (Hexagonal Architecture)
///
/// This trait abstracts command execution to allow different implementations
/// (shell commands, mock execution, etc.) and to enable comprehensive testing.
/// It provides both buffered and streaming execution modes with timeout support.
#[cfg_attr(any(test, feature = "with_mocks"), mockall::automock)]
pub trait CommandRunner: Send + Sync {
    /// Check if a command executable exists on `PATH`
    ///
    /// Tests whether the specified command can be found as a filesystem
    /// executable on `PATH`. Shell builtins are not detected. This is
    /// useful for checking package manager prerequisites (e.g., `brew`,
    /// `npm`, `apt`) before attempting package installations.
    ///
    /// # Arguments
    ///
    /// * `command` - The command name to check (e.g., "npm", "brew", "apt")
    ///
    /// # Returns
    ///
    /// `true` if an executable with the given name exists on `PATH`, `false` otherwise
    fn is_command_available(&self, command: &str) -> impl Future<Output = bool> + Send;

    /// Execute a command and wait for completion
    ///
    /// Runs the specified command and waits for it to complete, collecting
    /// all output. This is suitable for commands that don't produce large
    /// amounts of output or don't need real-time feedback.
    ///
    /// # Arguments
    ///
    /// * `command` - The shell command to execute
    ///
    /// # Errors
    ///
    /// Returns [`CommandError`] if:
    /// - The command cannot be started (IO error)
    /// - The command exits with a non-zero status code
    /// - Command execution times out (implementation-dependent default)
    fn execute(
        &self,
        command: &str,
        token: &CancellationToken,
    ) -> impl Future<Output = Result<CommandOutput, CommandError>> + Send;

    /// Execute a command with a specific timeout
    ///
    /// Like [`execute`](CommandRunner::execute) but with an explicit timeout.
    /// The command will be terminated if it doesn't complete within the specified duration.
    ///
    /// # Arguments
    ///
    /// * `command` - The shell command to execute
    /// * `timeout` - Maximum duration to wait for command completion
    ///
    /// # Errors
    ///
    /// Returns [`CommandError`] if:
    /// - The command cannot be started (IO error)
    /// - The command exits with a non-zero status code
    /// - The command times out before completion
    fn execute_with_timeout(
        &self,
        command: &str,
        timeout: Duration,
        token: &CancellationToken,
    ) -> impl Future<Output = Result<CommandOutput, CommandError>> + Send;

    /// Execute a command with a specific working directory and timeout
    ///
    /// Like [`execute_with_timeout`](CommandRunner::execute_with_timeout), but the
    /// command runs with `working_dir` as its current directory rather than
    /// inheriting selfie's own. Used where a command's meaning depends on where it
    /// runs — dotfile content providers resolve against the package file's parent
    /// directory, the same base repository sources resolve against.
    ///
    /// # Arguments
    ///
    /// * `command` - The shell command to execute
    /// * `working_dir` - Directory to run the command in
    /// * `timeout` - Maximum duration to wait for command completion
    ///
    /// # Errors
    ///
    /// Returns [`CommandError`] if:
    /// - `working_dir` does not exist or is not a directory (reported as
    ///   [`CommandError::IoError`], since the shell cannot be spawned there)
    /// - The command cannot be started (IO error)
    /// - The command times out before completion
    /// - The command is cancelled via `token`
    ///
    /// A non-zero exit is **not** an error: it is reported through
    /// [`CommandOutput::is_success`], as with the other execution methods.
    fn execute_in_dir(
        &self,
        command: &str,
        working_dir: &Path,
        timeout: Duration,
        token: &CancellationToken,
    ) -> impl Future<Output = Result<CommandOutput, CommandError>> + Send;

    /// Execute a command with streaming output
    ///
    /// Runs the command and streams stdout/stderr output through the provided
    /// channel as it becomes available. This is ideal for long-running commands
    /// or when real-time feedback is needed.
    ///
    /// # Arguments
    ///
    /// * `command` - The shell command to execute
    /// * `timeout` - Maximum duration to wait for command completion
    /// * `output_sender` - Channel sender to send output chunks to
    ///
    /// # Errors
    ///
    /// Returns [`CommandError`] if:
    /// - The command cannot be started (IO error)
    /// - The command exits with a non-zero status code
    /// - The command times out before completion
    /// - Channel communication fails
    fn execute_streaming(
        &self,
        command: &str,
        timeout: Duration,
        output_sender: mpsc::Sender<OutputChunk>,
        token: &CancellationToken,
    ) -> impl Future<Output = Result<CommandOutput, CommandError>> + Send;
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

/// Errors that can occur during command execution
///
/// Represents all possible failure modes when executing system commands,
/// providing detailed context for debugging and error handling.
#[derive(Error, Debug, Clone)]
pub enum CommandError {
    /// Command execution exceeded the specified timeout
    #[error("Command timed out after {timeout:?}: {command}")]
    Timeout {
        command: String,
        timeout: Duration,
        working_directory: PathBuf,
    },

    /// IO error occurred while starting or running the command
    #[error("IO Error executing command '{command}': {source}")]
    IoError {
        command: String,
        working_directory: PathBuf,
        #[source]
        source: Arc<std::io::Error>,
    },

    /// Command executed but returned a non-zero exit code
    #[error("Command failed with exit code {exit_code}: {command}")]
    NonZeroExit {
        command: String,
        exit_code: i32,
        stdout: String,
        stderr: String,
        working_directory: PathBuf,
        execution_duration: Duration,
    },

    /// Command was cancelled via a cancellation token
    #[error("Command cancelled: {command}")]
    Cancelled {
        command: String,
        working_directory: PathBuf,
    },

    /// Failed to capture stdout during streaming execution
    #[error("Failed spawning stdout during command: {0}")]
    StdoutSpawn(String),

    /// Failed to capture stderr during streaming execution
    #[error("Failed spawning stderr during command: {0}")]
    StderrSpawn(String),

    /// Error occurred in the output callback during streaming execution
    #[error("Error while processing command: {0}")]
    Callback(OutputChunk),
}

/// How many **input bytes** a [`BoundedText`] keeps.
///
/// Stated as a number in `docs/package-files.md`, because a user who sees the
/// truncation marker needs to know how much was lost. Change both together.
/// `pub(crate)` deliberately: [`BoundedText`] is the public surface, and
/// exporting the number would pin 2000 as API for no caller that needs it. The
/// tradeoff is that `selfie-cli` and `selfie-mcp` can *call* [`BoundedText::bound`]
/// but cannot assert the bound without hardcoding 2000, so a test of the limit
/// itself belongs in this crate.
pub(crate) const MAX_BOUNDED_BYTES: usize = 2000;

/// Text selfie does not control, bounded before it enters a diagnostic.
///
/// A command invoked with a verbose or debug flag can echo secret material to
/// stderr, so the paths that forward such text bound it here rather than each
/// deciding a limit. Named for the text rather than for stderr because the
/// inputs differ: a command's stderr bytes at one site, and at another a
/// rendered [`CommandError`], whose `Display` embeds the package file's own
/// unbounded `command:` string. Bounding that rendered message is not the same
/// as bounding the command: `ExecutionFailed`'s `command` field is a plain
/// `String` and stays unbounded.
/// Lives beside [`CommandError`] because both the dotfile resolve path and the
/// general failure path forward the same bytes and must treat them the same way.
///
/// # What is and is not enforced
///
/// The private field means [`BoundedText::bound`] is the only way to make one,
/// so `CommandFailure::ExecutionFailed`'s `stderr` — the one field typed as
/// `BoundedText` — cannot be given unbounded text by any struct-variant literal.
/// **That is the only compiler-enforced site.** Callers that need a `String`
/// (`AuditResult::Error`, `ResolveError`'s `stderr` fields) construct one and
/// unwrap it, so at those sites the bound is still a convention — a better-named
/// helper someone has to remember to call, not a gate. Within this one module
/// the tuple constructor is in scope and could be called directly; every other
/// module, in this crate and outside it, must go through `bound`.
///
/// # Why `Debug` is derived
///
/// Deliberately, and unlike `ResolvedContent` or [`CommandError::NonZeroExit`]'s
/// stdout. Those are never forwarded, so their `Debug` is a pure exit worth
/// closing by hand. This value is text selfie forwards on purpose, and
/// `.claude/rules/secrets.md` prescribes scanning an event's `Debug` output for
/// a secret literal. A hand-written `Debug` printing `<N bytes>` would hide
/// forwarded stderr from that scan, so a secret that reaches stderr later would
/// go unseen rather than caught. Blinding it would contain nothing: the text is
/// already on its way to the terminal by design.
///
/// It is load-bearing today — replacing the derive fails
/// `a_failing_check_still_reports_why_it_failed`, which reads the forwarded
/// stderr out of `{:?}`. It does **not** guard that test's neighbor,
/// `a_failing_check_keeps_its_stdout_out_of_the_completed_event`: that fixture
/// puts its secret on *stdout*, which never becomes a `BoundedText`, so the
/// derive cannot affect it and its own inline positive control is what keeps it
/// honest. Do not read the two as each other's controls — an earlier version of
/// this comment did, and the risk is that someone who believes it deletes that
/// control as redundant and the leak test silently goes vacuous.
#[derive(Debug, Clone)]
pub struct BoundedText(String);

impl BoundedText {
    /// Bound `bytes`, appending a marker when anything was cut.
    ///
    /// **The bound is on the input byte count, not on the length of the string
    /// this returns.** Invalid UTF-8 is replaced lossily and each bad byte
    /// becomes a 3-byte `U+FFFD`, so `MAX_BOUNDED_BYTES` bytes of binary input
    /// yield about three times that many bytes of text. What is bounded is how
    /// much of the command's output survives, which is what the forwarding paths
    /// need; `as_str().len() <= MAX_BOUNDED_BYTES` does not hold.
    ///
    /// Truncates the bytes and then decodes, rather than slicing a `String`: a
    /// multi-byte character straddling the cut would panic on a string slice.
    #[must_use]
    pub fn bound(bytes: &[u8]) -> Self {
        if bytes.len() <= MAX_BOUNDED_BYTES {
            Self(String::from_utf8_lossy(bytes).into_owned())
        } else {
            Self(format!(
                "{}… (truncated)",
                String::from_utf8_lossy(&bytes[..MAX_BOUNDED_BYTES])
            ))
        }
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

    #[test]
    fn bound_does_not_split_a_multibyte_character() {
        // A multi-byte character straddling the cut would panic a string slice.
        let mut input = vec![b'a'; MAX_BOUNDED_BYTES - 1];
        input.extend_from_slice("é".as_bytes());

        let bounded = BoundedText::bound(&input);

        assert!(bounded.as_str().ends_with("… (truncated)"));
    }

    #[test]
    fn bound_leaves_short_input_alone() {
        assert_eq!(
            BoundedText::bound(b"not logged in").as_str(),
            "not logged in"
        );
    }

    #[test]
    fn bound_cuts_a_long_input() {
        // The cut is the point of the type; without this the two tests above
        // pass against an implementation that marks the output as truncated
        // without cutting it.
        let input = vec![b'x'; MAX_BOUNDED_BYTES * 3];

        let bounded = BoundedText::bound(&input);

        assert!(bounded.as_str().ends_with("… (truncated)"));
        assert_eq!(
            bounded.as_str().chars().filter(|c| *c == 'x').count(),
            MAX_BOUNDED_BYTES,
            "exactly the bound should survive"
        );
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
