//! Shell command runner adapter implementation
//!
//! This module provides a concrete implementation of the `CommandRunner` trait
//! that executes commands through a system shell. It supports both blocking
//! and streaming execution modes with configurable timeouts.

use std::{
    path::Path,
    process::{Output, Stdio},
    sync::Arc,
    time::{Duration, Instant},
};

use tokio::{io::AsyncReadExt, process::Command};
use tokio_util::sync::CancellationToken;

use super::runner::{CommandError, CommandOutput, CommandRunner, OutputChunk, OutputStream};

/// Shell command runner implementation
///
/// Executes commands using a system shell (e.g., `/bin/sh`, `/bin/bash`).
/// Provides both simple execution and streaming output capabilities with
/// configurable timeouts and working directory support.
#[derive(Clone, Debug)]
pub struct ShellCommandRunner {
    /// Path to the shell executable to use for command execution
    shell: String,

    /// Default timeout for commands when no explicit timeout is provided
    default_timeout: Duration,

    /// Whether to run as a login shell (-l flag), sourcing the user's profile.
    /// Needed when the process is launched from a GUI app (e.g., MCP server
    /// started by Claude Desktop) where PATH doesn't include user-installed
    /// tools like ~/.cargo/bin, homebrew, fnm, etc.
    login: bool,
}

impl ShellCommandRunner {
    /// Create a new shell command runner
    ///
    /// # Arguments
    ///
    /// * `shell` - Path to the shell executable (e.g., "/bin/sh", "/bin/bash")
    /// * `default_timeout` - Default timeout for command execution
    ///
    /// # Examples
    ///
    /// ```
    /// use std::time::Duration;
    /// use selfie::commands::ShellCommandRunner;
    ///
    /// let runner = ShellCommandRunner::new(ShellCommandRunner::default_shell(), Duration::from_secs(30));
    /// ```
    #[must_use]
    pub fn new(shell: &str, default_timeout: Duration) -> Self {
        Self {
            shell: shell.to_string(),
            default_timeout,
            login: false,
        }
    }

    /// Create a login shell command runner that sources the user's profile.
    ///
    /// On Unix, uses the user's default shell (from `SHELL` env var, falling
    /// back to `/bin/sh`) with the `-l` flag to source login profiles
    /// (`.bash_profile`, `.zshrc`, etc.). This ensures PATH includes
    /// user-installed tools like `~/.cargo/bin`, homebrew paths, etc.
    ///
    /// On non-Unix platforms, falls back to the default shell without `-l`
    /// since login shell semantics don't apply.
    ///
    /// Use this when the process is launched from a non-shell context
    /// (e.g., an MCP server started by a GUI application).
    #[must_use]
    pub fn login_shell(default_timeout: Duration) -> Self {
        #[cfg(unix)]
        {
            let shell =
                std::env::var("SHELL").unwrap_or_else(|_| Self::default_shell().to_string());
            Self {
                shell,
                default_timeout,
                login: true,
            }
        }
        #[cfg(not(unix))]
        {
            Self::new(Self::default_shell(), default_timeout)
        }
    }

    /// Build a `Command` with the configured shell, login flag, and command string.
    ///
    /// When `working_dir` is `Some`, the child runs there; otherwise it inherits
    /// selfie's own current directory.
    fn build_command(&self, command: &str, working_dir: Option<&Path>) -> Command {
        let mut cmd = Command::new(&self.shell);
        if self.login {
            cmd.arg("-l");
        }
        cmd.arg("-c")
            .arg(command)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(dir) = working_dir {
            cmd.current_dir(dir);
        }
        cmd
    }

