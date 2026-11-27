//! A `CommandRunner` with scripted answers, for tests that must not run real commands.

use std::collections::HashMap;
use std::os::unix::process::ExitStatusExt as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio_util::sync::CancellationToken;

use super::runner::{
    CommandError, CommandOutput, CommandRunner, ContentOutput, OutputChunk, OutputStream,
    StreamedExit,
};

/// What a scripted command does when run.
#[derive(Clone)]
enum Response {
    /// The command ran and wrote this, as every method but
    /// `execute_for_content` reports it. `separation` says what
    /// `execute_for_content` makes of the same stdout.
    Output {
        exit_code: i32,
        stdout: Vec<u8>,
        stderr: Vec<u8>,
        separation: Separation,
    },
    /// The command did not produce usable output, and the runner says why.
    Error(CommandError),
    /// The command's stdout pipe died part-way through being read.
    StdoutReadFailed,
}

/// How `execute_for_content` separates a command's output from the shell's.
#[derive(Debug, Clone, Copy)]
enum Separation {
    /// The command's output starts `discarded_before` bytes into stdout, and
    /// `tail_verified` says whether where it ends was established.
    Separated {
        discarded_before: usize,
        tail_verified: bool,
    },
    /// The command's output could not be told from the shell's.
    Unseparable,
}

impl Separation {
    const CLEAN: Self = Self::Separated {
        discarded_before: 0,
        tail_verified: true,
    };
}

// Prints lengths, never content: a scripted stdout is often a test's secret, and
// a derived `Debug` would print it from any `{:?}` of the runner.
impl std::fmt::Debug for Response {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Output {
                exit_code,
                stdout,
                stderr,
                separation,
            } => f
                .debug_struct("Output")
                .field("exit_code", exit_code)
                .field("stdout", &format_args!("<{} bytes>", stdout.len()))
                .field("stderr", &format_args!("<{} bytes>", stderr.len()))
                .field("separation", separation)
                .finish(),
            Self::Error(error) => f.debug_tuple("Error").field(error).finish(),
            Self::StdoutReadFailed => f.write_str("StdoutReadFailed"),
        }
    }
}

/// A `CommandRunner` that answers from a script and records every call.
///
/// Use it where a service needs a runner that is `Clone`, which
/// `MockCommandRunner` is not, or where a test asserts on the commands that ran
/// and the directories they ran in. Clones share one record of calls.
///
/// An unscripted command answers with an error, so a test cannot pass by
/// running a command it never meant to allow. A test that expects an error
/// should still match the specific one, because an unscripted command is an
/// error too.
#[derive(Debug, Clone, Default)]
pub struct FakeCommandRunner {
    responses: HashMap<String, Response>,
    calls: Arc<Mutex<Vec<(String, PathBuf)>>>,
    cancels: HashMap<String, CancellationToken>,
    yields: bool,
}

impl FakeCommandRunner {
    /// A runner with nothing scripted.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Script a command that succeeds with `stdout`.
    #[must_use]
    pub fn succeeding(self, command: &str, stdout: &[u8]) -> Self {
        self.output(command, 0, stdout, b"")
    }

    /// Script a command that succeeds with `stdout` while also writing to
    /// `stderr`.
    #[must_use]
    pub fn succeeding_noisy(self, command: &str, stdout: &[u8], stderr: &[u8]) -> Self {
        self.output(command, 0, stdout, stderr)
    }

    /// Script a command that exits non-zero with `stderr`.
    #[must_use]
    pub fn failing(self, command: &str, stderr: &[u8]) -> Self {
        self.output(command, 1, b"", stderr)
    }

    /// Script a command that exits non-zero while also writing to stdout.
    #[must_use]
    pub fn failing_with_stdout(self, command: &str, stdout: &[u8], stderr: &[u8]) -> Self {
        self.output(command, 1, stdout, stderr)
    }

    /// Script a command the runner refuses to report output for, answering
    /// `error`.
    ///
    /// For the failures that are not a non-zero exit, such as a timeout or a
    /// cancellation, which a scripted exit code cannot provoke.
    #[must_use]
    pub fn erroring(mut self, command: &str, error: CommandError) -> Self {
        self.responses
            .insert(command.to_string(), Response::Error(error));
        self
    }

