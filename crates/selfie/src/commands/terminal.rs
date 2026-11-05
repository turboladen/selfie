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

    /// The last line of [`text`](Self::text), which is the question itself;
    /// the lines above it are what the command said before asking.
    #[must_use]
    pub fn question(&self) -> &str {
        self.text.rsplit('\n').next().unwrap_or_default().trim()
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

/// How a [`PromptHandler`] settled a [`TerminalPrompt`].
#[derive(Debug)]
pub enum TerminalReply {
    /// The user answered.
    Answer(TerminalAnswer),
    /// Nobody can answer, so the command fails with
    /// [`CommandError::InputRequired`].
    Decline,
}

type ReplyFuture = Pin<Box<dyn Future<Output = TerminalReply> + Send>>;

/// Answers the prompts a command writes to its terminal.
///
/// A runner built without one fails any command that asks, because nothing can
/// answer. The caller supplies the handler because only it knows whether a
/// person is at a terminal.
#[derive(Clone)]
pub struct PromptHandler(Arc<dyn Fn(TerminalPrompt) -> ReplyFuture + Send + Sync>);

impl PromptHandler {
    /// Build a handler from `answer`. It may be called from several commands at
    /// once and has to serialize whatever it shows.
    pub fn new<F, Fut>(answer: F) -> Self
    where
        F: Fn(TerminalPrompt) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = TerminalReply> + Send + 'static,
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
    terminal: Terminal,
    handler: Option<&PromptHandler>,
    command: &str,
    working_directory: &Path,
) -> CommandError {
    let Terminal { mut pty, _slave } = terminal;
    let mut shown = Vec::new();
    let mut chunk = [0u8; 1024];
    loop {
        // Only text that stops mid-line can be a question, and only once the
        // program has stopped writing.
        let asking = !shown.is_empty() && !shown.ends_with(b"\n");
        let read = if asking {
            match tokio::time::timeout(QUIET, pty.read(&mut chunk)).await {
                Ok(read) => read,
                Err(_quiet) => {
                    let prompt = prompt_from(&shown);
                    shown.clear();
                    let asked = BoundedText::bound(prompt.text().as_bytes());
                    let Some(reply) = answer(handler, prompt).await else {
                        return CommandError::InputRequired {
                            command: command.to_string(),
                            working_directory: working_directory.to_path_buf(),
                            prompt: asked,
                        };
                    };
                    let mut line = Zeroizing::new(reply.0.as_bytes().to_vec());
                    line.push(b'\n');
                    if pty.write_all(&line).await.is_err() {
                        break;
                    }
                    continue;
                }
            }
        } else {
            pty.read(&mut chunk).await
        };
        match read {
            Ok(0) | Err(_) => break,
            Ok(read) => {
                shown.extend_from_slice(&chunk[..read]);
                if shown.len() > KEPT_BYTES {
                    shown.drain(..shown.len() - KEPT_BYTES);
                }
            }
        }
    }
    std::future::pending().await
}

async fn answer(handler: Option<&PromptHandler>, prompt: TerminalPrompt) -> Option<TerminalAnswer> {
    let reply = (handler?.0)(prompt).await;
    match reply {
        TerminalReply::Answer(answer) => Some(answer),
        TerminalReply::Decline => None,
    }
}

fn prompt_from(bytes: &[u8]) -> TerminalPrompt {
    TerminalPrompt::new(&String::from_utf8_lossy(bytes))
}