    /// Run a command to completion, buffering stdout and stderr.
    ///
    /// Shared by [`execute_with_timeout`](CommandRunner::execute_with_timeout) and
    /// [`execute_in_dir`](CommandRunner::execute_in_dir); the only difference
    /// between them is whether a working directory is supplied.
    async fn run_buffered(
        &self,
        command: &str,
        working_dir: Option<&Path>,
        timeout: Duration,
        token: &CancellationToken,
    ) -> Result<CommandOutput, CommandError> {
        let start_time = Instant::now();
        // Report the directory the command actually ran in, not selfie's own, so a
        // failure names the place the user configured.
        let working_directory = working_dir.map_or_else(
            || std::env::current_dir().unwrap_or_else(|_| Path::new(".").to_path_buf()),
            Path::to_path_buf,
        );

        // Check for pre-cancellation before spawning
        if token.is_cancelled() {
            return Err(CommandError::Cancelled {
                command: command.to_string(),
                working_directory,
            });
        }

        let mut cmd = self.build_command(command, working_dir);

        let mut child = cmd.spawn().map_err(|e| CommandError::IoError {
            command: command.to_string(),
            working_directory: working_directory.clone(),
            source: Arc::new(e),
        })?;

        // Take pipes and read them in spawned tasks, so both drain while wait()
        // runs. This is what avoids the deadlock when the child produces more
        // than the OS pipe buffer (~64KB): a read that only started after wait()
        // returned would never get there, because the child cannot exit until
        // something drains the pipe it is blocked writing to. The spawns must
        // stay *before* the select for that to hold; nothing below depends on
        // the order the handles are later joined in.
        let child_stdout = child.stdout.take();
        let child_stderr = child.stderr.take();
        let stdout_handle = tokio::spawn(read_stream(child_stdout));
        let stderr_handle = tokio::spawn(read_stream(child_stderr));

        tokio::select! {
            status = child.wait() => {
                let status = status.map_err(|e| CommandError::IoError {
                    command: command.to_string(),
                    working_directory: working_directory.clone(),
                    source: Arc::new(e),
                })?;
                // Both readers are already running, so joining them concurrently
                // rather than one after the other is neutral for progress. It is
                // done this way so that a failure on one stream still reaps the
                // other instead of leaving a detached task behind.
                let (out, err) = tokio::join!(stdout_handle, stderr_handle);
                finish(status, out, err, command, &working_directory, start_time.elapsed())
            }
            () = tokio::time::sleep(timeout) => {
                let _ = child.kill().await;
                // The readers are abandoned deliberately. A read error here would
                // be a consequence of the kill above rather than a cause, and the
                // timeout is the more specific answer. Aborting rather than
                // dropping the handles: dropping only detaches, which would leave
                // a task buffering a killed command's output after the caller has
                // given up on it.
                stdout_handle.abort();
                stderr_handle.abort();
                Err(CommandError::Timeout {
                    command: command.to_string(),
                    timeout,
                    working_directory,
                })
            }
            () = token.cancelled() => {
                let _ = child.kill().await;
                // Abandoned for the same reason as the timeout arm above.
                stdout_handle.abort();
                stderr_handle.abort();
                Err(CommandError::Cancelled {
                    command: command.to_string(),
                    working_directory,
                })
            }
        }
    }

    /// Return the platform-appropriate default shell path.
    ///
    /// - **Unix**: `/bin/sh`
    /// - **Windows**: the value of `COMSPEC` (usually `cmd.exe`)
    #[must_use]
    pub fn default_shell() -> &'static str {
        #[cfg(unix)]
        {
            "/bin/sh"
        }
        #[cfg(windows)]
        {
            // COMSPEC is always set on Windows; fall back to cmd.exe
            static SHELL: std::sync::OnceLock<String> = std::sync::OnceLock::new();
            SHELL.get_or_init(|| std::env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".to_string()))
        }
    }
}

impl CommandRunner for ShellCommandRunner {
    /// Check if a command executable exists on `PATH`
    ///
    /// Uses the `which` crate to perform a native PATH lookup without
    /// spawning a subprocess. This checks for filesystem executables
    /// only — shell builtins (e.g., `cd`, `test`) will not be found.
    /// This is the appropriate check for package manager prerequisites
    /// like `brew`, `npm`, or `apt`.
    ///
    /// # Arguments
    ///
    /// * `command` - The command name to check (e.g., "npm", "git", "python")
    ///
    /// # Returns
    ///
    /// `true` if an executable with the given name exists on `PATH`, `false` otherwise
    async fn is_command_available(&self, command: &str) -> bool {
        which::which(command).is_ok()
    }

    /// Execute a command using the default timeout
    ///
    /// Runs the specified shell command and waits for completion, using
    /// the default timeout configured for this runner instance.
    ///
    /// # Arguments
    ///
    /// * `command` - The shell command to execute
    ///
    /// # Errors
    ///
    /// Returns [`CommandError`] if:
    /// - The command cannot be started, or fails part-way through (IO error)
    /// - Either output stream cannot be read to the end
    ///   ([`CommandError::OutputReadFailed`]) — **including stderr**, on a command
    ///   whose correctness depends only on stdout
    /// - The command times out (exceeds default timeout)
    /// - The command is cancelled via `token`
    async fn execute(
        &self,
        command: &str,
        token: &CancellationToken,
    ) -> Result<CommandOutput, CommandError> {
        self.execute_with_timeout(command, self.default_timeout, token)
            .await
    }

    /// Execute a command with a specific timeout
    ///
    /// Runs the specified shell command and waits for completion within
    /// the given timeout duration. The command will be terminated if it
    /// doesn't complete in time.
    ///
    /// # Arguments
    ///
    /// * `command` - The shell command to execute
    /// * `timeout` - Maximum duration to wait for completion
    ///
    /// # Errors
    ///
    /// Returns [`CommandError`] if:
    /// - The command cannot be started, or fails part-way through (IO error)
    /// - Either output stream cannot be read to the end
    ///   ([`CommandError::OutputReadFailed`]) — **including stderr**, on a command
    ///   whose correctness depends only on stdout
    /// - The command times out before completion
    /// - The command is cancelled via `token`
    async fn execute_with_timeout(
        &self,
        command: &str,
        timeout: Duration,
        token: &CancellationToken,
    ) -> Result<CommandOutput, CommandError> {
        self.run_buffered(command, None, timeout, token).await
    }