    /// Script a command whose stdout pipe dies part-way through being read.
    ///
    /// The runner answers [`CommandError::OutputReadFailed`] for
    /// [`OutputStream::Stdout`], naming the directory the command ran in.
    #[must_use]
    pub fn stdout_read_failing(mut self, command: &str) -> Self {
        self.responses
            .insert(command.to_string(), Response::StdoutReadFailed);
        self
    }

    /// Script a command that succeeds with `stdout` after `discarded` bytes of
    /// shell noise.
    ///
    /// [`execute_for_content`](CommandRunner::execute_for_content) answers
    /// `stdout` and reports the `discarded` bytes cut off ahead of it. Every
    /// other method answers the noise and `stdout` together, as a real runner
    /// would.
    #[must_use]
    pub fn with_noise_before(self, command: &str, stdout: &[u8], discarded: usize) -> Self {
        let mut raw = vec![b'~'; discarded];
        raw.extend_from_slice(stdout);
        self.separated(
            command,
            &raw,
            Separation::Separated {
                discarded_before: discarded,
                tail_verified: true,
            },
        )
    }

    /// Script a command that succeeds with `stdout`, which
    /// [`execute_for_content`](CommandRunner::execute_for_content) reports
    /// without having established where the output ends.
    #[must_use]
    pub fn with_unverified_tail(self, command: &str, stdout: &[u8]) -> Self {
        self.separated(
            command,
            stdout,
            Separation::Separated {
                discarded_before: 0,
                tail_verified: false,
            },
        )
    }

    /// Script a command that succeeds with no output, which
    /// [`execute_for_content`](CommandRunner::execute_for_content) cannot tell
    /// from the shell's.
    ///
    /// `execute_for_content` answers [`CommandError::ContentMarkersAbsent`].
    /// Every other method answers the empty success, as a real runner would.
    #[must_use]
    pub fn unseparable(self, command: &str) -> Self {
        self.separated(command, b"", Separation::Unseparable)
    }

    /// Cancel `token` when `command` runs, as a Ctrl+C landing while it runs
    /// would. The command still answers as scripted.
    #[must_use]
    pub fn canceling(mut self, command: &str, token: &CancellationToken) -> Self {
        self.cancels.insert(command.to_string(), token.clone());
        self
    }

    /// Yield to the runtime before answering each command, as a real command
    /// does while it runs, so futures awaited together interleave.
    #[must_use]
    pub fn yielding(mut self) -> Self {
        self.yields = true;
        self
    }

    /// Every (command, working directory) pair this runner was asked to run,
    /// in order.
    ///
    /// # Panics
    ///
    /// Panics if a thread panicked while recording a call.
    #[must_use]
    pub fn calls(&self) -> Vec<(String, PathBuf)> {
        self.calls.lock().unwrap().clone()
    }

    /// How many commands this runner has been asked to run.
    ///
    /// # Panics
    ///
    /// Panics if a thread panicked while recording a call.
    #[must_use]
    pub fn call_count(&self) -> usize {
        self.calls.lock().unwrap().len()
    }

    fn output(mut self, command: &str, exit_code: i32, stdout: &[u8], stderr: &[u8]) -> Self {
        self.responses.insert(
            command.to_string(),
            Response::Output {
                exit_code,
                stdout: stdout.to_vec(),
                stderr: stderr.to_vec(),
                separation: Separation::CLEAN,
            },
        );
        self
    }

    fn separated(mut self, command: &str, stdout: &[u8], separation: Separation) -> Self {
        self.responses.insert(
            command.to_string(),
            Response::Output {
                exit_code: 0,
                stdout: stdout.to_vec(),
                stderr: Vec::new(),
                separation,
            },
        );
        self
    }

    fn record(&self, command: &str, working_dir: &Path) {
        self.calls
            .lock()
            .unwrap()
            .push((command.to_string(), working_dir.to_path_buf()));
        if let Some(token) = self.cancels.get(command) {
            token.cancel();
        }
    }

