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