    /// Execute a command in a specific working directory
    ///
    /// Identical to [`execute_with_timeout`](CommandRunner::execute_with_timeout)
    /// except that the child's current directory is `working_dir`.
    ///
    /// # Errors
    ///
    /// Returns [`CommandError`] if the command cannot be started (including when
    /// `working_dir` does not exist), fails part-way through, has either output
    /// stream fail to read to the end ([`CommandError::OutputReadFailed`] —
    /// **including stderr**, on a command whose correctness depends only on
    /// stdout), times out, or is cancelled.
    async fn execute_in_dir(
        &self,
        command: &str,
        working_dir: &Path,
        timeout: Duration,
        token: &CancellationToken,
    ) -> Result<CommandOutput, CommandError> {
        self.run_buffered(command, Some(working_dir), timeout, token)
            .await
    }

    /// Execute a command with streaming output processing
    ///
    /// Runs the command and streams stdout/stderr output through the provided
    /// channel as it becomes available. This allows real-time processing of
    /// command output, which is useful for long-running commands or when
    /// providing user feedback.
    ///
    /// A chunk is dropped rather than blocking the read loop when the receiver
    /// falls behind, and dropped outright once the receiver is gone, so the
    /// channel is best-effort; the returned [`CommandOutput`] always holds the
    /// whole output.
    ///
    /// # Arguments
    ///
    /// * `command` - The shell command to execute
    /// * `timeout` - Maximum duration to wait for completion
    /// * `output_sender` - Channel sender each chunk of output is sent to
    ///
    /// # Errors
    ///
    /// Returns [`CommandError`] if:
    /// - The command cannot be started, or fails part-way through (IO error)
    /// - The command times out before completion
    /// - The command is cancelled via `token`
    /// - Output stream handling fails
    async fn execute_streaming(
        &self,
        command: &str,
        timeout: Duration,
        output_sender: tokio::sync::mpsc::Sender<OutputChunk>,
        token: &CancellationToken,
    ) -> Result<CommandOutput, CommandError> {
        let start_time = Instant::now();
        let working_directory =
            std::env::current_dir().unwrap_or_else(|_| Path::new(".").to_path_buf());

        // Check for pre-cancellation before spawning
        if token.is_cancelled() {
            return Err(CommandError::Cancelled {
                command: command.to_string(),
                working_directory,
            });
        }

        let mut cmd = self.build_command(command, None);

        let mut child = cmd.spawn().map_err(|e| CommandError::IoError {
            command: command.to_string(),
            working_directory: working_directory.clone(),
            source: Arc::new(e),
        })?;

        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| CommandError::StdoutSpawn(command.to_string()))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| CommandError::StderrSpawn(command.to_string()))?;

        let mut stdout = tokio::io::BufReader::new(stdout);
        let mut stderr = tokio::io::BufReader::new(stderr);

        let mut full_stdout = Vec::new();
        let mut full_stderr = Vec::new();

        let mut stdout_buf = vec![0; 1024]; // Buffer of 1024 bytes
        let mut stderr_buf = vec![0; 1024]; // Buffer of 1024 bytes

        let mut stdout_done = false;
        let mut stderr_done = false;
        let mut process_done = false;
        let mut exit_status = None;

        let deadline = tokio::time::Instant::now() + timeout;

        loop {
            tokio::select! {
                result = stdout.read(&mut stdout_buf), if !stdout_done => {
                    if handle_chunked_read_result_streaming(result, &mut full_stdout, &mut stdout_buf, &output_sender, OutputChunk::Stdout)? {
                        stdout_done = true;
                    }
                },
                result = stderr.read(&mut stderr_buf), if !stderr_done => {
                    if handle_chunked_read_result_streaming(result, &mut full_stderr, &mut stderr_buf, &output_sender, OutputChunk::Stderr)? {
                        stderr_done = true;
                    }
                },
                status = child.wait(), if !process_done => {
                    exit_status = Some(status.map_err(|e| CommandError::IoError {
                        command: command.to_string(),
                        working_directory: working_directory.clone(),
                        source: Arc::new(e),
                    })?);
                    process_done = true;
                },
                () = tokio::time::sleep_until(deadline), if !process_done => {
                    let _ = child.kill().await;
                    return Err(CommandError::Timeout {
                        command: command.to_string(),
                        timeout,
                        working_directory,
                    });
                },
                () = token.cancelled(), if !process_done => {
                    let _ = child.kill().await;
                    return Err(CommandError::Cancelled {
                        command: command.to_string(),
                        working_directory,
                    });
                }
            }

            // Exit when process is done AND both streams are done
            if process_done && stdout_done && stderr_done {
                break;
            }
        }

        let duration = start_time.elapsed();
        Ok(CommandOutput {
            output: Output {
                // SAFETY: exit_status is guaranteed to be Some(_) at this point because
                // the loop only exits when process_done is true, which is only set to true
                // after exit_status is assigned Some(status) from child.wait()
                status: exit_status.unwrap(),
                stdout: full_stdout,
                stderr: full_stderr,
            },
            duration,
        })
    }
}

