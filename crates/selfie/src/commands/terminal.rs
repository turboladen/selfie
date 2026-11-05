//! A command's controlling terminal, and how a prompt written to it is answered.
//!
//! A program such as `sudo` or `mas` asks for a password by writing to
//! `/dev/tty` and reading from it, which bypasses the pipes selfie captures.
//! The shell runner therefore gives each command a pseudo-terminal as its
//! controlling terminal. What a command writes there reaches
//! [`watch`], and an answer goes back through the same pseudo-terminal.

use std::{future::Future, path::Path, pin::Pin, sync::Arc, time::Duration};

use nix::sys::termios::{LocalFlags, SetArg, tcgetattr, tcsetattr};
use pty_process::{Pts, Pty};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use zeroize::Zeroizing;

use super::runner::{BoundedText, CommandError};

/// How long a terminal stays quiet before text without a final newline counts
/// as a question.
// A prompt is written once and then waited on, while a program drawing progress
// on its terminal keeps writing. The pause tells them apart, and 250 ms is long
// enough for a prompt split across two writes to arrive whole.
const QUIET: Duration = Duration::from_millis(250);

/// How much terminal text is kept while waiting to see whether it is a prompt.
const KEPT_BYTES: usize = 4096;

/// What a command wrote to its terminal and then waited on.
#[derive(Debug, Clone)]
pub struct TerminalPrompt {
    text: String,
}

impl TerminalPrompt {
    /// The prompt a command wrote as `text`, whose last line is the question.
    /// Carriage returns are removed.
    #[must_use]
    pub fn new(text: &str) -> Self {
        Self {
            text: text.replace('\r', ""),
        }
    }

    /// The text as the command wrote it, carriage returns removed. It may hold
    /// terminal escape sequences, so a caller shows it escaped.
    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }

    /// The last line of [`text`](Self::text), which is the question itself.
    #[must_use]
    pub fn question(&self) -> &str {
        self.split().1.trim()
    }

    /// The lines above [`question`](Self::question), which is what the command
    /// said before it asked. Empty when it said nothing.
    #[must_use]
    pub fn said(&self) -> &str {
        self.split().0.trim()
    }

    fn split(&self) -> (&str, &str) {
        self.text.rsplit_once('\n').unwrap_or(("", &self.text))
    }
}

/// What the user typed in answer to a [`TerminalPrompt`], kept out of every
/// `Debug` output and wiped from memory when dropped.
pub struct TerminalAnswer(Zeroizing<String>);

impl TerminalAnswer {
    /// Wrap `text`, which is sent to the command followed by a newline.
    #[must_use]
    pub fn new(text: String) -> Self {
        Self(Zeroizing::new(text))
    }
}

// The answer to a password prompt is a password, so nothing that formats this
// type may print it. `Zeroizing<String>` derives a `Debug` that does.
impl std::fmt::Debug for TerminalAnswer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "TerminalAnswer(<{} bytes>)", self.0.len())
    }
}

type ReplyFuture = Pin<Box<dyn Future<Output = Option<TerminalAnswer>> + Send>>;

/// Answers the prompts a command writes to its terminal.
///
/// A handler answers with `None` when nobody can, which fails the command with
/// [`CommandError::InputRequired`]. A runner built without one fails any command
/// that asks. The caller supplies the handler because only it knows whether a
/// person is at a terminal.
#[derive(Clone)]
pub struct PromptHandler(Arc<dyn Fn(TerminalPrompt) -> ReplyFuture + Send + Sync>);

impl PromptHandler {
    /// Build a handler from `answer`. It may be called from several commands at
    /// once and has to serialize whatever it shows.
    pub fn new<F, Fut>(answer: F) -> Self
    where
        F: Fn(TerminalPrompt) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Option<TerminalAnswer>> + Send + 'static,
    {
        Self(Arc::new(move |prompt| Box::pin(answer(prompt))))
    }
}

impl std::fmt::Debug for PromptHandler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PromptHandler")
    }
}

/// The selfie side of a command's terminal.
pub(crate) struct Terminal {
    pty: Pty,
    // Held for as long as the command runs. macOS drops a session's controlling
    // terminal when the last descriptor on the slave closes, and the command's
    // own copies are gone once it execs because its stdin is closed and its
    // output is piped. Without this one, `/dev/tty` fails with "Device not
    // configured". Linux keeps the terminal either way.
    _slave: std::os::fd::OwnedFd,
}

/// Open a pseudo-terminal for one command, or `None` when the system cannot
/// provide one, in which case the command runs without a terminal. The `Pts`
/// goes to the command.
pub(crate) fn open() -> Option<(Terminal, Pts)> {
    let (pty, pts) = pty_process::open()
        .inspect_err(|error| tracing::debug!(%error, "no pseudo-terminal for a command"))
        .ok()?;
    // Echo is off so that what selfie writes back, which may be a password, is
    // never copied onto the terminal's output and read as the next prompt. A
    // program that wants echo, such as `sudo`, turns it on or off for itself.
    // The settings go on the slave side because macOS refuses them on a master
    // whose slave is not open yet.
    if let Ok(mut settings) = tcgetattr(&pts) {
        settings.local_flags.remove(LocalFlags::ECHO);
        let _ = tcsetattr(&pts, SetArg::TCSANOW, &settings);
    }
    let slave = std::os::fd::AsFd::as_fd(&pts).try_clone_to_owned().ok()?;
    Some((Terminal { pty, _slave: slave }, pts))
}

/// Read what a command writes to its terminal and answer its prompts.
///
/// Resolves only with the failure that should end the command: it never
/// resolves while the command runs and asks nothing, and it stays pending once
/// the terminal closes, so the caller races it against the command's exit.
pub(crate) async fn watch(
    terminal: Option<Terminal>,
    handler: Option<&PromptHandler>,
    command: &str,
    working_directory: &Path,
) -> CommandError {
    let Some(Terminal { mut pty, _slave }) = terminal else {
        return std::future::pending().await;
    };
    let mut shown = Vec::new();
    let mut chunk = [0u8; 1024];
    loop {
        // Only text that stops mid-line can be a question, and only once the
        // program has stopped writing.
        let asking = !shown.is_empty() && !shown.ends_with(b"\n");
        let read = if asking {
            tokio::time::timeout(QUIET, pty.read(&mut chunk)).await
        } else {
            Ok(pty.read(&mut chunk).await)
        };
        match read {
            Err(_quiet) => {
                let prompt =
                    TerminalPrompt::new(&String::from_utf8_lossy(&std::mem::take(&mut shown)));
                let asked = BoundedText::bound(prompt.text().as_bytes());
                let answer = match handler {
                    Some(handler) => (handler.0)(prompt).await,
                    None => None,
                };
                let Some(answer) = answer else {
                    return CommandError::InputRequired {
                        command: command.to_string(),
                        working_directory: working_directory.to_path_buf(),
                        prompt: asked,
                    };
                };
                // Two writes, so the answer is never copied into a buffer that
                // holds the newline too and is freed without being wiped. A
                // terminal in line mode hands nothing to the command until the
                // newline arrives.
                let sent = pty.write_all(answer.0.as_bytes()).await;
                if sent.and(pty.write_all(b"\n").await).is_err() {
                    break;
                }
            }
            Ok(Ok(0) | Err(_)) => break,
            Ok(Ok(read)) => {
                shown.extend_from_slice(&chunk[..read]);
                if shown.len() > KEPT_BYTES {
                    shown.drain(..shown.len() - KEPT_BYTES);
                }
            }
        }
    }
    std::future::pending().await
}
