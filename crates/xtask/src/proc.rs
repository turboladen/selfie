//! Running a child under a deadline, with everything it spawns killed afterwards.

use std::io::{Read, Seek, SeekFrom};
use std::os::unix::process::CommandExt;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::Mutex;
use std::thread::sleep;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use nix::errno::Errno;
use nix::sys::signal::{Signal, killpg};
use nix::unistd::Pid;

/// How a child run by [`run`] ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// The child exited on its own. `None` means a signal ended it.
    Exited(Option<i32>),
    /// The deadline passed and the child's process group was killed.
    TimedOut,
}

impl Outcome {
    /// Whether the child exited on its own with status 0.
    pub fn success(self) -> bool {
        self == Outcome::Exited(Some(0))
    }
}

impl std::fmt::Display for Outcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Outcome::Exited(Some(code)) => write!(f, "exit {code}"),
            Outcome::Exited(None) => write!(f, "killed by a signal"),
            Outcome::TimedOut => write!(f, "timed out"),
        }
    }
}

// These are the process groups `run` currently owns. The interrupt handler
// kills them, because a child in its own group never receives the terminal's
// Ctrl-C.
static ACTIVE: Mutex<Vec<Pid>> = Mutex::new(Vec::new());

/// Makes SIGINT, SIGTERM and SIGHUP kill every process group [`run`] owns,
/// then exit with status 130. Call it once, at startup.
///
/// # Errors
///
/// Fails when the handler cannot be installed.
pub fn kill_groups_on_interrupt() -> Result<()> {
    ctrlc::set_handler(|| {
        let groups = ACTIVE.lock().map(|g| g.clone()).unwrap_or_default();
        for pgid in groups {
            kill_group(pgid);
        }
        std::process::exit(130);
    })
    .context("could not install the interrupt handler")
}

/// Runs `cmd` in a fresh process group and waits for it, up to `deadline`.
///
/// The caller sets up stdout and stderr; stdin is always `/dev/null`. When the
/// child exits, or when the deadline passes, every process left in its group is
/// sent `SIGKILL`, so a grandchild cannot outlive the run.
///
/// # Errors
///
/// Fails when the child cannot be spawned or waited on.
pub fn run(cmd: &mut Command, deadline: Duration) -> Result<Outcome> {
    // The group's leader is a placeholder `cat`, not the command. A pid is
    // never reused while a group with that id has a member, so killing the
    // group after reaping the command cannot hit an unrelated process. With
    // the command as leader, reaping it first would free its pid before the
    // kill. `cat` exits when its stdin closes, so the placeholder cannot
    // outlive this process.
    let mut group = Group::new()?;
    let started = Instant::now();
    let mut child = cmd
        .stdin(Stdio::null())
        .process_group(group.pgid.as_raw())
        .spawn()
        .with_context(|| format!("could not start {cmd:?}"))?;

    loop {
        if let Some(status) = child.try_wait()? {
            group.kill();
            return Ok(Outcome::Exited(status.code()));
        }
        if started.elapsed() >= deadline {
            group.kill();
            child.wait()?;
            return Ok(Outcome::TimedOut);
        }
        sleep(Duration::from_millis(100));
    }
}

/// Runs `cmd` like [`run`] and returns its stdout. Its stderr is discarded.
///
/// # Errors
///
/// Fails when the child cannot be spawned or its output cannot be read.
pub fn output(cmd: &mut Command, deadline: Duration) -> Result<(Outcome, String)> {
    let mut file = tempfile::tempfile()?;
    let outcome = run(
        cmd.stdout(file.try_clone()?).stderr(Stdio::null()),
        deadline,
    )?;
    let mut bytes = Vec::new();
    file.seek(SeekFrom::Start(0))?;
    file.read_to_end(&mut bytes)?;
    Ok((outcome, String::from_utf8_lossy(&bytes).into_owned()))
}

fn kill_group(pgid: Pid) {
    match killpg(pgid, Signal::SIGKILL) {
        Ok(()) | Err(Errno::ESRCH) => {}
        Err(e) => eprintln!("warning: could not kill process group {pgid}: {e}"),
    }
}

struct Group {
    pgid: Pid,
    leader: Child,
    // The placeholder runs for as long as this handle keeps its stdin open.
    _stdin: ChildStdin,
    killed: bool,
}

impl Group {
    fn new() -> Result<Self> {
        let mut leader = Command::new("cat")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn()
            .context("could not start the process-group placeholder")?;
        let stdin = leader.stdin.take().context("placeholder has no stdin")?;
        let pgid = Pid::from_raw(i32::try_from(leader.id())?);
        if let Ok(mut active) = ACTIVE.lock() {
            active.push(pgid);
        }
        Ok(Self {
            pgid,
            leader,
            _stdin: stdin,
            killed: false,
        })
    }

    // The group is killed once. On macOS a second `killpg` fails with EPERM
    // when every member left is an unreaped zombie.
    fn kill(&mut self) {
        if !self.killed {
            kill_group(self.pgid);
            self.killed = true;
        }
    }
}

impl Drop for Group {
    fn drop(&mut self) {
        self.kill();
        // The group leaves the registry before its leader is reaped. Reaping
        // frees the pgid for reuse, and the interrupt handler must never kill
        // a group that is no longer ours.
        if let Ok(mut active) = ACTIVE.lock() {
            active.retain(|p| *p != self.pgid);
        }
        let _ = self.leader.wait();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nix::sys::signal::kill;
    use std::fs;
    use std::path::Path;

    // A killed process stays a zombie until its new parent reaps it, and a
    // container whose init does not reap keeps it forever, so a zombie counts
    // as gone.
    fn is_gone(pid: i32) -> bool {
        for _ in 0..50 {
            if kill(Pid::from_raw(pid), None) == Err(Errno::ESRCH) {
                return true;
            }
            let state = Command::new("ps")
                .args(["-o", "stat=", "-p", &pid.to_string()])
                .output()
                .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_owned())
                .unwrap_or_default();
            if state.starts_with('Z') {
                return true;
            }
            sleep(Duration::from_millis(100));
        }
        false
    }

    fn background_sleep(dir: &Path, then: &str) -> Command {
        let mut cmd = Command::new("sh");
        cmd.arg("-c")
            .arg(format!(
                "sleep 60 & echo $! > '{}'; {then}",
                dir.join("pid").display()
            ))
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        cmd
    }

    fn read_pid(dir: &Path) -> i32 {
        fs::read_to_string(dir.join("pid"))
            .expect("the shell wrote its background pid before the deadline")
            .trim()
            .parse()
            .unwrap()
    }

    #[test]
    fn a_deadline_kills_the_grandchild_too() {
        let dir = tempfile::tempdir().unwrap();
        // The deadline is long enough for the shell to write the pid on a
        // loaded machine.
        let outcome = run(
            &mut background_sleep(dir.path(), "wait"),
            Duration::from_secs(3),
        );
        assert_eq!(outcome.unwrap(), Outcome::TimedOut);
        assert!(
            is_gone(read_pid(dir.path())),
            "the background sleep survived"
        );
    }

    #[test]
    fn a_normal_exit_still_kills_what_it_left_behind() {
        let dir = tempfile::tempdir().unwrap();
        let outcome = run(
            &mut background_sleep(dir.path(), "exit 3"),
            Duration::from_secs(30),
        );
        assert_eq!(outcome.unwrap(), Outcome::Exited(Some(3)));
        assert!(
            is_gone(read_pid(dir.path())),
            "the background sleep survived"
        );
    }
}
