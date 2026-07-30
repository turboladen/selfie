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

use futures::{Stream, StreamExt as _};
use tokio::{io::AsyncReadExt, process::Command};
use tokio_util::{
    bytes::{Bytes, BytesMut},
    codec::{Decoder, FramedRead},
    sync::CancellationToken,
};

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
                match status {
                    Ok(status) => {
                        // Both readers are already running, so joining them
                        // concurrently rather than one after the other is neutral
                        // for progress. It is done this way so that a failure on
                        // one stream still reaps the other instead of leaving a
                        // detached task behind.
                        let (out, err) = tokio::join!(stdout_handle, stderr_handle);
                        finish(status, out, err, command, &working_directory, start_time.elapsed())
                    }
                    // Aborted for the same reason as the timeout and cancellation
                    // arms below: without a status there is no output to report,
                    // and dropping the handles would only detach them.
                    Err(e) => {
                        stdout_handle.abort();
                        stderr_handle.abort();
                        Err(CommandError::IoError {
                            command: command.to_string(),
                            working_directory: working_directory.clone(),
                            source: Arc::new(e),
                        })
                    }
                }
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

        let mut full_stdout = Vec::new();
        let mut full_stderr = Vec::new();

        let frames = futures::stream::select(
            tagged_frames(stdout, OutputStream::Stdout),
            tagged_frames(stderr, OutputStream::Stderr),
        );

        let outcome = tokio::select! {
            // Biased so a command that finished within its budget is reported as
            // finished. Unbiased, `select!` picks at random when both arms are
            // ready at once, and a token cancelled in the same instant a package
            // finished installing would report it as cancelled about half the
            // time. The old shape could not do that: its cancellation arm was
            // guarded `if !process_done`, so a completed command always won.
            biased;
            result = tokio::time::timeout(
                timeout,
                drain_and_wait(
                    &mut child,
                    frames,
                    &mut full_stdout,
                    &mut full_stderr,
                    &output_sender,
                    command,
                    &working_directory,
                ),
            ) => result.unwrap_or_else(|_elapsed| Err(CommandError::Timeout {
                command: command.to_string(),
                timeout,
                working_directory: working_directory.clone(),
            })),
            () = token.cancelled() => Err(CommandError::Cancelled {
                command: command.to_string(),
                working_directory: working_directory.clone(),
            }),
        };

        // Every failure here abandons a child that may still be running: a
        // timeout and a cancellation by definition, and a failed read because
        // nothing is draining the pipe it died on. Killing one that already
        // exited is a no-op rather than an error — `Child::start_kill` returns
        // `Ok` for a reaped child (tokio `process/mod.rs`) — so this needs no
        // guard on which failure it was.
        //
        // `a_timed_out_streaming_command_does_not_leave_its_child_running`
        // covers the timeout arm. The read-failure arm is the one no test
        // reaches: it needs a genuine pipe failure from a real child.
        if outcome.is_err() {
            let _ = child.kill().await;
        }

        Ok(CommandOutput {
            output: Output {
                status: outcome?,
                stdout: full_stdout,
                stderr: full_stderr,
            },
            duration: start_time.elapsed(),
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
/// The [`JoinError`](tokio::task::JoinError) is inspected only for whether the
/// task was cancelled, and is then **dropped**. It is never wrapped or rendered:
/// a panic payload is produced by the very task that was holding this command's
/// output, so it can be derived from a credential, and `OutputReadFailed`'s
/// `Display` reaches `PackageEvent::Completed`, the CLI, and the MCP server's
/// JSON. The replacement is a fixed `&'static str`, so no runtime data can reach
/// it even by accident later.
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
/// output is trustworthy can be tested directly, on inputs a real child process
/// cannot be made to produce on demand.
///
/// **That makes this function's own logic testable; it does not cover the call
/// site.** Going back to `unwrap_or_default()` where `run_buffered` calls this
/// would still fail no test: reaching it needs a genuine pipe failure from a
/// real child, and the tests that exercise the consumers substitute a fake
/// runner and never construct a `ShellCommandRunner` at all. Read the tests
/// below as covering the decision, not the wiring into it.
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

/// Cuts a pipe into frames whose decoding does not depend on where the cuts fell.
///
/// The property relied on is that framing never *adds* a replacement character:
/// `lossy(a) + lossy(b) == lossy(a + b)` for any two consecutive frames. That is
/// what makes it safe to decode each frame on its own and relay it.
///
/// Note it is **not** the stronger "a frame never ends inside a character". A
/// frame can end inside an *invalid* sequence, at the boundary of its maximal
/// subpart — `[0x61, 0xF4]` followed by `0xF5` yields a frame of just `[0xF4]`.
/// That costs nothing, because `0xF4` decodes to one `U+FFFD` framed alone or
/// with its neighbours. What never happens is a *valid* character being split,
/// which is the case that would turn one character into two `U+FFFD`.
///
/// The boundary rule is `std`'s, not this module's: [`std::str::from_utf8`]
/// reports both how far the input was valid ([`Utf8Error::valid_up_to`]) and
/// whether the trailing bytes were merely *incomplete* or genuinely *invalid*
/// ([`Utf8Error::error_len`]). This is the ~15-line adapter from that onto
/// [`Decoder`]; it does not parse UTF-8 itself.
///
/// Framing at a fixed byte count instead — which is what the read loop this
/// replaced did, at 1024 bytes — splits any multi-byte character straddling the
/// cut into **two** `U+FFFD`s, one at the end of a chunk and one at the start of
/// the next. Both halves then reach the terminal and the MCP server's JSON,
/// because the relayed chunk is decoded here and nothing downstream can undo it.
///
/// Yields the **raw bytes**, not a `String`, so the same frames can be
/// accumulated byte-exactly into [`CommandOutput`] while only the relayed copy
/// is decoded. Yielding text would make a command's captured stdout lossy under
/// streaming and byte-exact everywhere else.
///
/// [`Utf8Error::valid_up_to`]: std::str::Utf8Error::valid_up_to
/// [`Utf8Error::error_len`]: std::str::Utf8Error::error_len
struct Utf8Boundary;

impl Utf8Boundary {
    /// How many leading bytes of `src` can be handed over without cutting a
    /// character in half. `0` means "nothing yet — read more".
    fn boundary(src: &[u8]) -> usize {
        match std::str::from_utf8(src) {
            Ok(_) => src.len(),
            Err(e) => match e.error_len() {
                // The tail is a prefix of a character whose remaining bytes have
                // not arrived. Keeping it buffered is the whole point of this type.
                None => e.valid_up_to(),
                // Genuinely invalid, and no later byte can repair it. Hand it over
                // with the valid prefix so it becomes one `U+FFFD` in place and
                // the scan resumes after it, rather than stalling the stream.
                Some(invalid) => e.valid_up_to() + invalid,
            },
        }
    }
}

impl Decoder for Utf8Boundary {
    type Item = Bytes;

    /// Required to be [`From<std::io::Error>`], so it cannot be [`Infallible`].
    ///
    /// **Nothing here constructs one**, and that is load-bearing rather than
    /// incidental: this error would become [`CommandError::OutputReadFailed`]'s
    /// `source`, whose `Display` reaches `PackageEvent::Completed`, the CLI and
    /// the MCP server's JSON — and it is the one place on the streaming path
    /// where a constructible error and the process's raw bytes are in scope
    /// together. Should a fallible case ever be added here, its message must be
    /// a fixed `&'static str` and must never embed `src`.
    ///
    /// `decode_returns_ok_for_every_input` asserts the stronger property the
    /// code has today: there is no input for which this returns `Err` at all.
    ///
    /// [`Infallible`]: std::convert::Infallible
    type Error = std::io::Error;

    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<Bytes>, Self::Error> {
        let take = Self::boundary(src);
        if take == 0 {
            // Either nothing buffered, or only a partial character. Both mean
            // "ask for more bytes"; `decode_eof` handles the case where there
            // are none left to ask for.
            return Ok(None);
        }
        Ok(Some(src.split_to(take).freeze()))
    }

    fn decode_eof(&mut self, src: &mut BytesMut) -> Result<Option<Bytes>, Self::Error> {
        if let Some(frame) = self.decode(src)? {
            return Ok(Some(frame));
        }
        if src.is_empty() {
            return Ok(None);
        }
        // A partial character at EOF: nothing is coming to complete it. Hand it
        // over so it decodes to `U+FFFD` rather than vanishing — without this,
        // `FramedRead` reports "bytes remaining on stream" and the tail is lost.
        Ok(Some(src.split_to(src.len()).freeze()))
    }
}

/// One frame of a child's output, tagged with the pipe it came from.
///
/// The tag rides on the **error** as well as the value. Merging the two pipes
/// erases which one an item came from, and
/// [`CommandError::OutputReadFailed`] names the stream that failed.
type TaggedFrame = Result<(OutputStream, Bytes), (OutputStream, std::io::Error)>;

/// Frame one of a child's pipes, tagging every frame with `stream`.
fn tagged_frames<R>(reader: R, stream: OutputStream) -> impl Stream<Item = TaggedFrame>
where
    R: tokio::io::AsyncRead,
{
    FramedRead::new(reader, Utf8Boundary).map(move |frame| match frame {
        Ok(bytes) => Ok((stream, bytes)),
        Err(source) => Err((stream, source)),
    })
}

/// Drain `frames` while the child runs, and give back its exit status.
///
/// # Why `try_join!` and not `join!`
///
/// The pump is the only thing draining the pipes, and a child blocked writing
/// to a full one cannot exit until something does. So a `join!` — which waits
/// for *both* — turns a read failure into a wait for the entire remaining
/// timeout, then reports it as [`CommandError::Timeout`]: a prompt and
/// correctly-typed failure replaced by a slow and wrong one. `try_join!`
/// returns the read failure as soon as it happens, and the caller kills the
/// child it left behind.
///
/// The success path still requires both: the status *and* both pipes at EOF.
/// That is what avoids the ~64KB pipe-buffer deadlock, and it is also why a
/// grandchild holding the pipe open runs out the timeout rather than hanging
/// past it — there is no phase here that the deadline does not cover.
///
/// # Errors
///
/// Returns [`CommandError::OutputReadFailed`] naming the pipe that failed, or
/// [`CommandError::IoError`] if waiting on the child itself failed.
async fn drain_and_wait<S>(
    child: &mut tokio::process::Child,
    frames: S,
    full_stdout: &mut Vec<u8>,
    full_stderr: &mut Vec<u8>,
    output_sender: &tokio::sync::mpsc::Sender<OutputChunk>,
    command: &str,
    working_directory: &Path,
) -> Result<std::process::ExitStatus, CommandError>
where
    S: Stream<Item = TaggedFrame>,
{
    let pump = async {
        let mut frames = std::pin::pin!(frames);

        while let Some(frame) = frames.next().await {
            let (stream, bytes) =
                frame.map_err(|(stream, source)| CommandError::OutputReadFailed {
                    command: command.to_string(),
                    working_directory: working_directory.to_path_buf(),
                    stream,
                    source: Arc::new(source),
                })?;

            // The captured output accumulates the raw bytes...
            match stream {
                OutputStream::Stdout => full_stdout.extend_from_slice(&bytes),
                OutputStream::Stderr => full_stderr.extend_from_slice(&bytes),
            }

            // ...and only the relayed copy is decoded. `Utf8Boundary` frames so
            // that decoding one frame at a time gives the same text as decoding
            // the whole stream, so the only bytes this replaces are ones the
            // command itself emitted invalid.
            let text = String::from_utf8_lossy(&bytes).into_owned();
            let chunk = match stream {
                OutputStream::Stdout => OutputChunk::Stdout(text),
                OutputStream::Stderr => OutputChunk::Stderr(text),
            };

            // Best-effort, as the port documents: a chunk is dropped rather than
            // stalling the only thing draining the child's pipes. The captured
            // output above is unaffected.
            let _ = output_sender.try_send(chunk);
        }

        Ok(())
    };

    let wait = async {
        child.wait().await.map_err(|e| CommandError::IoError {
            command: command.to_string(),
            working_directory: working_directory.to_path_buf(),
            source: Arc::new(e),
        })
    };

    let (status, ()) = tokio::try_join!(wait, pump)?;
    Ok(status)
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
                // Only what fits: `put_slice` panics past the remaining capacity,
                // which today's 7-byte fixtures never reach but a realistic one
                // would. The rest is handed over on the next poll.
                let n = self.bytes.len().min(buf.remaining());
                let taken: Vec<u8> = self.bytes.drain(..n).collect();
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

    /// Serializes the panic-hook swap below.
    ///
    /// The hook is process-global and the lib test binary runs tests in parallel,
    /// so two callers can interleave their take/set/restore and leave the
    /// silencing hook installed for the rest of the run. That cannot cause a
    /// false pass, but it would swallow the panic output of some later genuine
    /// failure, which is a trap for whoever hits it.
    ///
    /// `tokio`'s mutex rather than `std`'s because the guarded region spans the
    /// `.await` on the spawned task, which is exactly what `std`'s must not do.
    static PANIC_HOOK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    /// A real `JoinError` from a task that panicked with `payload`.
    async fn joined_panic(payload: &'static str) -> JoinedRead {
        // The default hook would print the payload to the test log, which for the
        // leak test below is the very thing under examination.
        let guard = PANIC_HOOK.lock().await;
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        let joined = tokio::spawn(async move { panic!("{payload}") }).await;
        std::panic::set_hook(previous);
        drop(guard);
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

    /// Writes well past the ~64KB pipe buffer on **both** streams.
    ///
    /// Shared by the buffered and streaming deadlock tests so the two cannot
    /// drift into measuring different amounts of output.
    const BOTH_PIPES_PAST_THE_BUFFER: &str = "for i in $(seq 1 4000); do \
         echo 'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa'; \
         echo 'bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb' >&2; \
         done";

    /// 1023 ASCII bytes, then a two-byte `é`, then an invalid `0xFF`, then more
    /// ASCII.
    ///
    /// The `é` straddles the 1024-byte boundary the old read loop cut on. The
    /// `0xFF` is what makes a byte-exactness assertion able to fail: a *valid*
    /// character that gets split still round-trips through
    /// `String::from_utf8_lossy(..).as_bytes()` byte for byte, so a fixture
    /// without an invalid byte cannot detect output that was decoded before it
    /// was captured.
    /// **Octal, not `\\xNN`.** Hex escapes are a bash/BSD extension; POSIX
    /// specifies `\\ooo`, and `/bin/sh` is dash on Debian and Ubuntu, whose
    /// `printf` emits `\\xc3` as five literal characters. That made this fixture
    /// produce no split character at all and the test fail for a reason having
    /// nothing to do with the decoder.
    const SPLIT_CHARACTER: &str = "printf 'a%.0s' $(seq 1 1023); printf '\\303\\251'; \
         printf '\\377'; printf 'b%.0s' $(seq 1 100)";

    /// Exactly what [`SPLIT_CHARACTER`] writes to stdout.
    fn split_character_bytes() -> Vec<u8> {
        let mut expected = vec![b'a'; 1023];
        expected.extend_from_slice("é".as_bytes());
        expected.push(0xff);
        expected.extend(std::iter::repeat_n(b'b', 100));
        expected
    }

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
        let command = BOTH_PIPES_PAST_THE_BUFFER;

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

    /// Is a process whose command line contains `marker` still running?
    ///
    /// **The marker has to be the command the shell execs**, not a comment
    /// beside it. `sh -c 'sleep 30 # marker'` replaces the shell with `sleep 30`
    /// and the comment is gone, so `pgrep -f marker` finds nothing whether or not
    /// the child was killed — a test written that way passes either way. An
    /// unusual sleep duration is the marker instead: it survives the exec because
    /// it *is* the exec'd command.
    ///
    /// **The callers run it through `exec`, and that is load-bearing.** What the
    /// runner promises is that it kills its *direct child*; what this function
    /// observes is that no process is running the marker. Those are the same
    /// claim only when the shell has replaced itself with the marker rather than
    /// forking it. Replacing itself is what `exec` is specified to do, so this
    /// holds on every POSIX shell — whereas relying on a shell to *choose* to
    /// exec a lone command makes the test depend on an optimization. `bash` and
    /// `dash` differ there, which is how a version of this without `exec` passed
    /// on macOS and failed on Ubuntu, reporting a grandchild the runner never
    /// promised to kill (that leak is real, tracked separately, and deliberately
    /// out of scope here).
    fn a_process_matching(marker: &str) -> bool {
        let found = std::process::Command::new("pgrep")
            // `-x` anchors: the whole command line must be the marker, so an
            // unrelated process merely *containing* it - a shell running this
            // very test, an editor, a grep - is not mistaken for the child.
            // The dot is escaped because these patterns are regexes.
            .args(["-x", "-f", &marker.replace('.', "\\.")])
            .output()
            .expect("pgrep is not available");
        !found.stdout.is_empty()
    }

    /// Kill anything left over, so a failing assertion does not also leak a
    /// process into the developer's session.
    fn kill_processes_matching(marker: &str) {
        let _ = std::process::Command::new("pkill")
            .args(["-x", "-f", &marker.replace('.', "\\.")])
            .output();
    }

    #[tokio::test]
    async fn a_timed_out_streaming_command_does_not_leave_its_child_running() {
        // The streaming half of the test above. Distinct markers so the two
        // cannot see each other's child when the suite runs them in parallel.
        const MARKER: &str = "sleep 33.72";
        let runner =
            ShellCommandRunner::new(ShellCommandRunner::default_shell(), Duration::from_secs(30));
        let (tx, _rx) = tokio::sync::mpsc::channel(10);

        let result = runner
            .execute_streaming(
                &format!("exec {MARKER}"),
                Duration::from_millis(300),
                tx,
                &token(),
            )
            .await;

        assert!(
            matches!(result, Err(CommandError::Timeout { .. })),
            "{result:?}"
        );
        let survived = a_process_matching(MARKER);
        kill_processes_matching(MARKER);
        assert!(!survived, "the timed-out command's child is still running");
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

    // ---- The decoder that frames a pipe at character boundaries ---------------
    //
    // Driven directly, not through a command. The straddle the old read loop
    // produced was deterministic only because its buffer was a fixed 1024 bytes;
    // `FramedRead`'s is 8KB and a pipe delivers whatever the writer wrote, so an
    // end-to-end version of these can pass without the boundary ever being
    // crossed. Feeding hand-split slices is the only way to be sure it was.

    /// Push `pieces` through one decoder in order, as a pipe would deliver them,
    /// and collect the frames it chose to emit.
    fn decode_pieces(pieces: &[&[u8]]) -> Vec<Vec<u8>> {
        let mut decoder = Utf8Boundary;
        let mut buf = BytesMut::new();
        let mut frames = Vec::new();

        for piece in pieces {
            buf.extend_from_slice(piece);
            while let Some(frame) = decoder.decode(&mut buf).unwrap() {
                frames.push(frame.to_vec());
            }
        }
        while let Some(frame) = decoder.decode_eof(&mut buf).unwrap() {
            frames.push(frame.to_vec());
        }

        frames
    }

    #[test]
    fn a_character_split_across_two_reads_is_never_split_across_two_frames() {
        // 'é' is 0xC3 0xA9, arriving one byte per read.
        let frames = decode_pieces(&[b"abc\xc3", b"\xa9def"]);

        for frame in &frames {
            assert!(
                std::str::from_utf8(frame).is_ok(),
                "a frame ended inside a character: {frame:?}"
            );
        }
        assert_eq!(frames.concat(), "abcédef".as_bytes());
    }

    #[test]
    fn a_four_byte_character_arriving_one_byte_at_a_time_survives() {
        // U+1F600, 0xF0 0x9F 0x98 0x80: three separate incomplete states in a row.
        let frames = decode_pieces(&[b"\xf0", b"\x9f", b"\x98", b"\x80"]);

        for frame in &frames {
            assert!(
                std::str::from_utf8(frame).is_ok(),
                "a frame ended inside a character: {frame:?}"
            );
        }
        assert_eq!(frames.concat(), "😀".as_bytes());
    }

    #[test]
    fn a_character_that_was_never_split_is_handed_over_whole() {
        // Control. Without it the two tests above pass against a decoder that
        // buffers everything and emits it once at EOF, which would frame nothing
        // in real time and defeat the point of streaming.
        let frames = decode_pieces(&[b"abc\xc3\xa9def"]);

        assert_eq!(frames.len(), 1, "expected one frame, got {frames:?}");
        assert_eq!(frames[0], "abcédef".as_bytes());
    }

    #[test]
    fn genuinely_invalid_bytes_do_not_stall_the_stream() {
        // Distinct from an incomplete tail: nothing can complete 0xFF, so holding
        // it back would wedge the stream until EOF. It is handed over in place.
        let frames = decode_pieces(&[b"good\xffafter"]);

        assert_eq!(frames.concat(), b"good\xffafter");
        assert!(
            frames.len() > 1,
            "the invalid byte should have ended a frame, not been buffered: {frames:?}"
        );
    }

    #[test]
    fn a_partial_character_at_eof_is_handed_over_rather_than_lost() {
        // Nothing is coming to complete it. Dropping it would lose real output,
        // and returning `Ok(None)` makes `FramedRead` report "bytes remaining".
        let frames = decode_pieces(&[b"abc\xc3"]);

        assert_eq!(frames.concat(), b"abc\xc3");
    }

    #[test]
    fn decode_returns_ok_for_every_input() {
        // The property named on `Utf8Boundary::Error`: this decoder has no
        // failing case, so no error of its can carry process bytes into
        // `OutputReadFailed`'s `Display`. Asserted rather than commented,
        // because the type it must satisfy (`From<io::Error>`) cannot express it.
        let inputs: &[&[u8]] = &[
            b"",
            b"plain",
            b"\xc3\xa9",
            b"\xc3",         // incomplete two-byte
            b"\xf0\x9f\x98", // incomplete four-byte
            b"\xff",         // invalid
            b"\xc0\xaf",     // overlong
            b"\x80",         // lone continuation
            b"\xed\xa0\x80", // surrogate
            b"a\xffb\xc3\xa9c",
        ];

        for input in inputs {
            let mut buf = BytesMut::from(*input);
            assert!(
                Utf8Boundary.decode(&mut buf).is_ok(),
                "decode returned Err for {input:?}"
            );
            let mut buf = BytesMut::from(*input);
            assert!(
                Utf8Boundary.decode_eof(&mut buf).is_ok(),
                "decode_eof returned Err for {input:?}"
            );
        }
    }

    #[tokio::test]
    async fn a_frame_carries_the_stream_it_came_from_on_success_and_on_failure() {
        // Merging the two pipes erases which one an item came from, so the tag
        // has to ride on the item — including on the error, which is what
        // `OutputReadFailed` names the stream from. Without this the pump's own
        // read-failure test still passes while the tags are swapped, because it
        // builds its frames by hand rather than through `tagged_frames`.
        let frames: Vec<TaggedFrame> = tagged_frames(
            FailingReader::failing_after(b"partial"),
            OutputStream::Stderr,
        )
        .collect()
        .await;

        let (last, earlier) = frames.split_last().expect("no frames at all");
        assert!(
            matches!(last, Err((OutputStream::Stderr, _))),
            "the failure did not name the pipe it happened on: {last:?}"
        );
        // Control: the tag on the successful frames must be the same pipe, so
        // this cannot pass against an implementation that tags every error
        // `Stderr` regardless.
        assert!(!earlier.is_empty(), "expected bytes before the failure");
        for frame in earlier {
            assert!(
                matches!(frame, Ok((OutputStream::Stderr, _))),
                "a successful frame was tagged with the wrong pipe: {frame:?}"
            );
        }
    }

    #[tokio::test]
    async fn a_frame_from_stdout_is_tagged_stdout() {
        // The other half of the control above: both tests would pass against a
        // `tagged_frames` that hardcoded whichever single stream one of them used.
        let frames: Vec<TaggedFrame> = tagged_frames(
            FailingReader::failing_after(b"partial"),
            OutputStream::Stdout,
        )
        .collect()
        .await;

        assert!(
            frames.iter().all(|frame| matches!(
                frame,
                Ok((OutputStream::Stdout, _)) | Err((OutputStream::Stdout, _))
            )),
            "{frames:?}"
        );
    }

    // ---- What the pump does with the frames ----------------------------------

    /// A child that will not exit on its own, for the read-failure test below.
    fn sleeping_child() -> tokio::process::Child {
        Command::new(ShellCommandRunner::default_shell())
            .arg("-c")
            .arg("exec sleep 30")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("failed to spawn")
    }

    #[tokio::test]
    async fn a_read_failure_is_reported_at_once_rather_than_waiting_for_the_child() {
        // The pump is the only thing draining the pipes, so joining it with
        // `child.wait()` — rather than `try_join`ing — makes a read failure wait
        // for a child that may never exit, and the caller then reports the whole
        // thing as a `Timeout`. This child never exits, so a shape that waits
        // for it cannot finish inside the assertion below.
        let mut child = sleeping_child();
        let (tx, _rx) = tokio::sync::mpsc::channel(10);
        let frames = futures::stream::iter(vec![Err((
            OutputStream::Stderr,
            std::io::Error::other("pipe died"),
        ))]);

        let result = tokio::time::timeout(
            Duration::from_secs(3),
            drain_and_wait(
                &mut child,
                frames,
                &mut Vec::new(),
                &mut Vec::new(),
                &tx,
                "cmd",
                Path::new("/pkg"),
            ),
        )
        .await
        .expect("the read failure waited for a child that never exits");
        let _ = child.kill().await;

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

    #[tokio::test]
    async fn the_pump_accumulates_raw_bytes_and_relays_decoded_text() {
        // Two things at once because they must not diverge: what a caller reads
        // out of `CommandOutput` and what a watcher sees on the channel come from
        // the same frames.
        let mut child = Command::new(ShellCommandRunner::default_shell())
            .arg("-c")
            .arg("true")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::channel(10);
        let frames = futures::stream::iter(vec![
            Ok((OutputStream::Stdout, Bytes::from_static(b"out"))),
            Ok((OutputStream::Stderr, Bytes::from_static(b"err"))),
        ]);
        let (mut out, mut err) = (Vec::new(), Vec::new());

        drain_and_wait(
            &mut child,
            frames,
            &mut out,
            &mut err,
            &tx,
            "cmd",
            Path::new("/pkg"),
        )
        .await
        .unwrap();
        drop(tx);

        assert_eq!(out, b"out");
        assert_eq!(err, b"err");

        let mut relayed = Vec::new();
        while let Some(chunk) = rx.recv().await {
            relayed.push(chunk);
        }
        assert_eq!(
            relayed,
            vec![
                OutputChunk::Stdout("out".to_string()),
                OutputChunk::Stderr("err".to_string()),
            ]
        );
    }

    // ---- Streaming, end to end -----------------------------------------------

    #[tokio::test]
    async fn a_streaming_command_writing_past_the_pipe_buffer_on_both_streams_completes() {
        // The twin of the buffered deadlock test. `execute_streaming` had no such
        // test at all, and it is the half of the runner where a stall is most
        // likely to be mistaken for a slow command.
        let runner =
            ShellCommandRunner::new(ShellCommandRunner::default_shell(), Duration::from_secs(30));
        let (tx, mut rx) = tokio::sync::mpsc::channel(1000);
        let relayed = tokio::spawn(async move {
            let mut n = 0;
            while rx.recv().await.is_some() {
                n += 1;
            }
            n
        });

        let output = tokio::time::timeout(
            Duration::from_secs(20),
            runner.execute_streaming(
                BOTH_PIPES_PAST_THE_BUFFER,
                Duration::from_secs(20),
                tx,
                &token(),
            ),
        )
        .await
        .expect("reading both pipes deadlocked")
        .expect("command failed");

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
        // Positive control: without this the test passes against a runner that
        // captures everything and relays nothing, which is not streaming.
        assert!(
            relayed.await.unwrap() > 0,
            "the command's output never reached the channel"
        );
    }

    #[tokio::test]
    async fn a_streamed_character_spanning_a_read_is_not_relayed_as_replacement_characters() {
        // The user-visible half of the decoder tests above, through the real
        // adapter: one 'é' used to arrive as two U+FFFD, in the terminal and in
        // the MCP server's JSON alike.
        let runner =
            ShellCommandRunner::new(ShellCommandRunner::default_shell(), Duration::from_secs(30));
        let (tx, mut rx) = tokio::sync::mpsc::channel(10_000);
        let relayed = tokio::spawn(async move {
            let mut text = String::new();
            while let Some(chunk) = rx.recv().await {
                if let OutputChunk::Stdout(s) = chunk {
                    text.push_str(&s);
                }
            }
            text
        });

        let output = runner
            .execute_streaming(SPLIT_CHARACTER, Duration::from_secs(20), tx, &token())
            .await
            .expect("command failed");
        let text = relayed.await.unwrap();

        // Exactly what a lossy decode of the whole output gives: the `é` intact,
        // and one U+FFFD for the one byte that really is invalid. Asserting the
        // whole string rather than "no U+FFFD" — the fixture deliberately
        // contains a byte that must become one, so the property is that the
        // replacement count matches the genuinely invalid input and no boundary
        // added any.
        assert_eq!(text, String::from_utf8_lossy(&split_character_bytes()));
        assert_eq!(
            text.matches('\u{FFFD}').count(),
            1,
            "a character was split across frames: {text:?}"
        );
        assert!(text.contains('é'), "the character never arrived at all");
        // The captured output is raw bytes, and stays byte-exact past the split.
        assert_eq!(output.stdout(), split_character_bytes());
    }

    #[tokio::test]
    async fn a_streaming_command_that_outruns_its_timeout_is_reported_as_a_timeout() {
        let runner =
            ShellCommandRunner::new(ShellCommandRunner::default_shell(), Duration::from_secs(30));
        let (tx, _rx) = tokio::sync::mpsc::channel(10);

        let result = runner
            .execute_streaming("sleep 30", Duration::from_millis(200), tx, &token())
            .await;

        assert!(
            matches!(result, Err(CommandError::Timeout { .. })),
            "{result:?}"
        );
    }

    #[tokio::test]
    async fn a_streaming_command_hangs_on_a_grandchild_only_until_its_timeout() {
        // selfie-b7mv, the streaming half. A grandchild inheriting the pipe keeps
        // it open after the shell exits, so the reads never reach EOF. The old
        // shape's deadline arm was guarded `if !process_done`, so once the shell
        // exited nothing bounded the reads: the command ran ~8s past a 2s budget
        // and was then reported as `Ok`.
        let runner =
            ShellCommandRunner::new(ShellCommandRunner::default_shell(), Duration::from_secs(30));
        let (tx, _rx) = tokio::sync::mpsc::channel(10);
        let started = Instant::now();

        let result = runner
            .execute_streaming(
                "sleep 8 & echo started",
                Duration::from_secs(2),
                tx,
                &token(),
            )
            .await;

        assert!(
            matches!(result, Err(CommandError::Timeout { .. })),
            "a command past its timeout was not reported as one: {result:?}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(6),
            "the timeout was not enforced; took {:?}",
            started.elapsed()
        );
    }

    // NOTE: the `biased;` on the outer `select!` has no test, deliberately.
    // Asserting it needs both arms ready within one poll, and nothing the
    // adapter exposes lets a caller arrange that: cancelling early is a genuine
    // cancellation, and cancelling late cannot reach the select at all. A loop
    // racing the two would flake in the *false failure* direction, since a token
    // cancelled while the command is still running makes `Cancelled` the correct
    // answer. Left untested rather than covered by a test that cannot fail.
}