    // Answers with the command's whole stdout, and how `execute_for_content`
    // separates it. The other methods ignore the separation.
    async fn answer(
        &self,
        command: &str,
        working_dir: &Path,
    ) -> Result<(CommandOutput, Separation), CommandError> {
        if self.yields {
            tokio::task::yield_now().await;
        }
        self.record(command, working_dir);

        match self.responses.get(command) {
            Some(Response::Output {
                exit_code,
                stdout,
                stderr,
                separation,
            }) => {
                let output = CommandOutput {
                    output: std::process::Output {
                        status: exited_with(*exit_code),
                        stdout: stdout.clone(),
                        stderr: stderr.clone(),
                    },
                    duration: Duration::ZERO,
                };
                Ok((output, *separation))
            }
            Some(Response::Error(error)) => Err(error.clone()),
            Some(Response::StdoutReadFailed) => Err(CommandError::OutputReadFailed {
                command: command.to_string(),
                working_directory: working_dir.to_path_buf(),
                stream: OutputStream::Stdout,
                cause: Arc::new(std::io::Error::other("pipe died mid-read")),
            }),
            // An error, never a default success, so that a test cannot pass by
            // exercising a command it did not script.
            None => Err(CommandError::IoError {
                command: command.to_string(),
                working_directory: working_dir.to_path_buf(),
                cause: Arc::new(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "command not scripted in this test",
                )),
            }),
        }
    }

    async fn output_of(
        &self,
        command: &str,
        working_dir: &Path,
    ) -> Result<CommandOutput, CommandError> {
        self.answer(command, working_dir)
            .await
            .map(|(output, _)| output)
    }
}

// A wait status carries the exit code in its high byte.
fn exited_with(exit_code: i32) -> std::process::ExitStatus {
    std::process::ExitStatus::from_raw(exit_code << 8)
}

impl CommandRunner for FakeCommandRunner {
    async fn is_command_available(&self, _command: &str) -> bool {
        true
    }

    async fn execute(
        &self,
        command: &str,
        working_dir: &Path,
        _token: &CancellationToken,
    ) -> Result<CommandOutput, CommandError> {
        self.output_of(command, working_dir).await
    }

    async fn execute_in_dir(
        &self,
        command: &str,
        working_dir: &Path,
        _timeout: Duration,
        _token: &CancellationToken,
    ) -> Result<CommandOutput, CommandError> {
        self.output_of(command, working_dir).await
    }

    async fn execute_streaming(
        &self,
        command: &str,
        working_dir: &Path,
        _timeout: Duration,
        _output_sender: tokio::sync::mpsc::Sender<OutputChunk>,
        _token: &CancellationToken,
    ) -> Result<StreamedExit, CommandError> {
        self.output_of(command, working_dir)
            .await
            .map(StreamedExit::from_output)
    }

    /// Answers with the scripted output, separated as the script says.
    async fn execute_for_content(
        &self,
        command: &str,
        working_dir: &Path,
        _timeout: Duration,
        _token: &CancellationToken,
    ) -> Result<ContentOutput, CommandError> {
        // A fake runs no shell, so the script says where the command's output
        // starts. That holds only here: separating a real command's output is
        // the shell adapter's job, not a trait default.
        let (output, separation) = self.answer(command, working_dir).await?;
        let Separation::Separated {
            discarded_before,
            tail_verified,
        } = separation
        else {
            return Err(CommandError::ContentMarkersAbsent {
                command: command.to_string(),
                working_directory: working_dir.to_path_buf(),
            });
        };
        let success = output.is_success();
        let stderr = output.stderr().to_vec();
        let mut stdout = output.into_stdout();
        stdout.drain(..discarded_before);
        Ok(ContentOutput::from_capture(
            success,
            stdout,
            stderr,
            discarded_before,
            tail_verified,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TIMEOUT: Duration = Duration::from_secs(5);

    // Only `execute_for_content` separates output, so the other methods see a
    // scripted command as a real runner would show it.
    #[tokio::test]
    async fn only_execute_for_content_separates_the_output() {
        let token = CancellationToken::new();
        let dir = Path::new("/pkg");
        let runner = FakeCommandRunner::new()
            .with_noise_before("noisy", b"out", 3)
            .unseparable("mixed");

        let raw = runner
            .execute_in_dir("noisy", dir, TIMEOUT, &token)
            .await
            .unwrap();
        assert_eq!(raw.stdout(), b"~~~out");
        let content = runner
            .execute_for_content("noisy", dir, TIMEOUT, &token)
            .await
            .unwrap();
        assert_eq!(content.discarded_before(), 3);
        assert_eq!(content.into_stdout(), b"out");

        let plain = runner
            .execute_in_dir("mixed", dir, TIMEOUT, &token)
            .await
            .unwrap();
        assert!(plain.is_success());
        let refused = runner
            .execute_for_content("mixed", dir, TIMEOUT, &token)
            .await
            .unwrap_err();
        assert!(
            matches!(refused, CommandError::ContentMarkersAbsent { .. }),
            "{refused}"
        );
    }
}
