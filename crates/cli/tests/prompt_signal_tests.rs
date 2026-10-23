// How a prompt on a real terminal ends: answered, interrupted by a signal, or by
// Ctrl+C typed at it. Each run drives `selfie` on a pseudoterminal, waits for the
// question, acts, and then reads back what the terminal was left as.
//
// Every wait is bounded and a run still alive at its deadline is killed, so a
// prompt that never ends fails the test rather than hanging the suite.

pub mod common;

use std::{
    fs::File,
    io::{ErrorKind, Read, Write},
    os::fd::OwnedFd,
    path::{Path, PathBuf},
    process::{Child, Stdio},
    thread::sleep,
    time::{Duration, Instant},
};

use common::{CANCELED, CLEAN, SELFIE_ENV, sandboxed_std_command, setup_default_test_config};
use nix::{
    fcntl::{FcntlArg, OFlag, fcntl},
    pty::openpty,
    sys::{
        signal::{Signal, kill},
        termios::{LocalFlags, tcgetattr},
    },
    unistd::Pid,
};
use tempfile::TempDir;

// Long enough for a debug binary on a loaded machine to reach its question.
const DEADLINE: Duration = Duration::from_secs(20);

const HIDE_CURSOR: &str = "\x1b[?25l";
const SHOW_CURSOR: &str = "\x1b[?25h";

// `selfie` running on a pseudoterminal, with everything it has written so far.
struct PtyRun {
    child: Child,
    master: File,
    // Held open so the terminal's settings can still be read after the child exits.
    slave: OwnedFd,
    output: Vec<u8>,
}

// How a run ended, and the terminal it left behind.
struct Finished {
    code: Option<i32>,
    output: String,
    // ICANON and ECHO are both on: the terminal is back in cooked mode.
    cooked: bool,
}

impl PtyRun {
    fn spawn(temp: &TempDir, args: &[&str]) -> Self {
        let pty = openpty(None, None).unwrap();
        fcntl(&pty.master, FcntlArg::F_SETFL(OFlag::O_NONBLOCK)).unwrap();
        let child = sandboxed_std_command(temp)
            .args(args)
            .env("TERM", "xterm")
            .stdin(Stdio::from(pty.slave.try_clone().unwrap()))
            .stdout(Stdio::from(pty.slave.try_clone().unwrap()))
            .stderr(Stdio::from(pty.slave.try_clone().unwrap()))
            .spawn()
            .unwrap();
        Self {
            child,
            master: File::from(pty.master),
            slave: pty.slave,
            output: Vec::new(),
        }
    }

    fn drain(&mut self) {
        let mut buf = [0_u8; 4096];
        loop {
            match self.master.read(&mut buf) {
                Ok(0) => return,
                Ok(n) => self.output.extend_from_slice(&buf[..n]),
                Err(error) if error.kind() == ErrorKind::WouldBlock => return,
                Err(error) if error.kind() == ErrorKind::Interrupted => {}
                // Linux reports a closed slave side as EIO.
                Err(_) => return,
            }
        }
    }

    fn text(&self) -> String {
        String::from_utf8_lossy(&self.output).into_owned()
    }

    fn wait_for_text(&mut self, needle: &str) {
        let start = Instant::now();
        while !self.text().contains(needle) {
            assert!(
                start.elapsed() < DEADLINE,
                "never saw {needle:?}; the run wrote:\n{}",
                self.text()
            );
            self.drain();
            sleep(Duration::from_millis(10));
        }
    }

    // The question is drawn before the terminal goes raw, and a 0x03 typed in
    // that window is taken by the line discipline as an interrupt and signals
    // nobody: the child has no controlling terminal.
    fn wait_for_raw_mode(&mut self) {
        let start = Instant::now();
        while self.terminal_flags().contains(LocalFlags::ICANON) {
            assert!(
                start.elapsed() < DEADLINE,
                "the terminal never went raw; the run wrote:\n{}",
                self.text()
            );
            self.drain();
            sleep(Duration::from_millis(10));
        }
    }

    fn terminal_flags(&self) -> LocalFlags {
        tcgetattr(&self.slave).unwrap().local_flags
    }

    fn type_keys(&mut self, keys: &[u8]) {
        self.wait_for_raw_mode();
        self.master.write_all(keys).unwrap();
    }

    fn signal(&self, signal: Signal) {
        kill(Pid::from_raw(self.child.id().try_into().unwrap()), signal).unwrap();
    }

    fn finish(mut self) -> Finished {
        let start = Instant::now();
        let status = loop {
            self.drain();
            if let Some(status) = self.child.try_wait().unwrap() {
                break status;
            }
            assert!(
                start.elapsed() < DEADLINE,
                "the run was still going {DEADLINE:?} later; it wrote:\n{}",
                self.text()
            );
            sleep(Duration::from_millis(10));
        };
        self.drain();
        let flags = self.terminal_flags();
        Finished {
            code: status.code(),
            output: self.text(),
            cooked: flags.contains(LocalFlags::ICANON) && flags.contains(LocalFlags::ECHO),
        }
    }
}

impl Drop for PtyRun {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Finished {
    // The terminal is usable again: cooked mode, and the cursor dialoguer hid
    // shown again.
    fn assert_terminal_restored(&self) {
        assert!(self.cooked, "the terminal was left raw:\n{}", self.output);
        let hidden = self
            .output
            .rfind(HIDE_CURSOR)
            .expect("the prompt hid the cursor");
        assert!(
            self.output[hidden..].contains(SHOW_CURSOR),
            "the cursor was left hidden:\n{:?}",
            self.output
        );
    }
}

// A sandbox holding one spec, `doomed`, that `spec remove` asks about.
fn one_spec() -> (TempDir, PathBuf) {
    let temp = setup_default_test_config();
    let spec = temp.path().join("packages/doomed.yaml");
    std::fs::write(
        &spec,
        format!("name: doomed\nenvironments:\n  {SELFIE_ENV}:\n    install: \"true\"\n"),
    )
    .unwrap();
    (temp, spec)
}

fn remove_doomed(temp: &TempDir) -> PtyRun {
    let mut run = PtyRun::spawn(temp, &["spec", "remove", "doomed"]);
    run.wait_for_text("Remove package 'doomed'?");
    run
}

fn exists(path: &Path) -> bool {
    path.try_exists().unwrap()
}

#[test]
fn a_signal_at_a_main_thread_prompt_ends_the_command_as_canceled() {
    let (temp, spec) = one_spec();
    let run = remove_doomed(&temp);

    run.signal(Signal::SIGINT);
    let finished = run.finish();

    assert_eq!(finished.code, Some(CANCELED), "{}", finished.output);
    finished.assert_terminal_restored();
    assert!(exists(&spec), "a canceled remove removed the spec");
}

// The control: an answered prompt still does what it was told.
#[test]
fn an_answered_prompt_still_answers() {
    let (temp, spec) = one_spec();
    let mut run = remove_doomed(&temp);

    run.type_keys(b"y");
    let finished = run.finish();

    assert_eq!(finished.code, Some(CLEAN), "{}", finished.output);
    finished.assert_terminal_restored();
    assert!(!exists(&spec), "the answered remove left the spec");
}