/// What a spawned reader task hands back once joined.
///
/// The outer `Result` is the join itself (the task panicked or was aborted); the
/// inner one is whether the pipe read to the end.
type JoinedRead = Result<std::io::Result<Vec<u8>>, tokio::task::JoinError>;

/// Read one of a child's pipes to the end, or fail.
///
/// **A partial buffer is dropped, never returned.** This function existing at all
/// is the fix for the shape it replaced, which discarded the error and returned
/// whatever had accumulated as though the command had produced exactly that.
///
/// `None` is not a failure: a pipe that was never captured has no output, which
/// is the same empty answer the previous shape gave.
async fn read_stream<R>(reader: Option<R>) -> std::io::Result<Vec<u8>>
where
    R: tokio::io::AsyncRead + Unpin + Send,
{
    let mut buf = Vec::new();
    if let Some(mut reader) = reader {
        reader.read_to_end(&mut buf).await?;
    }
    Ok(buf)
}

/// Turn a joined reader task into its bytes, or into a [`CommandError`].
///
/// A failed read and a reader task that did not finish are the same answer to
/// the caller — selfie does not have the command's output — so they share one
/// variant.
///
/// The [`JoinError`](tokio::task::JoinError) is read for its two booleans and
/// then **dropped**. It is never wrapped or rendered: a panic payload is
/// produced by the very task that was holding this command's output, so it can
/// be derived from a credential, and `OutputReadFailed`'s `Display` reaches
/// `PackageEvent::Completed`, the CLI, and the MCP server's JSON. The
/// replacement is a fixed `&'static str`, so no runtime data can reach it even
/// by accident later.
fn join_read(
    joined: JoinedRead,
    stream: OutputStream,
    command: &str,
    working_directory: &Path,
) -> Result<Vec<u8>, CommandError> {
    let source = match joined {
        Ok(Ok(bytes)) => return Ok(bytes),
        Ok(Err(io_error)) => Arc::new(io_error),
        Err(join_error) => {
            let reason: &'static str = if join_error.is_cancelled() {
                "output reader task was cancelled"
            } else {
                "output reader task panicked"
            };
            Arc::new(std::io::Error::other(reason))
        }
    };

    Err(CommandError::OutputReadFailed {
        command: command.to_string(),
        working_directory: working_directory.to_path_buf(),
        stream,
        source,
    })
}

/// Build the finished [`CommandOutput`], or the first read failure.
///
/// Split out of `run_buffered` so the step that decides whether a command's
/// output is trustworthy can be tested directly. Testing it only through the
/// runner leaves this wiring uncovered: the mechanism tests below would still
/// pass if a caller went back to ignoring what [`join_read`] returns.
///
/// stdout is checked first because it is the stream every consumer makes a
/// decision from — an executable path, a credential, a check verdict, a source
/// list. An exit status is not enough to report on its own: a command can exit 0
/// while the pipe carrying its answer failed.
fn finish(
    status: std::process::ExitStatus,
    joined_stdout: JoinedRead,
    joined_stderr: JoinedRead,
    command: &str,
    working_directory: &Path,
    duration: Duration,
) -> Result<CommandOutput, CommandError> {
    let stdout = join_read(
        joined_stdout,
        OutputStream::Stdout,
        command,
        working_directory,
    )?;
    let stderr = join_read(
        joined_stderr,
        OutputStream::Stderr,
        command,
        working_directory,
    )?;

    Ok(CommandOutput {
        output: Output {
            status,
            stdout,
            stderr,
        },
        duration,
    })
}

