//! A command's controlling terminal, and how a prompt written to it is answered.
//!
//! A program such as `sudo` or `mas` asks for a password by writing to
//! `/dev/tty` and reading from it, which bypasses the pipes selfie captures.
//! The shell runner therefore gives each command a pseudo-terminal as its
//! controlling terminal. What a command writes there reaches
//! [`watch`], and an answer goes back through the same pseudo-terminal.

use std::{future::Future, path::Path, pin::Pin, sync::Arc, time::Duration};

use tokio::{sync::watch, time::Instant};

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

/// The time one command may run, which does not pass while selfie waits on
/// something other than the command: a question of its own being answered, or
/// selfie's own reader taking its output.
// A person typing a password is not the command running, and neither is a slow
// reader holding up the relay, so a budget that kept counting would end the
// command for selfie's own delay. The two waits can overlap, so pausing is
// counted: the deadline is `None` exactly while `depth > 0`, and `left` holds
// what was unspent when the first pause began.
pub(crate) struct Budget {
    timeout: Duration,
    deadline: watch::Sender<Option<Instant>>,
    state: std::sync::Mutex<Clock>,
}

// `Budget` keeps these under its lock: the time unspent at the first pause, and
// how many pauses are open.
struct Clock {
    left: Duration,
    depth: usize,
}

/// A pause of a [`Budget`], which resumes it when dropped.
#[must_use = "the budget resumes as soon as the pause is dropped"]
pub(crate) struct Paused<'a>(&'a Budget);

impl Drop for Paused<'_> {
    fn drop(&mut self) {
        self.0.resume();
    }
}

impl Budget {
    pub(crate) fn new(timeout: Duration) -> Self {
        Self {
            timeout,
            deadline: watch::Sender::new(Some(Instant::now() + timeout)),
            state: std::sync::Mutex::new(Clock {
                left: timeout,
                depth: 0,
            }),
        }
    }

