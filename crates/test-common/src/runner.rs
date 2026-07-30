//! A `CommandRunner` with scripted answers, for tests that must not run real commands.
//!
//! `MockCommandRunner` is not `Clone`, which the services that own a runner
//! require. This is a small hand-written stand-in that is, and that additionally
//! records what it was asked so a test can assert on the working directory a
//! command ran in — or that no command ran at all.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use selfie::commands::{CommandError, CommandOutput, CommandRunner, OutputChunk, OutputStream};
use tokio_util::sync::CancellationToken;

/// What a scripted command does when run.
#[derive(Debug, Clone)]
enum Response {
    /// The command ran and produced this.
    Output {
        exit_code: i32,
        stdout: Vec<u8>,
        stderr: Vec<u8>,
    },
    /// The command did not produce usable output, and the runner says why.
    Error(CommandError),
}

/// A `CommandRunner` that answers from a script and records every call.
///
/// An unscripted command is an error rather than a default success, so a test
/// cannot pass by accidentally exercising a command it never meant to allow.
#[derive(Debug, Clone, Default)]
pub struct FakeCommandRunner {
    responses: HashMap<String, Response>,
    calls: Arc<Mutex<Vec<(String, PathBuf)>>>,
}

impl FakeCommandRunner {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Script a command that succeeds with `stdout`.
    #[must_use]
    pub fn succeeding(mut self, command: &str, stdout: &[u8]) -> Self {
        self.responses.insert(
            command.to_string(),
            Response::Output {
                exit_code: 0,
                stdout: stdout.to_vec(),
                stderr: Vec::new(),
            },
        );
        self
    }

    /// Script a command the runner refuses to report output for.
    ///
    /// For the failures that are not a non-zero exit: a pipe that could not be
    /// read to the end, a timeout, a cancellation. These cannot be provoked from
    /// a scripted exit code, and they are the cases where a caller must not treat
    /// a buffer as the command's answer.
    #[must_use]
    pub fn erroring(mut self, command: &str, error: CommandError) -> Self {
        self.responses
            .insert(command.to_string(), Response::Error(error));
        self
    }

    /// Script a command whose stdout pipe dies part-way through being read.
    ///
    /// The shape of `selfie-ql8m`: the command itself is fine, and what selfie
    /// buffered is a prefix of its output rather than the whole of it.
    #[must_use]
    pub fn stdout_read_failing(self, command: &str) -> Self {
        let error = CommandError::OutputReadFailed {
            command: command.to_string(),
            working_directory: PathBuf::from("."),
            stream: OutputStream::Stdout,
            source: Arc::new(std::io::Error::other("pipe died mid-read")),
        };
        self.erroring(command, error)
    }

    /// Script a command that succeeds while also writing to stderr.
    ///
    /// Used to check that stderr does not surface on the success path.
    #[must_use]
    pub fn succeeding_noisy(mut self, command: &str, stdout: &[u8], stderr: &[u8]) -> Self {
        self.responses.insert(
            command.to_string(),
            Response::Output {
                exit_code: 0,
                stdout: stdout.to_vec(),
                stderr: stderr.to_vec(),
            },
        );
        self
    }

    /// Script a command that exits non-zero while also writing to stdout.
    ///
    /// Used to check the failure path: a provider's stdout is the secret, and a
    /// failure must not forward it.
    #[must_use]
    pub fn failing_with_stdout(mut self, command: &str, stdout: &[u8], stderr: &[u8]) -> Self {
        self.responses.insert(
            command.to_string(),
            Response::Output {
                exit_code: 1,
                stdout: stdout.to_vec(),
                stderr: stderr.to_vec(),
            },
        );
        self
    }

    /// Script a command that exits non-zero with `stderr`.
    #[must_use]
    pub fn failing(mut self, command: &str, stderr: &[u8]) -> Self {
        self.responses.insert(
            command.to_string(),
            Response::Output {
                exit_code: 1,
                stdout: Vec::new(),
                stderr: stderr.to_vec(),
            },
        );
        self
    }

    /// Every (command, working directory) pair this runner was asked to run.
    #[must_use]
    pub fn calls(&self) -> Vec<(String, PathBuf)> {
        self.calls.lock().unwrap().clone()
    }

    /// How many commands this runner has been asked to run.
    #[must_use]
    pub fn call_count(&self) -> usize {
        self.calls.lock().unwrap().len()
    }

    fn answer(&self, command: &str, working_dir: &Path) -> Result<CommandOutput, CommandError> {
        self.calls
            .lock()
            .unwrap()
            .push((command.to_string(), working_dir.to_path_buf()));

        match self.responses.get(command) {
            Some(Response::Output {
                exit_code,
                stdout,
                stderr,
            }) => Ok(command_output(*exit_code, stdout.clone(), stderr.clone())),
            Some(Response::Error(error)) => Err(error.clone()),
            None => Err(CommandError::IoError {
                command: command.to_string(),
                working_directory: working_dir.to_path_buf(),
                source: Arc::new(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "command not scripted in this test",
                )),
            }),
        }
    }
}

impl CommandRunner for FakeCommandRunner {
    async fn is_command_available(&self, _command: &str) -> bool {
        true
    }

    async fn execute(
        &self,
        command: &str,
        _token: &CancellationToken,
    ) -> Result<CommandOutput, CommandError> {
        self.answer(command, Path::new("."))
    }

    async fn execute_with_timeout(
        &self,
        command: &str,
        _timeout: Duration,
        _token: &CancellationToken,
    ) -> Result<CommandOutput, CommandError> {
        self.answer(command, Path::new("."))
    }

    async fn execute_in_dir(
        &self,
        command: &str,
        working_dir: &Path,
        _timeout: Duration,
        _token: &CancellationToken,
    ) -> Result<CommandOutput, CommandError> {
        self.answer(command, working_dir)
    }

    async fn execute_streaming(
        &self,
        command: &str,
        _timeout: Duration,
        _output_sender: tokio::sync::mpsc::Sender<OutputChunk>,
        _token: &CancellationToken,
    ) -> Result<CommandOutput, CommandError> {
        self.answer(command, Path::new("."))
    }
}

/// Build a `CommandOutput` with the given exit code and streams.
fn command_output(exit_code: i32, stdout: Vec<u8>, stderr: Vec<u8>) -> CommandOutput {
    #[cfg(unix)]
    let status = {
        use std::os::unix::process::ExitStatusExt as _;
        // Wait status encodes the exit code in the high byte.
        std::process::ExitStatus::from_raw(exit_code << 8)
    };
    #[cfg(windows)]
    let status = {
        use std::os::windows::process::ExitStatusExt as _;
        std::process::ExitStatus::from_raw(exit_code as u32)
    };

    CommandOutput::from_parts(status, stdout, stderr, Duration::ZERO)
}
