//! Shell command runner adapter implementation
//!
//! This module provides a concrete implementation of the `CommandRunner` trait
//! that executes commands through a system shell. It supports both blocking
//! and streaming execution modes with configurable timeouts.

use std::{
    future::Future,
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

use super::runner::{
    CommandError, CommandOutput, CommandRunner, ContentOutput, OutputChunk, OutputStream,
};

/// Unix-only: the separation is built out of descriptor redirection and a
/// `printf` builtin, neither of which `cmd.exe` has.
#[cfg(unix)]
mod content;

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
    /// Create a runner that invokes `shell` — `/bin/sh`, `/bin/bash` — without
    /// sourcing a login profile.
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

    /// Build the `Command` that runs `recipe` with the capture out of its reach.
    ///
    /// The outer shell is `/bin/sh` rather than the configured one because it must
    /// source nothing before its redirection takes effect. The four variables
    /// removed here are the ways it could be made to: `ENV` and `BASH_ENV` name a
    /// file it would source, and `SHELLOPTS`/`BASH_XTRACEFD` put its own trace
    /// output on a descriptor of the caller's choosing.
    #[cfg(unix)]
    fn build_content_command(&self, recipe: &str, working_dir: &Path, fd: u8) -> Command {
        let mut cmd = Command::new("/bin/sh");
        cmd.arg("-c")
            .arg(content::wrapper(fd, self.login))
            .env(content::SHELL_VAR, &self.shell)
            .env(content::COMMAND_VAR, recipe)
            .env_remove("ENV")
            .env_remove("BASH_ENV")
            .env_remove("SHELLOPTS")
            .env_remove("BASH_XTRACEFD")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .current_dir(working_dir);
        cmd
    }

    /// Run `cmd` to completion, buffering stdout and stderr.
    ///
    /// Takes the `Command` already built and, separately, the command string to
    /// **report**. Every error below names `reported`, and this function has no
    /// access to the text the shell was actually given — which is what keeps
    /// selfie's own scaffolding out of a failure a user reads. `collect` gets
    /// `reported` for the same reason: it builds two of the five error variants
    /// itself.
    async fn run_buffered(
        &self,
        mut cmd: Command,
        reported: &str,
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
                command: reported.to_string(),
                working_directory,
            });
        }

        let mut child = cmd.spawn().map_err(|e| CommandError::IoError {
            command: reported.to_string(),
            working_directory: working_directory.clone(),
            source: Arc::new(e),
        })?;

        // Both pipes are read *concurrently with* `wait()`, not after it. That is
        // what avoids the deadlock when a child produces more than the OS pipe
        // buffer (~64KB): a read that only started once `wait()` returned would
        // never get there, because the child cannot exit until something drains
        // the pipe it is blocked writing to.
        let stdout = read_stream(child.stdout.take());
        let stderr = read_stream(child.stderr.take());

        let outcome = tokio::select! {
            // Biased for the same reason as `execute_streaming`: a command that
            // finished inside its budget should be reported as finished, and an
            // unbiased `select!` picks at random when both arms are ready at once.
            biased;
            result = tokio::time::timeout(
                timeout,
                collect(child.wait(), stdout, stderr, reported, &working_directory),
            ) => result.unwrap_or_else(|_elapsed| Err(CommandError::Timeout {
                command: reported.to_string(),
                timeout,
                working_directory: working_directory.clone(),
            })),
            () = token.cancelled() => Err(CommandError::Cancelled {
                command: reported.to_string(),
                working_directory: working_directory.clone(),
            }),
        };

        // The child is only borrowed above, so it is still ours to kill here.
        // Every failure abandons one that may still be running — including a
        // failed read, where nothing is left draining the pipe it died on, which
        // the previous shape left running. Killing an already-exited child is a
        // no-op rather than an error (`Child::start_kill` returns `Ok` for a
        // reaped child), so this needs no guard on which failure it was.
        //
        // `a_timed_out_command_does_not_leave_its_child_running` covers the
        // timeout arm. The read-failure arm is the one no test reaches: it needs
        // a genuine pipe failure from a real child.
        if outcome.is_err() {
            let _ = child.kill().await;
        }

        let (status, stdout, stderr) = outcome?;

        Ok(CommandOutput {
            output: Output {
                status,
                stdout,
                stderr,
            },
            duration: start_time.elapsed(),
        })
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
    // `which` does a native PATH lookup without spawning a subprocess, which is
    // also why it finds filesystem executables and not shell builtins.
    async fn is_command_available(&self, command: &str) -> bool {
        which::which(command).is_ok()
    }

    // The trait leaves the default timeout to the implementation; this one uses
    // whatever the runner was built with.
    async fn execute(
        &self,
        command: &str,
        token: &CancellationToken,
    ) -> Result<CommandOutput, CommandError> {
        self.execute_with_timeout(command, self.default_timeout, token)
            .await
    }

    async fn execute_with_timeout(
        &self,
        command: &str,
        timeout: Duration,
        token: &CancellationToken,
    ) -> Result<CommandOutput, CommandError> {
        self.run_buffered(
            self.build_command(command, None),
            command,
            None,
            timeout,
            token,
        )
        .await
    }

    async fn execute_in_dir(
        &self,
        command: &str,
        working_dir: &Path,
        timeout: Duration,
        token: &CancellationToken,
    ) -> Result<CommandOutput, CommandError> {
        self.run_buffered(
            self.build_command(command, Some(working_dir)),
            command,
            Some(working_dir),
            timeout,
            token,
        )
        .await
    }

    // Drops a chunk rather than blocking the read loop when the receiver falls
    // behind, which is the best-effort delivery the trait allows for.
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

    /// Run a command whose stdout becomes a file's content.
    ///
    /// Deliberately **not** conditioned on `self.login`: a non-login shell is
    /// quieter, not silent, and gating on the flag would leave every test built
    /// with [`ShellCommandRunner::new`] passing against a splicing production
    /// path.
    #[cfg(unix)]
    async fn execute_for_content(
        &self,
        command: &str,
        working_dir: &Path,
        timeout: Duration,
        token: &CancellationToken,
    ) -> Result<ContentOutput, CommandError> {
        let markers = content::Markers::new();
        let fd = content::CAPTURE_FD;
        let recipe = content::recipe(&self.shell, command, &markers, fd);

        let output = self
            .run_buffered(
                self.build_content_command(&recipe, working_dir, fd),
                command,
                Some(working_dir),
                timeout,
                token,
            )
            .await?;

        let success = output.is_success();
        let stderr = output.stderr().to_vec();
        match content::extract(output.into_stdout(), &markers) {
            Some(extracted) => Ok(ContentOutput::from_capture(
                success,
                extracted.content,
                stderr,
                extracted.discarded_before,
                extracted.tail_verified,
            )),
            // A failed run's stderr holds the diagnosis — an unusable `$SHELL`, a
            // profile that exited — and reporting absent markers instead would
            // drop it. What keeps this from deploying anything is the guard: a
            // `ContentOutput` that says it failed is refused by `run_capture`
            // before its bytes are read at all. The empty buffer is belt and
            // braces, not the guarantee.
            None if !success => Ok(ContentOutput::from_capture(
                false,
                Vec::new(),
                stderr,
                0,
                false,
            )),
            None => Err(CommandError::ContentMarkersAbsent {
                command: command.to_string(),
                working_directory: working_dir.to_path_buf(),
            }),
        }
    }

    /// Run a command whose stdout becomes a file's content.
    ///
    /// Windows has no login profile to source and no `printf` in `cmd.exe`, so
    /// the command runs as it always did and the tail is reported unverified —
    /// which is what it is. Nothing separates a `cmd.exe` `AutoRun` command's
    /// output from the command's own here.
    #[cfg(not(unix))]
    async fn execute_for_content(
        &self,
        command: &str,
        working_dir: &Path,
        timeout: Duration,
        token: &CancellationToken,
    ) -> Result<ContentOutput, CommandError> {
        let output = self
            .run_buffered(
                self.build_command(command, Some(working_dir)),
                command,
                Some(working_dir),
                timeout,
                token,
            )
            .await?;

        let success = output.is_success();
        let stderr = output.stderr().to_vec();
        Ok(ContentOutput::from_capture(
            success,
            output.into_stdout(),
            stderr,
            0,
            false,
        ))
    }
}