    // Locks the clock's state. It is plain data that every update leaves whole, so a
    // panic while it was held leaves nothing half-written to refuse.
    fn clock(&self) -> std::sync::MutexGuard<'_, Clock> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Stops the clock until the returned guard is dropped. Pauses nest: the
    /// clock runs again only once every open pause has ended.
    pub(crate) fn pause(&self) -> Paused<'_> {
        // The deadline is copied out in a statement of its own: the `Ref` holds
        // the channel's read lock, and `send_replace` below needs the write lock.
        let deadline = *self.deadline.borrow();
        let mut clock = self.clock();
        clock.depth += 1;
        if clock.depth == 1 {
            if let Some(at) = deadline {
                clock.left = at.saturating_duration_since(Instant::now());
            }
            self.deadline.send_replace(None);
        }
        Paused(self)
    }

    // Ends one pause. The clock resumes with what was unspent when the first
    // pause began, or with the whole allowance if output restarted it meanwhile.
    fn resume(&self) {
        let mut clock = self.clock();
        clock.depth = clock.depth.saturating_sub(1);
        if clock.depth == 0 {
            self.deadline
                .send_replace(Some(Instant::now() + clock.left));
        }
    }

    /// Gives the command its whole allowance again, so the budget counts time
    /// since the last restart. A paused budget stays paused and resumes with
    /// the whole allowance.
    pub(crate) fn restart(&self) {
        let mut clock = self.clock();
        clock.left = self.timeout;
        if clock.depth == 0 {
            self.deadline
                .send_replace(Some(Instant::now() + self.timeout));
        }
    }

    /// Resolves once the time has run out, not counting any time paused.
    pub(crate) async fn expired(&self) {
        let mut changes = self.deadline.subscribe();
        loop {
            let deadline = *changes.borrow_and_update();
            match deadline {
                Some(at) => {
                    // The select is biased toward a change, so a restart or a pause
                    // landing in the same poll as the old deadline is honored.
                    tokio::select! {
                        biased;
                        _ = changes.changed() => {}
                        () = tokio::time::sleep_until(at) => return,
                    }
                }
                None => {
                    let _ = changes.changed().await;
                }
            }
        }
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

/// How many times a pseudo-terminal is opened before giving up on it.
// On macOS, `posix_openpt` can fail with `ENXIO` ("Device not configured") for
// an instant while another pseudo-terminal is closing, whether that one is
// selfie's own or another process's. A command that gets no terminal runs on
// selfie's own, so its question never reaches the handler. An immediate second
// attempt succeeds, and a failure that lasts, such as running out of
// pseudo-terminals, fails every attempt and is logged once.
const OPEN_ATTEMPTS: usize = 5;

/// Call `attempt` once, and again after each failure up to `attempts` calls in
/// all, and return the last result.
fn retrying<T, E>(attempts: usize, mut attempt: impl FnMut() -> Result<T, E>) -> Result<T, E> {
    for _ in 1..attempts {
        if let ok @ Ok(_) = attempt() {
            return ok;
        }
    }
    attempt()
}

/// Open a pseudo-terminal for one command, or `None` when the system cannot
/// provide one, in which case the command runs without a terminal. The `Pts`
/// goes to the command.
pub(crate) fn open() -> Option<(Terminal, Pts)> {
    // A command that runs without a terminal inherits selfie's own, so a prompt it
    // writes to `/dev/tty` reaches the user behind whatever selfie is drawing.
    // Each way of ending up there is logged, because nothing else says so.
    const WITHOUT: &str = "a command runs without a terminal of its own";
    let (pty, pts) = retrying(OPEN_ATTEMPTS, pty_process::open)
        .inspect_err(|error| tracing::warn!(%error, "{WITHOUT}: no pseudo-terminal"))
        .ok()?;
    // Echo is off so that what selfie writes back, which may be a password, is
    // never copied onto the terminal's output and read as the next prompt. A
    // program that wants echo, such as `sudo`, turns it on or off for itself.
    // The settings go on the slave side because macOS refuses them on a master
    // whose slave is not open yet.
    let mut settings = tcgetattr(&pts)
        .inspect_err(|error| tracing::warn!(%error, "{WITHOUT}: no terminal settings"))
        .ok()?;
    settings.local_flags.remove(LocalFlags::ECHO);
    tcsetattr(&pts, SetArg::TCSANOW, &settings)
        .inspect_err(|error| tracing::warn!(%error, "{WITHOUT}: echo stays on"))
        .ok()?;
    let slave = std::os::fd::AsFd::as_fd(&pts)
        .try_clone_to_owned()
        .inspect_err(|error| tracing::warn!(%error, "{WITHOUT}: no spare descriptor"))
        .ok()?;
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
    budget: &Budget,
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
                let paused = budget.pause();
                let answer = match handler {
                    Some(handler) => (handler.0)(prompt).await,
                    None => None,
                };
                drop(paused);
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

#[cfg(test)]
mod tests {
    use super::*;

    // The clock is paused, so the sleeps below cost nothing and the deadlines
    // are exact.
    #[tokio::test(start_paused = true)]
    async fn a_restart_gives_the_whole_allowance_again() {
        let budget = Budget::new(Duration::from_secs(10));
        tokio::time::sleep(Duration::from_secs(6)).await;

        budget.restart();

        // 9 seconds after the restart is inside the allowance; 10 is not.
        let early = tokio::time::timeout(Duration::from_secs(9), budget.expired()).await;
        assert!(
            early.is_err(),
            "the budget ran out before its restart allowance"
        );
        let late = tokio::time::timeout(Duration::from_secs(2), budget.expired()).await;
        assert!(late.is_ok(), "the budget outlasted its restart allowance");
    }

    #[tokio::test(start_paused = true)]
    async fn a_restart_while_paused_leaves_the_budget_paused_and_resumes_with_the_whole_allowance()
    {
        let budget = Budget::new(Duration::from_secs(10));
        tokio::time::sleep(Duration::from_secs(6)).await;
        let pause = budget.pause();

        budget.restart();

        let paused = tokio::time::timeout(Duration::from_secs(100), budget.expired()).await;
        assert!(paused.is_err(), "a paused budget expired");
        drop(pause);
        let resumed = tokio::time::timeout(Duration::from_secs(9), budget.expired()).await;
        assert!(
            resumed.is_err(),
            "resumed with less than the whole allowance"
        );
        let expired = tokio::time::timeout(Duration::from_secs(2), budget.expired()).await;
        assert!(expired.is_ok());
    }

    // A question and a blocked relay can each pause the same budget, and either
    // can end first. The clock must stay stopped until both have.
    #[tokio::test(start_paused = true)]
    async fn nested_pauses_hold_the_budget_until_the_last_resume() {
        let budget = Budget::new(Duration::from_secs(10));
        tokio::time::sleep(Duration::from_secs(4)).await;
        let outer = budget.pause();
        let inner = budget.pause();

        drop(inner);

        let held = tokio::time::timeout(Duration::from_secs(100), budget.expired()).await;
        assert!(held.is_err(), "the budget ran while a pause was still open");
        drop(outer);
        // It resumes with the 6 seconds it had left at the first pause.
        let early = tokio::time::timeout(Duration::from_secs(5), budget.expired()).await;
        assert!(early.is_err(), "resumed with less than it had left");
        let late = tokio::time::timeout(Duration::from_secs(2), budget.expired()).await;
        assert!(
            late.is_ok(),
            "the budget never ran again after the last resume"
        );
    }

    #[test]
    fn a_failure_that_clears_is_retried_until_it_succeeds() {
        let mut calls = 0;

        let result = retrying(5, || {
            calls += 1;
            if calls < 3 { Err("busy") } else { Ok(calls) }
        });

        assert_eq!(result, Ok(3));
    }

    #[test]
    fn a_failure_that_lasts_is_reported_after_the_last_attempt() {
        let mut calls = 0;

        let result: Result<(), usize> = retrying(5, || {
            calls += 1;
            Err(calls)
        });

        // The error is the last attempt's, and no attempt is made beyond the limit.
        assert_eq!(result, Err(5));
        assert_eq!(calls, 5);
    }
}