/// Handle the result of reading a chunk with real-time streaming
///
/// Processes the result of an async read operation, updating the full output
/// buffer and sending the chunk immediately for real-time streaming. The send
/// is non-blocking: a chunk is dropped if the channel is full or its receiver
/// is gone.
///
/// # Arguments
///
/// * `result` - Result of the read operation
/// * `full_output` - Buffer to accumulate complete output
/// * `buffer` - Read buffer containing the latest chunk
/// * `output_sender` - Sender channel for streaming chunks
/// * `output_type` - Function to wrap chunks as stdout or stderr
///
/// # Returns
///
/// Returns `Ok(true)` if EOF reached, `Ok(false)` to continue reading
///
/// # Errors
///
/// Returns [`CommandError`] if the read operation failed (IO error)
fn handle_chunked_read_result_streaming(
    result: Result<usize, tokio::io::Error>,
    full_output: &mut Vec<u8>,
    buffer: &mut [u8],
    output_sender: &tokio::sync::mpsc::Sender<OutputChunk>,
    output_type: fn(String) -> OutputChunk,
) -> Result<bool, CommandError> {
    match result {
        Ok(0) => Ok(true), // End of stream
        Ok(n) => {
            full_output.extend_from_slice(&buffer[..n]);
            let chunk = String::from_utf8_lossy(&buffer[..n]).to_string();
            // Send the chunk immediately for real-time streaming
            let _ = output_sender.try_send(output_type(chunk));
            // Note: Don't clear the buffer here - tokio reuses it for the next read
            Ok(false) // Continue reading
        }
        Err(e) => Err(CommandError::IoError {
            command: "streaming command".to_string(),
            working_directory: std::env::current_dir()
                .unwrap_or_else(|_| Path::new(".").to_path_buf()),
            source: Arc::new(e),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn token() -> CancellationToken {
        CancellationToken::new()
    }

    /// An `AsyncRead` that yields `bytes` and then fails.
    ///
    /// Stands in for a pipe that dies part-way through, which cannot be provoked
    /// reliably from a real child process.
    struct FailingReader {
        bytes: Vec<u8>,
        fail: bool,
    }

    impl FailingReader {
        fn failing_after(bytes: &[u8]) -> Self {
            Self {
                bytes: bytes.to_vec(),
                fail: true,
            }
        }

        /// The control: the same bytes, then a clean EOF.
        fn clean(bytes: &[u8]) -> Self {
            Self {
                bytes: bytes.to_vec(),
                fail: false,
            }
        }
    }

    impl tokio::io::AsyncRead for FailingReader {
        fn poll_read(
            mut self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
            buf: &mut tokio::io::ReadBuf<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            if !self.bytes.is_empty() {
                let taken = std::mem::take(&mut self.bytes);
                buf.put_slice(&taken);
                return std::task::Poll::Ready(Ok(()));
            }
            if self.fail {
                return std::task::Poll::Ready(Err(std::io::Error::other("pipe died")));
            }
            std::task::Poll::Ready(Ok(())) // EOF
        }
    }

    fn joined_ok(bytes: &[u8]) -> JoinedRead {
        Ok(Ok(bytes.to_vec()))
    }

    fn joined_io_error() -> JoinedRead {
        Ok(Err(std::io::Error::other("pipe died")))
    }

    /// A real `JoinError` from a task that panicked with `payload`.
    async fn joined_panic(payload: &'static str) -> JoinedRead {
        // The default hook would print the payload to the test log, which for the
        // leak test below is the very thing under examination.
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        let joined = tokio::spawn(async move { panic!("{payload}") }).await;
        std::panic::set_hook(previous);
        joined.map(|()| Ok(Vec::new()))
    }

    fn exit_status(code: i32) -> std::process::ExitStatus {
        use std::os::unix::process::ExitStatusExt as _;
        std::process::ExitStatus::from_raw(code << 8)
    }

    // ---- T1: a failed read is an error, not a short buffer -------------------

    #[tokio::test]
    async fn a_failed_stdout_read_is_an_error_not_a_short_buffer() {
        let result = read_stream(Some(FailingReader::failing_after(b"partial"))).await;

        assert!(
            result.is_err(),
            "a read that failed part-way returned {result:?} as though it were the whole output"
        );
    }

    #[tokio::test]
    async fn a_read_that_reaches_eof_returns_every_byte() {
        // Control for the test above: without this, a `read_stream` that always
        // failed would pass it.
        let bytes = read_stream(Some(FailingReader::clean(b"partial")))
            .await
            .unwrap();

        assert_eq!(bytes, b"partial");
    }

    #[tokio::test]
    async fn an_uncaptured_pipe_is_empty_rather_than_an_error() {
        let bytes = read_stream(Option::<FailingReader>::None).await.unwrap();

        assert!(bytes.is_empty());
    }

    // ---- T2/T3/T4: carrying the failure across the join ----------------------

    #[tokio::test]
    async fn a_failing_read_is_reported_as_output_read_failed_naming_the_stream() {
        let error = join_read(
            joined_io_error(),
            OutputStream::Stderr,
            "op read x",
            Path::new("/pkg"),
        )
        .unwrap_err();

        match &error {
            CommandError::OutputReadFailed {
                command,
                working_directory,
                stream,
                ..
            } => {
                assert_eq!(*stream, OutputStream::Stderr);
                assert_eq!(command, "op read x");
                assert_eq!(working_directory, Path::new("/pkg"));
            }
            other => panic!("expected OutputReadFailed, got: {other:?}"),
        }
        assert!(error.to_string().contains("stderr"), "{error}");
        assert!(error.to_string().contains("op read x"), "{error}");
    }

    #[tokio::test]
    async fn a_panicking_reader_task_is_an_error_not_an_empty_buffer() {
        let joined = joined_panic("boom").await;

        let result = join_read(joined, OutputStream::Stdout, "cmd", Path::new("/pkg"));

        assert!(
            matches!(result, Err(CommandError::OutputReadFailed { .. })),
            "a reader task that panicked was reported as empty output: {result:?}"
        );
    }

    #[tokio::test]
    async fn a_panicking_reader_does_not_forward_its_panic_payload() {
        // The panic payload is produced by the task holding the command's
        // output, so it can be derived from a credential. `OutputReadFailed`'s
        // `Display` reaches `PackageEvent::Completed`, the CLI, and the MCP
        // JSON, so the payload must not survive the join.
        const SECRET: &str = "hunter2-Zk9xQw-vault-token";

        let joined = joined_panic(SECRET).await;

        let error = join_read(joined, OutputStream::Stdout, "cmd", Path::new("/pkg")).unwrap_err();
        let rendered = error.to_string();

        assert!(
            !rendered.contains(SECRET),
            "the panic payload was forwarded: {rendered}"
        );
        // Positive control: without this the assertion above passes against an
        // error that renders nothing at all.
        assert!(
            rendered.contains("reader task panicked"),
            "expected the fixed reason, got: {rendered}"
        );
    }

    #[tokio::test]
    async fn a_successful_read_still_returns_the_bytes() {
        // Control for the two tests above: they would pass against a `join_read`
        // that failed unconditionally.
        let bytes = join_read(
            joined_ok(b"hello"),
            OutputStream::Stdout,
            "cmd",
            Path::new("/pkg"),
        )
        .unwrap();

        assert_eq!(bytes, b"hello");
    }

    // ---- The wiring that builds the finished output --------------------------

    #[tokio::test]
    async fn finish_builds_the_output_when_both_streams_read() {
        let output = finish(
            exit_status(0),
            joined_ok(b"out"),
            joined_ok(b"err"),
            "cmd",
            Path::new("/pkg"),
            Duration::ZERO,
        )
        .unwrap();

        assert_eq!(output.stdout(), b"out");
        assert_eq!(output.stderr(), b"err");
        assert!(output.is_success());
    }

    #[tokio::test]
    async fn finish_refuses_to_report_output_whose_stdout_read_failed() {
        // Exit 0 with a failed stdout read is the dangerous case: the status says
        // the command succeeded, and the bytes are not what it produced.
        let result = finish(
            exit_status(0),
            joined_io_error(),
            joined_ok(b"err"),
            "which ripgrep",
            Path::new("/pkg"),
            Duration::ZERO,
        );

        assert!(
            matches!(
                result,
                Err(CommandError::OutputReadFailed {
                    stream: OutputStream::Stdout,
                    ..
                })
            ),
            "{result:?}"
        );
    }

    #[tokio::test]
    async fn finish_refuses_to_report_output_whose_stderr_read_failed() {
        let result = finish(
            exit_status(0),
            joined_ok(b"out"),
            joined_io_error(),
            "cmd",
            Path::new("/pkg"),
            Duration::ZERO,
        );

        assert!(
            matches!(
                result,
                Err(CommandError::OutputReadFailed {
                    stream: OutputStream::Stderr,
                    ..
                })
            ),
            "{result:?}"
        );
    }

    // ---- T5: the deadlock this shape exists to avoid -------------------------

    #[tokio::test]
    async fn a_command_writing_past_the_pipe_buffer_on_both_streams_completes() {
        // The OS pipe buffer is ~64KB. A child that fills both pipes blocks until
        // something drains them, so a runner that reads only after `wait()`
        // returns — or that drains one stream fully before the other — deadlocks.
        // Nothing in this repository covered that: the largest existing output
        // test writes ~9KB, to stdout only.
        //
        // The timeout is what turns a deadlock into a failed assertion instead of
        // a hung suite.
        let runner =
            ShellCommandRunner::new(ShellCommandRunner::default_shell(), Duration::from_secs(30));
        let command = "for i in $(seq 1 4000); do \
                       echo 'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa'; \
                       echo 'bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb' >&2; \
                       done";

        let output = tokio::time::timeout(
            Duration::from_secs(20),
            runner.execute_with_timeout(command, Duration::from_secs(20), &token()),
        )
        .await
        .expect("reading both pipes deadlocked")
        .expect("command failed");

        // Positive control: without these the test passes even if the command
        // never exceeded the pipe buffer, which is the only thing it is for.
        assert!(
            output.stdout().len() > 200_000,
            "stdout was only {} bytes, under the pipe buffer",
            output.stdout().len()
        );
        assert!(
            output.stderr().len() > 200_000,
            "stderr was only {} bytes, under the pipe buffer",
            output.stderr().len()
        );
    }

    // These tests will actually run commands on the system
    // They could be skipped in CI environments if necessary
    #[tokio::test]
    async fn test_shell_command_runner_basic() {
        let runner =
            ShellCommandRunner::new(ShellCommandRunner::default_shell(), Duration::from_secs(10));
        let token = token();

        // Test a basic echo command
        let result = runner.execute("echo hello", &token).await;
        assert!(result.is_ok());
        let output = result.unwrap();
        assert!(output.stdout_str().contains("hello"));
        assert!(output.is_success());

        // Test command failure
        let result = runner.execute("exit 1", &token).await;
        assert!(result.is_ok());
        let output = result.unwrap();
        assert!(!output.is_success());
        assert_eq!(output.exit_code(), 1);
    }

    #[tokio::test]
    async fn test_command_availability() {
        let runner =
            ShellCommandRunner::new(ShellCommandRunner::default_shell(), Duration::from_secs(10));

        // "ls" is a filesystem binary on all target platforms
        assert!(runner.is_command_available("ls").await);

        // A random string should not be a valid command
        let random_cmd = "xyzabc123notarealcommand";
        assert!(!runner.is_command_available(random_cmd).await);
    }

    // This test relies on timing and could be flaky
    // Consider skipping or adjusting in CI environments
    #[tokio::test]
    async fn test_timeout() {
        let runner = ShellCommandRunner::new(
            ShellCommandRunner::default_shell(),
            Duration::from_millis(100),
        );
        let token = token();

        // Command that should timeout (sleep for 1s)
        let result = runner
            .execute_with_timeout("sleep 1", Duration::from_millis(10), &token)
            .await;
        assert!(matches!(result, Err(CommandError::Timeout { .. })));
    }

    // Error handling tests
    #[tokio::test]
    async fn test_command_timeout_error() {
        let runner = ShellCommandRunner::new(
            ShellCommandRunner::default_shell(),
            Duration::from_millis(50),
        );
        let token = token();

        // Create a command that will timeout
        let result = runner
            .execute_with_timeout("sleep 1", Duration::from_millis(10), &token)
            .await;

        assert!(result.is_err());
        let error = result.unwrap_err();
        match error {
            CommandError::Timeout { .. } => {
                // Expected timeout error
            }
            _ => panic!("Expected CommandError::Timeout, got: {error:?}"),
        }
    }

    #[tokio::test]
    async fn test_command_io_error() {
        let runner =
            ShellCommandRunner::new(ShellCommandRunner::default_shell(), Duration::from_secs(5));
        let token = token();

        // Try to execute a command that doesn't exist
        let result = runner
            .execute("nonexistent_command_12345_xyz", &token)
            .await;

        // Command might succeed but with non-zero exit code, or fail
        if let Ok(output) = result {
            // If command executes, it should fail (non-zero exit code)
            assert!(!output.is_success());
        }
        // If result is Err, that's also acceptable for this test
    }

    #[tokio::test]
    async fn test_command_permission_denied() {
        let runner =
            ShellCommandRunner::new(ShellCommandRunner::default_shell(), Duration::from_secs(5));
        let token = token();

        // Try to access a file that should not be accessible
        let result = runner
            .execute(
                "cat /root/.ssh/id_rsa 2>/dev/null || echo 'permission denied'",
                &token,
            )
            .await;

        // This should either succeed with "permission denied" message or fail
        // Either way, we're testing that the command runner handles the scenario
        if let Ok(output) = result {
            assert!(
                output.stdout_str().contains("permission denied")
                    || !output.stderr_str().is_empty()
            );
        }
        // If it fails, that's also acceptable for this test
    }

    #[tokio::test]
    async fn test_command_invalid_syntax() {
        let runner =
            ShellCommandRunner::new(ShellCommandRunner::default_shell(), Duration::from_secs(5));
        let token = token();

        // Try to execute a command with invalid syntax
        let result = runner
            .execute("if [ 1 -eq 1 ; then echo 'unclosed'", &token)
            .await;

        // This should fail due to invalid shell syntax
        if let Ok(output) = result {
            // Some shells might handle this gracefully
            assert!(!output.is_success());
        }
        // If it errors, that's also expected
    }

    #[tokio::test]
    async fn test_error_display_formatting() {
        // Test that our error types format correctly
        let timeout_error = CommandError::Timeout {
            command: "test-command".to_string(),
            timeout: Duration::from_millis(100),
            working_directory: PathBuf::from("/tmp"),
        };
        assert!(
            timeout_error
                .to_string()
                .contains("Command timed out after 100ms")
        );
        assert!(timeout_error.to_string().contains("test-command"));

        let io_error = std::io::Error::other("test error");
        let cmd_error = CommandError::IoError {
            command: "test-command".to_string(),
            working_directory: PathBuf::from("/tmp"),
            source: Arc::new(io_error),
        };
        assert!(cmd_error.to_string().contains("test-command"));

        let cancelled_error = CommandError::Cancelled {
            command: "test-command".to_string(),
            working_directory: PathBuf::from("/tmp"),
        };
        assert!(cancelled_error.to_string().contains("Command cancelled"));
        assert!(cancelled_error.to_string().contains("test-command"));

        let stdout_error = CommandError::StdoutSpawn("stdout issue".to_string());
        assert_eq!(
            stdout_error.to_string(),
            "Failed spawning stdout during command: stdout issue"
        );

        let stderr_error = CommandError::StderrSpawn("stderr issue".to_string());
        assert_eq!(
            stderr_error.to_string(),
            "Failed spawning stderr during command: stderr issue"
        );
    }

    #[tokio::test]
    async fn test_command_with_large_output() {
        let runner =
            ShellCommandRunner::new(ShellCommandRunner::default_shell(), Duration::from_secs(5));
        let token = token();

        // Generate a large amount of output to test buffering
        let result = runner
            .execute("for i in $(seq 1 1000); do echo \"Line $i\"; done", &token)
            .await;

        assert!(result.is_ok());
        let output = result.unwrap();
        assert!(output.is_success());
        assert!(output.stdout_str().lines().count() >= 1000);
    }

    #[tokio::test]
    async fn test_command_output_methods() {
        let runner =
            ShellCommandRunner::new(ShellCommandRunner::default_shell(), Duration::from_secs(5));
        let token = token();

        // Test that our output methods work correctly
        let result = runner.execute("echo 'test output'", &token).await;

        assert!(result.is_ok());
        let output = result.unwrap();
        assert!(output.is_success());
        assert!(output.stdout_str().contains("test output"));

        // Test that stderr_str() method exists and returns a string
        let _stderr = output.stderr_str(); // Just verify the method works
    }

    #[tokio::test]
    async fn test_command_exit_code_handling() {
        let runner =
            ShellCommandRunner::new(ShellCommandRunner::default_shell(), Duration::from_secs(5));
        let token = token();

        // Command that exits with non-zero status
        let result = runner.execute("exit 42", &token).await;

        assert!(result.is_ok());
        let output = result.unwrap();
        assert!(!output.is_success());
        assert_eq!(output.exit_code(), 42);
    }

    #[tokio::test]
    async fn execute_in_dir_runs_the_command_there() {
        let runner =
            ShellCommandRunner::new(ShellCommandRunner::default_shell(), Duration::from_secs(5));
        let dir = tempfile::tempdir().unwrap();
        // Canonicalize: on macOS the temp dir is under a symlinked /var, and `pwd`
        // in a shell reports the resolved path.
        let expected = dir.path().canonicalize().unwrap();

        let output = runner
            .execute_in_dir("pwd", dir.path(), Duration::from_secs(5), &token())
            .await
            .unwrap();

        assert!(output.is_success());
        assert_eq!(
            output.stdout_str().trim(),
            expected.to_string_lossy(),
            "command should run in the directory it was given"
        );
    }

    #[tokio::test]
    async fn execute_in_dir_resolves_relative_paths_against_that_directory() {
        let runner =
            ShellCommandRunner::new(ShellCommandRunner::default_shell(), Duration::from_secs(5));
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("marker.txt"), "found-me").unwrap();

        let output = runner
            .execute_in_dir(
                "cat marker.txt",
                dir.path(),
                Duration::from_secs(5),
                &token(),
            )
            .await
            .unwrap();

        assert!(output.is_success());
        assert_eq!(output.stdout_str(), "found-me");
    }

    #[tokio::test]
    async fn execute_in_dir_reports_that_directory_on_failure() {
        let runner =
            ShellCommandRunner::new(ShellCommandRunner::default_shell(), Duration::from_secs(5));
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("does-not-exist");

        let error = runner
            .execute_in_dir("echo hi", &missing, Duration::from_secs(5), &token())
            .await
            .unwrap_err();

        match error {
            CommandError::IoError {
                working_directory, ..
            } => assert_eq!(working_directory, missing),
            other => panic!("Expected IoError for a missing working directory, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn execute_in_dir_reports_non_zero_exit_as_output_not_error() {
        let runner =
            ShellCommandRunner::new(ShellCommandRunner::default_shell(), Duration::from_secs(5));
        let dir = tempfile::tempdir().unwrap();

        let output = runner
            .execute_in_dir("exit 3", dir.path(), Duration::from_secs(5), &token())
            .await
            .unwrap();

        assert!(!output.is_success());
        assert_eq!(output.exit_code(), 3);
    }

    #[tokio::test]
    async fn execute_in_dir_honors_its_timeout() {
        let runner =
            ShellCommandRunner::new(ShellCommandRunner::default_shell(), Duration::from_secs(30));
        let dir = tempfile::tempdir().unwrap();

        let result = runner
            .execute_in_dir("sleep 1", dir.path(), Duration::from_millis(10), &token())
            .await;

        assert!(matches!(result, Err(CommandError::Timeout { .. })));
    }

    #[tokio::test]
    async fn execute_with_timeout_still_inherits_the_current_directory() {
        let runner =
            ShellCommandRunner::new(ShellCommandRunner::default_shell(), Duration::from_secs(5));
        let expected = std::env::current_dir().unwrap().canonicalize().unwrap();

        let output = runner
            .execute_with_timeout("pwd", Duration::from_secs(5), &token())
            .await
            .unwrap();

        assert_eq!(output.stdout_str().trim(), expected.to_string_lossy());
    }

    #[tokio::test]
    async fn test_pre_cancelled_token_returns_cancelled_error() {
        let runner =
            ShellCommandRunner::new(ShellCommandRunner::default_shell(), Duration::from_secs(5));
        let token = CancellationToken::new();
        token.cancel(); // Pre-cancel

        let result = runner.execute("echo should_not_run", &token).await;
        assert!(matches!(result, Err(CommandError::Cancelled { .. })));
    }

    #[tokio::test]
    async fn test_pre_cancelled_token_streaming_returns_cancelled_error() {
        let runner =
            ShellCommandRunner::new(ShellCommandRunner::default_shell(), Duration::from_secs(5));
        let token = CancellationToken::new();
        token.cancel(); // Pre-cancel

        let (tx, _rx) = tokio::sync::mpsc::channel(10);
        let result = runner
            .execute_streaming("echo should_not_run", Duration::from_secs(5), tx, &token)
            .await;
        assert!(matches!(result, Err(CommandError::Cancelled { .. })));
    }
}