/// Read one of a child's pipes to the end, or fail.
///
/// **A partial buffer is dropped, never returned**: what accumulated before a
/// read failed is not the command's output. `None` is not a failure — a pipe that
/// was never captured has no output.
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

/// Run a child to completion while both its pipes drain, or report the first
/// thing that went wrong.
///
/// # Errors
///
/// [`CommandError::OutputReadFailed`] naming the pipe that failed, or
/// [`CommandError::IoError`] if waiting on the child itself failed.
// Generic over the three futures rather than taking a child, so a read failure —
// which no real child can be made to produce on demand — is testable.
//
// `try_join3` polls all three concurrently, which is what avoids the ~64KB
// pipe-buffer deadlock, and returns the first error. Returning early matters
// because the pipe reads are the only thing draining the child: waiting on
// `wait()` after a read has failed blocks until the caller's timeout, and
// reports a read failure as a timeout.
//
// A failed read is an error rather than an empty buffer beside a successful
// status, because a command can exit 0 while the pipe carrying its answer fails,
// and callers act on that output — one uses it as an executable path, one writes
// it to a credentials file.
//
// Do not read the pipes in a `tokio::spawn`. That makes a `JoinError` reachable,
// and its `Display` renders the panic payload of the task holding this command's
// bytes into `CommandError::OutputReadFailed`, and on to
// `PackageEvent::Completed`, the CLI and the MCP server's JSON. Reading inline
// leaves nothing to leak.
async fn collect<S, O, E>(
    wait: S,
    stdout: O,
    stderr: E,
    command: &str,
    working_directory: &Path,
) -> Result<(std::process::ExitStatus, Vec<u8>, Vec<u8>), CommandError>
where
    S: Future<Output = std::io::Result<std::process::ExitStatus>>,
    O: Future<Output = std::io::Result<Vec<u8>>>,
    E: Future<Output = std::io::Result<Vec<u8>>>,
{
    let read_failed = |stream: OutputStream| {
        move |source: std::io::Error| CommandError::OutputReadFailed {
            command: command.to_string(),
            working_directory: working_directory.to_path_buf(),
            stream,
            source: Arc::new(source),
        }
    };

    futures::future::try_join3(
        async {
            wait.await.map_err(|e| CommandError::IoError {
                command: command.to_string(),
                working_directory: working_directory.to_path_buf(),
                source: Arc::new(e),
            })
        },
        async { stdout.await.map_err(read_failed(OutputStream::Stdout)) },
        async { stderr.await.map_err(read_failed(OutputStream::Stderr)) },
    )
    .await
}

