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

use common::{
    CANCELED, CLEAN, FAILED, SELFIE_ENV, sandboxed_std_command, setup_default_test_config,
    setup_test_config_with_state_directory,
};
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

// A sandbox whose one package deploys `fresh`, a target not there yet, and then
// meets `edited`, a target holding content of its own, which apply asks about.
struct ApplyFixture {
    temp: TempDir,
    fresh: PathBuf,
    edited: PathBuf,
}

fn deploy_then_conflict() -> ApplyFixture {
    let temp = setup_test_config_with_state_directory();
    let root = temp.path();
    for dir in ["state", "packages/app", "home"] {
        std::fs::create_dir_all(root.join(dir)).unwrap();
    }
    std::fs::write(root.join("packages/app/fresh.conf"), "from-repo\n").unwrap();
    std::fs::write(root.join("packages/app/edited.conf"), "from-repo\n").unwrap();
    let fresh = root.join("home/fresh.conf");
    let edited = root.join("home/edited.conf");
    std::fs::write(&edited, "hand-edited\n").unwrap();
    std::fs::write(
        root.join("packages/app.yaml"),
        format!(
            "name: app\nenvironments:\n  {SELFIE_ENV}:\n    install: \"true\"\ndotfiles:\n  \
             - source: app/fresh.conf\n    target: \"{}\"\n  \
             - source: app/edited.conf\n    target: \"{}\"\n",
            fresh.display(),
            edited.display()
        ),
    )
    .unwrap();
    ApplyFixture {
        temp,
        fresh,
        edited,
    }
}

fn apply_to_the_conflict(fixture: &ApplyFixture) -> PtyRun {
    let mut run = PtyRun::spawn(&fixture.temp, &["apply"]);
    run.wait_for_text("How should this conflict be resolved?");
    run
}