/// Cuts a pipe into frames whose decoding does not depend on where the cuts fell.
///
/// The property relied on is that framing never *adds* a replacement character:
/// `lossy(a) + lossy(b) == lossy(a + b)` for any two consecutive frames. That is
/// what makes it safe to decode each frame on its own and relay it.
///
/// Yields raw bytes rather than a `String`, so the same frames can be
/// accumulated byte-exactly into [`CommandOutput`] while only the relayed copy is
/// decoded.
// Not the stronger "a frame never ends inside a character". A frame can end
// inside an invalid sequence, at the boundary of its maximal subpart —
// `[0x61, 0xF4]` followed by `0xF5` yields a frame of just `[0xF4]`. That costs
// nothing, because `0xF4` decodes to one `U+FFFD` framed alone or with its
// neighbours. What never happens is a valid character being split, which is the
// case that would turn one character into two `U+FFFD`.
//
// The boundary rule is `std`'s, not this module's: `str::from_utf8` reports both
// how far the input was valid (`Utf8Error::valid_up_to`) and whether the
// trailing bytes were merely incomplete or genuinely invalid
// (`Utf8Error::error_len`). This is the adapter from that onto `Decoder`; it
// does not parse UTF-8 itself.
//
// Framing at a fixed byte count instead — which the read loop this replaced did,
// at 1024 bytes — splits any multi-byte character straddling the cut into two
// `U+FFFD`s, one ending a chunk and one starting the next. Both halves reach the
// terminal and the MCP server's JSON, because the relayed chunk is decoded here
// and nothing downstream can undo it. Yielding text would likewise make a
// command's captured stdout lossy under streaming and byte-exact everywhere
// else.
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

    // `Decoder` requires `From<std::io::Error>`, so this cannot be `Infallible`.
    // Nothing here constructs one, and that matters: such an error would become
    // `CommandError::OutputReadFailed`'s `source`, whose `Display` reaches
    // `PackageEvent::Completed`, the CLI and the MCP server's JSON — and this is
    // the one place on the streaming path where a constructible error and the
    // process's raw bytes are in scope together. If a fallible case is ever
    // added, its message must be a fixed `&'static str` and must never embed
    // `src`. `decode_returns_ok_for_every_input` pins the stronger property the
    // code has today: no input makes this return `Err` at all.
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
/// # Errors
///
/// [`CommandError::OutputReadFailed`] naming the pipe that failed, or
/// [`CommandError::IoError`] if waiting on the child itself failed.
// `try_join!`, not `join!`. The pump is the only thing draining the pipes, and a
// child blocked writing to a full one cannot exit until something does — so a
// `join!`, which waits for both, turns a read failure into a wait for the entire
// remaining timeout and then reports it as `CommandError::Timeout`. `try_join!`
// returns the read failure as soon as it happens, and the caller kills the child
// it left behind.
//
// The success path still requires both: the status and both pipes at EOF. That
// is what avoids the ~64KB pipe-buffer deadlock, and it is why a grandchild
// holding the pipe open runs out the timeout rather than hanging past it. No
// phase here escapes the deadline.
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

    // An `AsyncRead` that yields `bytes` and then fails.
    //
    // Stands in for a pipe that dies part-way through, which cannot be provoked
    // reliably from a real child process.
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

        // The control: the same bytes, then a clean EOF.
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

    // A pipe that read to the end, for driving [`collect`].
    async fn read_ok(bytes: &'static [u8]) -> std::io::Result<Vec<u8>> {
        Ok(bytes.to_vec())
    }

    // A pipe that died part-way through.
    async fn read_failed() -> std::io::Result<Vec<u8>> {
        Err(std::io::Error::other("pipe died"))
    }

    // A child whose `wait()` itself failed, which must stay distinguishable
    // from a pipe that failed.
    async fn wait_failed() -> std::io::Result<std::process::ExitStatus> {
        Err(std::io::Error::other("wait died"))
    }

    async fn wait_ok() -> std::io::Result<std::process::ExitStatus> {
        Ok(exit_status(0))
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

    // ---- Deciding whether a command's output is trustworthy ------------------
    //
    // These drive `collect`, which `run_buffered` calls. The shape they replaced
    // could only be tested beside its call site, and its own doc comment said so:
    // reverting the decision at the call site would have failed none of them.

    #[tokio::test]
    async fn a_failing_stdout_read_is_reported_as_output_read_failed_naming_the_stream() {
        let error = collect(
            wait_ok(),
            read_failed(),
            read_ok(b"err"),
            "which ripgrep",
            Path::new("/pkg"),
        )
        .await
        .unwrap_err();

        match &error {
            CommandError::OutputReadFailed {
                command,
                working_directory,
                stream,
                ..
            } => {
                assert_eq!(*stream, OutputStream::Stdout);
                assert_eq!(command, "which ripgrep");
                assert_eq!(working_directory, Path::new("/pkg"));
            }
            other => panic!("expected OutputReadFailed, got: {other:?}"),
        }
        assert!(error.to_string().contains("stdout"), "{error}");
        assert!(error.to_string().contains("which ripgrep"), "{error}");
    }

    #[tokio::test]
    async fn a_failing_stderr_read_is_an_error_even_though_stdout_read_fine() {
        // Reported for stderr too, on a command whose correctness depends only on
        // stdout. The runner reports what it could not read; it does not decide
        // which stream a caller cared about.
        let error = collect(
            wait_ok(),
            read_ok(b"out"),
            read_failed(),
            "cmd",
            Path::new("/pkg"),
        )
        .await
        .unwrap_err();

        assert!(
            matches!(
                error,
                CommandError::OutputReadFailed {
                    stream: OutputStream::Stderr,
                    ..
                }
            ),
            "{error:?}"
        );
        assert!(error.to_string().contains("stderr"), "{error}");
    }

    #[tokio::test]
    async fn a_failed_wait_is_an_io_error_not_a_read_failure() {
        // The two must stay distinguishable: one says the command could not be
        // run to completion, the other that selfie does not have its output.
        let error = collect(
            wait_failed(),
            read_ok(b"out"),
            read_ok(b"err"),
            "cmd",
            Path::new("/pkg"),
        )
        .await
        .unwrap_err();

        assert!(matches!(error, CommandError::IoError { .. }), "{error:?}");
    }

    #[tokio::test]
    async fn output_read_failed_does_not_render_the_bytes_that_were_read() {
        // `OutputReadFailed`'s `Display` reaches `PackageEvent::Completed`, the
        // CLI and the MCP server's JSON, and the stream it names is one a
        // credential can travel on. Whatever had been buffered is dropped rather
        // than reported, so there is nothing for the message to carry.
        const SECRET: &str = "hunter2-Zk9xQw-vault-token";

        let error = collect(
            wait_ok(),
            async { Err(std::io::Error::other("pipe died")) },
            read_ok(SECRET.as_bytes()),
            "op read x",
            Path::new("/pkg"),
        )
        .await
        .unwrap_err();
        let rendered = error.to_string();

        assert!(!rendered.contains(SECRET), "{rendered}");
        // Positive control: without this the assertion above passes against an
        // error that renders nothing at all.
        assert!(rendered.contains("pipe died"), "{rendered}");
    }

    #[tokio::test]
    async fn collect_returns_both_buffers_and_the_status_when_nothing_failed() {
        // Control for the four above: they would all pass against a `collect`
        // that failed unconditionally.
        let (status, stdout, stderr) = collect(
            wait_ok(),
            read_ok(b"out"),
            read_ok(b"err"),
            "cmd",
            Path::new("/pkg"),
        )
        .await
        .unwrap();

        assert_eq!(stdout, b"out");
        assert_eq!(stderr, b"err");
        assert!(status.success());
    }

    // ---- T5: the deadlock this shape exists to avoid -------------------------

    // Writes well past the ~64KB pipe buffer on **both** streams.
    //
    // Shared by the buffered and streaming deadlock tests so the two cannot
    // drift into measuring different amounts of output.
    const BOTH_PIPES_PAST_THE_BUFFER: &str = "for i in $(seq 1 4000); do \
         echo 'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa'; \
         echo 'bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb' >&2; \
         done";

    // 1023 ASCII bytes, then a two-byte `é`, then an invalid `0xFF`, then more
    // ASCII.
    //
    // The `é` straddles the 1024-byte boundary the old read loop cut on. The
    // `0xFF` is what makes a byte-exactness assertion able to fail: a *valid*
    // character that gets split still round-trips through
    // `String::from_utf8_lossy(..).as_bytes()` byte for byte, so a fixture
    // without an invalid byte cannot detect output that was decoded before it
    // was captured.
    // **Octal, not `\\xNN`.** Hex escapes are a bash/BSD extension; POSIX
    // specifies `\\ooo`, and `/bin/sh` is dash on Debian and Ubuntu, whose
    // `printf` emits `\\xc3` as five literal characters. That made this fixture
    // produce no split character at all and the test fail for a reason having
    // nothing to do with the decoder.
    const SPLIT_CHARACTER: &str = "printf 'a%.0s' $(seq 1 1023); printf '\\303\\251'; \
         printf '\\377'; printf 'b%.0s' $(seq 1 100)";

    // Exactly what [`SPLIT_CHARACTER`] writes to stdout.
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

    #[tokio::test]
    async fn a_buffered_command_hangs_on_a_grandchild_only_until_its_timeout() {
        // selfie-b7mv, the buffered half — the same shape as the streaming test
        // above, and the reason this bead is not closed by either commit alone.
        //
        // The timeout and cancellation arms must not sit inside a `select!` that
        // `child.wait()` can win. A grandchild inheriting the stdout pipe keeps it
        // open after the shell exits, so `wait()` returns in milliseconds and the
        // reads that follow would have no deadline at all: the command runs to
        // completion against an unenforced budget, and the caller is
        // told the command had succeeded.
        let runner =
            ShellCommandRunner::new(ShellCommandRunner::default_shell(), Duration::from_secs(30));
        let started = Instant::now();

        let result = runner
            .execute_with_timeout("sleep 8 & echo started", Duration::from_secs(2), &token())
            .await;

        assert!(
            matches!(result, Err(CommandError::Timeout { .. })),
            "a command past its timeout was reported as: {result:?}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(6),
            "the timeout was not enforced; took {:?}",
            started.elapsed()
        );
    }

    // Is a process whose command line contains `marker` still running?
    //
    // **The marker has to be the command the shell execs**, not a comment
    // beside it. `sh -c 'sleep 30 # marker'` replaces the shell with `sleep 30`
    // and the comment is gone, so `pgrep -f marker` finds nothing whether or not
    // the child was killed — a test written that way passes either way. An
    // unusual sleep duration is the marker instead: it survives the exec because
    // it *is* the exec'd command.
    //
    // **The callers run it through `exec`, and that is load-bearing.** What the
    // runner promises is that it kills its *direct child*; what this function
    // observes is that no process is running the marker. Those are the same
    // claim only when the shell has replaced itself with the marker rather than
    // forking it. Replacing itself is what `exec` is specified to do, so this
    // holds on every POSIX shell — whereas relying on a shell to *choose* to
    // exec a lone command makes the test depend on an optimization. `bash` and
    // `dash` differ there, which is how a version of this without `exec` passed
    // on macOS and failed on Ubuntu, reporting a grandchild the runner never
    // promised to kill (that leak is real, tracked separately, and deliberately
    // out of scope here).
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

    // Kill anything left over, so a failing assertion does not also leak a
    // process into the developer's session.
    fn kill_processes_matching(marker: &str) {
        let _ = std::process::Command::new("pkill")
            .args(["-x", "-f", &marker.replace('.', "\\.")])
            .output();
    }

    #[tokio::test]
    async fn a_timed_out_command_does_not_leave_its_child_running() {
        // Abandoning the child would leave a process holding pipes nobody reads
        // for as long as it wants. Without this, deleting the kill above fails
        // no test — the error is returned either way, and only the process table
        // shows the difference.
        // An unusual duration, so it is both findable and self-limiting if the
        // assertion below fails.
        const MARKER: &str = "sleep 33.71";
        let runner =
            ShellCommandRunner::new(ShellCommandRunner::default_shell(), Duration::from_secs(30));

        let result = runner
            .execute_with_timeout(
                &format!("exec {MARKER}"),
                Duration::from_millis(300),
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

    // Push `pieces` through one decoder in order, as a pipe would deliver them,
    // and collect the frames it chose to emit.
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

    // A child that will not exit on its own, for the read-failure test below.
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
        // adapter: a character split across two reads must not arrive as two
        // U+FFFD, in the terminal or in the MCP server's JSON.
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

// What a noisy login shell does to a command whose output becomes a file.
//
// The fixture is a script standing in for the user's shell, never the
// developer's own: a noisy profile is, at the descriptor level, a shell that
// writes before, during and after the `-c` string it was given. It `eval`s the
// recipe in a real `/bin/sh`, so behaviour that depends on the shell — selfie's
// `EXIT` trap displacing the profile's — is real rather than simulated.
//
// fish is not tested here. CI does not have it, and a test that skips when its
// subject is missing reports success for having done nothing.
#[cfg(all(test, unix))]
mod content_tests {
    use super::*;
    use std::path::PathBuf;

    const TIMEOUT: Duration = Duration::from_secs(30);
    const SECRET: &str = "s3cr3t-Value-9x7";

    fn token() -> CancellationToken {
        CancellationToken::new()
    }

    // Write a stand-in shell into `dir` and return its path.
    //
    // `before` is written when the shell starts, as a profile banner is. `after`
    // is written from an `EXIT` trap, as a profile's own trap would be.
    // `background` is written by a detached child a fraction of a second later,
    // as an update checker started by a profile does — the case no boundary
    // drawn in the output stream can catch, because it depends on timing rather
    // than on position.
    fn noisy_shell(dir: &Path, before: &str, after: &str, background: bool) -> PathBuf {
        let mut body = String::from("#!/bin/sh\n");
        if !before.is_empty() {
            body.push_str(&format!("printf '%s' '{before}'\n"));
        }
        if background {
            body.push_str("/bin/sh -c 'sleep 0.3; printf BACKGROUNDNOISE' &\n");
        }
        if !after.is_empty() {
            body.push_str(&format!("trap 'printf {after}' EXIT\n"));
        }
        // The recipe is the last argument, whether or not `-l` precedes `-c`.
        body.push_str("shift $(($# - 1)); eval \"$1\"\n");

        let path = dir.join("noisy-shell");
        test_common::write_executable(&path, &body);
        path
    }

    // A runner whose shell is noisy in every way at once.
    fn noisy_runner(dir: &Path) -> ShellCommandRunner {
        let shell = noisy_shell(dir, "LEADBANNER", "TRAILCHATTER", true);
        ShellCommandRunner::new(shell.to_str().unwrap(), TIMEOUT)
    }

    // A command that takes long enough for the backgrounded writer to land in
    // the middle of it. Without the wait the race is won by accident and the
    // test passes for the wrong reason.
    fn slow_secret() -> String {
        format!("sleep 0.6; printf '%s' '{SECRET}'")
    }

    #[tokio::test]
    async fn the_unfenced_path_splices_every_kind_of_shell_noise() {
        // The control for every test below. If this stops holding, the fixture
        // has stopped being noisy and the other tests pass by having nothing to
        // separate.
        let dir = tempfile::tempdir().unwrap();
        let runner = noisy_runner(dir.path());

        let output = runner
            .execute_in_dir(&slow_secret(), dir.path(), TIMEOUT, &token())
            .await
            .unwrap();
        let captured = String::from_utf8_lossy(output.stdout()).to_string();

        assert!(captured.contains("LEADBANNER"), "{captured}");
        assert!(captured.contains("BACKGROUNDNOISE"), "{captured}");
        assert!(captured.contains("TRAILCHATTER"), "{captured}");
        assert!(captured.contains(SECRET), "{captured}");
    }

    #[tokio::test]
    async fn content_capture_returns_only_what_the_command_wrote() {
        let dir = tempfile::tempdir().unwrap();
        let runner = noisy_runner(dir.path());

        let output = runner
            .execute_for_content(&slow_secret(), dir.path(), TIMEOUT, &token())
            .await
            .unwrap();

        assert!(output.is_success());
        assert!(output.tail_verified());
        assert_eq!(output.discarded_before(), 0);
        assert_eq!(String::from_utf8_lossy(&output.into_stdout()), SECRET);
    }

    #[tokio::test]
    async fn a_login_runner_separates_the_output_too() {
        // The login runner is the one users get: `create_command_runner` in the
        // CLI and the MCP server both build one, and it takes a different wrapper
        // because it passes `-l`. Every other test here uses a non-login runner,
        // so without this the production wrapper has no coverage at all.
        //
        // Built as a literal rather than through `login_shell`, which reads the
        // developer's own `SHELL`.
        let dir = tempfile::tempdir().unwrap();
        let shell = noisy_shell(dir.path(), "LEADBANNER", "TRAILCHATTER", true);
        let runner = ShellCommandRunner {
            shell: shell.to_str().unwrap().to_string(),
            default_timeout: TIMEOUT,
            login: true,
        };

        let output = runner
            .execute_for_content(&slow_secret(), dir.path(), TIMEOUT, &token())
            .await
            .unwrap();

        assert!(output.tail_verified());
        assert_eq!(String::from_utf8_lossy(&output.into_stdout()), SECRET);
    }

    #[tokio::test]
    async fn content_capture_is_not_gated_on_the_login_flag() {
        // `ShellCommandRunner::new` is not a login runner, and every test here
        // uses one. Were the separation conditioned on that flag, all of them
        // would pass against a production path that still splices — so this
        // asserts the flag directly rather than relying on the others.
        let dir = tempfile::tempdir().unwrap();
        let runner = noisy_runner(dir.path());
        assert!(!runner.login, "the fixture must be a non-login runner");

        let output = runner
            .execute_for_content("printf '%s' 'plain'", dir.path(), TIMEOUT, &token())
            .await
            .unwrap();

        assert_eq!(String::from_utf8_lossy(&output.into_stdout()), "plain");
    }

    #[tokio::test]
    async fn execute_in_dir_is_left_unfenced() {
        // Install, check and audit parse a command's output and show it. They
        // must not start finding selfie's markers in it because a later refactor
        // routed them through the content path.
        let dir = tempfile::tempdir().unwrap();
        let runner = noisy_runner(dir.path());

        let output = runner
            .execute_in_dir("printf '%s' 'plain'", dir.path(), TIMEOUT, &token())
            .await
            .unwrap();
        let captured = String::from_utf8_lossy(output.stdout()).to_string();

        assert!(captured.starts_with("LEADBANNER"), "{captured}");
        assert!(
            !captured.contains("sfo"),
            "markers reached a parsed command"
        );
        assert!(
            !captured.contains("sfc"),
            "markers reached a parsed command"
        );
    }

    #[tokio::test]
    async fn content_capture_keeps_the_status_the_stderr_and_the_directory() {
        let dir = tempfile::tempdir().unwrap();
        let runner = noisy_runner(dir.path());

        let output = runner
            .execute_for_content(
                "pwd; printf 'boom' >&2; exit 7",
                dir.path(),
                TIMEOUT,
                &token(),
            )
            .await
            .unwrap();

        assert!(!output.is_success());
        assert_eq!(String::from_utf8_lossy(output.stderr()), "boom");
        let printed = String::from_utf8_lossy(&output.into_stdout())
            .trim()
            .to_string();
        assert!(
            printed.ends_with(dir.path().file_name().unwrap().to_str().unwrap()),
            "the command ran in {printed}, not the directory it was given"
        );
    }

    #[tokio::test]
    async fn content_capture_is_byte_exact() {
        let dir = tempfile::tempdir().unwrap();
        let runner = noisy_runner(dir.path());

        // Octal escapes: `\xNN` is a bash and BSD extension, and CI's `/bin/sh`
        // is dash.
        let output = runner
            .execute_for_content(r"printf '\000\377\012'", dir.path(), TIMEOUT, &token())
            .await
            .unwrap();

        assert_eq!(output.into_stdout(), vec![0x00, 0xff, 0x0a]);
    }

    #[tokio::test]
    async fn a_command_ending_in_a_line_continuation_still_runs() {
        // Safe only because the command is the last thing in the recipe. A line
        // appended below it would be eaten by the backslash, and a trailing
        // comment would eat it too.
        let dir = tempfile::tempdir().unwrap();
        let runner = noisy_runner(dir.path());

        let output = runner
            .execute_for_content(
                "printf '%s' \\\n 'continued'",
                dir.path(),
                TIMEOUT,
                &token(),
            )
            .await
            .unwrap();

        assert_eq!(String::from_utf8_lossy(&output.into_stdout()), "continued");
    }

    #[tokio::test]
    async fn a_command_ending_in_a_comment_still_runs() {
        let dir = tempfile::tempdir().unwrap();
        let runner = noisy_runner(dir.path());

        let output = runner
            .execute_for_content(
                "printf '%s' 'commented' # why",
                dir.path(),
                TIMEOUT,
                &token(),
            )
            .await
            .unwrap();

        assert_eq!(String::from_utf8_lossy(&output.into_stdout()), "commented");
    }

    #[tokio::test]
    async fn a_command_that_installs_its_own_exit_trap_loses_the_tail_guarantee() {
        // The one shape the separation cannot cover: the command displaces the
        // trap selfie uses to find the end of its output, so what that trap
        // prints is appended to the content. The content is still returned —
        // refusing it would break a working command — and the caller is told the
        // tail is not established, because appending foreign bytes to a
        // credential and reporting success is the defect this change exists to
        // fix.
        let dir = tempfile::tempdir().unwrap();
        let runner = noisy_runner(dir.path());

        let output = runner
            .execute_for_content(
                &format!("trap 'printf MYCLEANUP' EXIT; printf '%s' '{SECRET}'"),
                dir.path(),
                TIMEOUT,
                &token(),
            )
            .await
            .unwrap();

        assert!(!output.tail_verified());
        assert_eq!(
            String::from_utf8_lossy(&output.into_stdout()),
            format!("{SECRET}MYCLEANUP"),
        );
    }

    #[tokio::test]
    async fn content_capture_fails_closed_when_it_cannot_find_the_command_s_output() {
        // A shell whose startup files exit before the command runs — the recipe
        // is never evaluated, so neither marker is there to find. What was
        // captured may be anything; it is not the command's output, and guessing
        // is what this path exists to stop.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("exiting-shell");
        test_common::write_executable(&path, "#!/bin/sh\nprintf '%s' 'goodbye'\nexit 0\n");
        let runner = ShellCommandRunner::new(path.to_str().unwrap(), TIMEOUT);

        let result = runner
            .execute_for_content("printf '%s' 'anything'", dir.path(), TIMEOUT, &token())
            .await;

        assert!(
            matches!(result, Err(CommandError::ContentMarkersAbsent { .. })),
            "expected a refusal, got {result:?}"
        );
    }

    // A stand-in shell whose startup does `redirect` before running its `-c`.
    fn shell_redirecting(dir: &Path, name: &str, redirect: &str) -> PathBuf {
        let path = dir.join(name);
        test_common::write_executable(
            &path,
            &format!("#!/bin/sh\n{redirect}\nshift $(($# - 1)); eval \"$1\"\n"),
        );
        path
    }

    #[tokio::test]
    async fn a_profile_holding_the_conventional_descriptors_is_harmless() {
        // `exec 3>somewhere` is an ordinary debugging idiom and `exec 3>&1` an
        // ordinary logging one; the first is handed the credential and the second
        // breaks every apply if the content travels on the descriptor it took.
        // 9 is `flock`'s. None of the three may be the one selfie uses.
        let dir = tempfile::tempdir().unwrap();
        let stolen = dir.path().join("stolen.log");
        let shell = shell_redirecting(
            dir.path(),
            "collecting-shell",
            &format!("exec 3>{} 4>&3 9>&1", stolen.display()),
        );
        let runner = ShellCommandRunner::new(shell.to_str().unwrap(), TIMEOUT);

        let output = runner
            .execute_for_content(
                &format!("printf '%s' '{SECRET}'"),
                dir.path(),
                TIMEOUT,
                &token(),
            )
            .await
            .unwrap();

        assert_eq!(String::from_utf8_lossy(&output.into_stdout()), SECRET);
        assert_eq!(
            std::fs::read_to_string(&stolen).unwrap_or_default(),
            "",
            "the credential was written where the profile could read it"
        );
    }

    #[tokio::test]
    async fn a_profile_holding_the_capture_descriptor_fails_closed() {
        // The documented residual, pinned: a startup file that takes the one
        // descriptor selfie uses is handed the content and selfie captures
        // nothing. It must refuse rather than deploy whatever is left.
        let dir = tempfile::tempdir().unwrap();
        let shell = shell_redirecting(
            dir.path(),
            "greedy-shell",
            &format!(
                "exec {}>{}/stolen.log",
                content::CAPTURE_FD,
                dir.path().display()
            ),
        );
        let runner = ShellCommandRunner::new(shell.to_str().unwrap(), TIMEOUT);

        let result = runner
            .execute_for_content("printf '%s' 'anything'", dir.path(), TIMEOUT, &token())
            .await;

        assert!(
            matches!(result, Err(CommandError::ContentMarkersAbsent { .. })),
            "expected a refusal, got {result:?}"
        );
    }

    #[tokio::test]
    async fn an_unusable_shell_reports_what_the_wrapper_said_about_it() {
        // The wrapper turns a spawn failure into a non-zero exit with the
        // diagnosis on stderr. Reporting absent markers instead would leave the
        // user with no idea their `$SHELL` is the problem.
        let dir = tempfile::tempdir().unwrap();
        let runner = ShellCommandRunner::new("/nonexistent/shell", TIMEOUT);

        let output = runner
            .execute_for_content("printf hi", dir.path(), TIMEOUT, &token())
            .await
            .expect("an unusable shell is a command failure, not an unreadable capture");

        assert!(!output.is_success());
        assert!(
            String::from_utf8_lossy(output.stderr()).contains("/nonexistent/shell"),
            "stderr must name the shell: {:?}",
            String::from_utf8_lossy(output.stderr())
        );
        // `run_capture` refuses a failed capture before reading this, so the
        // empty buffer is not what keeps it out of a file. Asserted anyway, so
        // that a later edit filling it in has to be a deliberate one.
        assert!(output.into_stdout().is_empty());
    }

    #[tokio::test]
    async fn the_command_cannot_write_to_the_capture_descriptor() {
        // Closed in the recipe, so a command cannot address the descriptor its
        // own output travels on.
        let dir = tempfile::tempdir().unwrap();
        let runner = noisy_runner(dir.path());

        let output = runner
            .execute_for_content(
                "for n in 5 6 7 8 9; do eval \"printf REACHED >&$n\" 2>/dev/null; done; \
                 printf '%s' 'done'",
                dir.path(),
                TIMEOUT,
                &token(),
            )
            .await
            .unwrap();

        assert_eq!(String::from_utf8_lossy(&output.into_stdout()), "done");
    }

    #[tokio::test]
    async fn a_failure_names_the_command_the_user_wrote() {
        // The shell is given a recipe selfie composed. A user reading a timeout
        // must see the command from their package file, not that.
        let dir = tempfile::tempdir().unwrap();
        let runner = noisy_runner(dir.path());

        let result = runner
            .execute_for_content("sleep 30", dir.path(), Duration::from_millis(200), &token())
            .await;

        let Err(error) = result else {
            panic!("expected a timeout, got {result:?}");
        };
        let rendered = error.to_string();
        assert!(rendered.contains("sleep 30"), "{rendered}");
        assert!(
            !rendered.contains("printf"),
            "the recipe leaked: {rendered}"
        );
        assert!(
            !rendered.contains("exec >&3"),
            "the recipe leaked: {rendered}"
        );
    }
}