impl ApplyFixture {
    // apply ended as canceled: it said so, left the conflicted target as it was,
    // and kept the record of the entry it deployed before the prompt.
    fn assert_canceled_at_the_conflict(&self, finished: &Finished) {
        assert_eq!(finished.code, Some(CANCELED), "{}", finished.output);
        assert!(
            finished.output.contains("Apply canceled"),
            "{}",
            finished.output
        );
        assert_eq!(
            std::fs::read_to_string(&self.edited).unwrap(),
            "hand-edited\n"
        );
        assert_eq!(std::fs::read_to_string(&self.fresh).unwrap(), "from-repo\n");
        let state =
            std::fs::read_to_string(self.temp.path().join("state/deploy-state.yml")).unwrap();
        assert!(
            state.contains(&self.fresh.display().to_string()),
            "the deploy before the prompt went unrecorded:\n{state}"
        );
        assert!(
            !state.contains(&self.edited.display().to_string()),
            "the conflict was recorded as deployed:\n{state}"
        );
    }
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

// The shell's prompt must not start on the question's line.
#[test]
fn ctrl_c_typed_at_a_question_ends_its_line() {
    let (temp, spec) = one_spec();
    let mut run = remove_doomed(&temp);

    run.type_keys(b"\x03");
    let finished = run.finish();

    assert_eq!(finished.code, Some(CANCELED), "{}", finished.output);
    finished.assert_terminal_restored();
    let (_, after) = finished
        .output
        .rsplit_once("Remove package 'doomed'?")
        .unwrap();
    assert!(after.contains('\n'), "{:?}", finished.output);
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

// Enter alone answers a confirm with its default, which for removing a spec is
// no.
#[test]
fn enter_alone_takes_the_confirms_default() {
    let (temp, spec) = one_spec();
    let mut run = remove_doomed(&temp);

    run.type_keys(b"\r");
    let finished = run.finish();

    assert_eq!(finished.code, Some(FAILED), "{}", finished.output);
    assert!(
        finished.output.contains("Package removal canceled."),
        "{}",
        finished.output
    );
    assert!(exists(&spec), "Enter alone removed the spec");
}

// A prompt draws on stderr and follows its color decision: the colorful theme,
// whose question ends in `›`, on a terminal with colors on, and the plain
// theme's `[y/N]` with them off.
#[test]
fn a_prompt_follows_stderrs_colors() {
    let (temp, _spec) = one_spec();

    let mut colored = remove_doomed(&temp);
    colored.wait_for_text("›");
    colored.type_keys(b"n");
    let colored = colored.finish();

    let mut plain = PtyRun::spawn(&temp, &["--no-color", "spec", "remove", "doomed"]);
    plain.wait_for_text("Remove package 'doomed'? [y/N] ");
    plain.type_keys(b"n");
    let plain = plain.finish();

    assert!(!colored.output.contains("[y/N]"), "{}", colored.output);
    assert!(!plain.output.contains('›'), "{}", plain.output);
}

// An environment name the prompt can only show escaped is noted escaped above
// an empty prompt, never drawn raw, and Enter writes its raw bytes.
#[test]
fn an_escaped_environment_is_noted_and_enter_keeps_it() {
    let temp = setup_default_test_config();
    let mut run = PtyRun::spawn(
        &temp,
        &[
            "--environment",
            "dev\u{1b}",
            "spec",
            "create",
            "fresh",
            "--interactive",
        ],
    );
    for (question, keys) in [
        ("Package name", &b"\r"[..]),
        ("Homepage URL", b"\r"),
        ("Description", b"\r"),
        ("Environment name", b"\r"),
        ("Install command", b"true\r"),
        ("Check command", b"\r"),
        ("Add another environment?", b"\r"),
        ("open the package file", b"n"),
    ] {
        run.wait_for_text(question);
        run.type_keys(keys);
    }
    let finished = run.finish();

    assert_eq!(finished.code, Some(CLEAN), "{}", finished.output);
    assert!(
        finished
            .output
            .contains(r"Suggested: dev\u{1b} (Enter keeps it)"),
        "{}",
        finished.output
    );
    assert!(
        !finished.output.contains("dev\u{1b}"),
        "{:?}",
        finished.output
    );
    let spec = std::fs::read_to_string(temp.path().join("packages/fresh.yml")).unwrap();
    let spec: selfie::package::Package = selfie::yaml::parse(&spec).unwrap();
    assert!(
        spec.environments().contains_key("dev\u{1b}"),
        "{:?}",
        spec.environments().keys().collect::<Vec<_>>()
    );
}

// A package name the spec-name rule refuses is not pre-filled at the name
// prompt, where Enter would only send it back to be refused.
#[test]
fn a_refused_package_name_is_not_prefilled() {
    let temp = setup_default_test_config();
    let mut run = PtyRun::spawn(&temp, &["spec", "create", "my app", "--interactive"]);
    run.wait_for_text("Package name");

    // Raw mode starts after any pre-filled text is written.
    run.type_keys(b"\x03");
    let finished = run.finish();

    assert_eq!(finished.code, Some(CANCELED), "{}", finished.output);
    assert!(!finished.output.contains("my app"), "{:?}", finished.output);
}

// apply asks about a conflict on a thread of its own, which a signal never
// reaches, so the run's cancellation has to end the prompt.
#[test]
fn a_signal_at_a_conflict_prompt_ends_apply_as_canceled() {
    let fixture = deploy_then_conflict();
    let run = apply_to_the_conflict(&fixture);

    run.signal(Signal::SIGINT);
    let finished = run.finish();

    fixture.assert_canceled_at_the_conflict(&finished);
    finished.assert_terminal_restored();
}

#[test]
fn sigterm_at_a_conflict_prompt_ends_apply_as_canceled() {
    let fixture = deploy_then_conflict();
    let run = apply_to_the_conflict(&fixture);

    run.signal(Signal::SIGTERM);
    let finished = run.finish();

    fixture.assert_canceled_at_the_conflict(&finished);
    finished.assert_terminal_restored();
}

// Ctrl+C typed at the prompt is a key, not a signal: the run is canceled without
// the hint about signaling again.
#[test]
fn ctrl_c_typed_at_a_conflict_prompt_ends_apply_as_canceled() {
    let fixture = deploy_then_conflict();
    let mut run = apply_to_the_conflict(&fixture);

    run.type_keys(b"\x03");
    let finished = run.finish();

    fixture.assert_canceled_at_the_conflict(&finished);
    finished.assert_terminal_restored();
    assert!(
        !finished.output.contains("Canceling"),
        "{}",
        finished.output
    );
}
