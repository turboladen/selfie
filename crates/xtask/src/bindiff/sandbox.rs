//! Running the binary under test in a sandbox, and proving it stayed there.

use std::collections::VecDeque;
use std::env;
use std::ffi::OsString;
use std::fs::{self, File};
use std::io::Read;
use std::os::fd::OwnedFd;
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::LazyLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use anyhow::{Result, anyhow};
use nix::fcntl::{FcntlArg, OFlag, fcntl};
use nix::unistd::pipe;
use regex::Regex;

use super::fixture::{self, Fixture};
use crate::proc::{self, Outcome};

const DIRECTORIES: [&str; 3] = ["package_directory", "dotfiles_directory", "state_directory"];

/// Gives `cmd` the environment `just sandbox-run` gives the binary, with TERM
/// pinned so both binaries see the same terminal, TMPDIR inside `writable`,
/// and git discovery stopped at `writable`. It runs from `home`.
pub fn sandboxed<'a>(cmd: &'a mut Command, home: &Path, writable: &Path) -> &'a mut Command {
    cmd.env_clear()
        .env(
            "PATH",
            env::var_os("PATH").unwrap_or_else(|| "/usr/bin:/bin".into()),
        )
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("SELFIE_CONFIG_DIR", home.join(".config/selfie"))
        .env("SHELL", "/bin/sh")
        .env("TERM", "dumb")
        .env("TMPDIR", writable.join("tmp"))
        .env("GIT_CEILING_DIRECTORIES", writable)
        .current_dir(home)
}

/// What one run printed, and how it ended.
pub struct Captured {
    pub outcome: Outcome,
    pub stdout: String,
    pub stderr: String,
}

/// At most this much of each stream is kept.
pub const OUTPUT_CAP: usize = 4 << 20;

/// Runs `cmd` under `deadline`, with stdout and stderr captured separately
/// and each kept up to [`OUTPUT_CAP`]. A copy of each is written beside
/// `stem`.
///
/// # Errors
///
/// Fails when the command cannot be spawned or its output cannot be read.
pub fn capture(cmd: &mut Command, stem: &Path, deadline: Duration) -> Result<Captured> {
    // Pipes rather than files: a process that leaves the run's process group
    // outlives the run, and a file it holds open would grow without bound
    // outside the sandbox. Once the pipes close, its next write fails.
    let (out_read, out_write) = pipe()?;
    let (err_read, err_write) = pipe()?;
    let stop = Arc::new(AtomicBool::new(false));
    let out = drain(out_read, Arc::clone(&stop))?;
    let err = drain(err_read, Arc::clone(&stop))?;
    let outcome = proc::run(
        cmd.stdout(Stdio::from(out_write))
            .stderr(Stdio::from(err_write)),
        deadline,
    );
    // Replacing the handles closes this process's copies of the write ends.
    cmd.stdout(Stdio::null()).stderr(Stdio::null());
    stop.store(true, Ordering::SeqCst);
    let stdout = out
        .join()
        .map_err(|_| anyhow!("the stdout reader panicked"))?;
    let stderr = err
        .join()
        .map_err(|_| anyhow!("the stderr reader panicked"))?;
    let outcome = outcome?;
    fs::write(stem.with_extension("stdout"), &stdout)?;
    fs::write(stem.with_extension("stderr"), &stderr)?;
    Ok(Captured {
        outcome,
        stdout,
        stderr,
    })
}

// Reads `fd` until end of file, or until `stop` is set and nothing more is
// waiting, keeping at most OUTPUT_CAP bytes and counting the rest. The read
// end is non-blocking, so a process still holding the write end cannot keep
// this thread waiting after the run ends.
fn drain(fd: OwnedFd, stop: Arc<AtomicBool>) -> Result<thread::JoinHandle<String>> {
    let flags = OFlag::from_bits_truncate(fcntl(&fd, FcntlArg::F_GETFL)?);
    fcntl(&fd, FcntlArg::F_SETFL(flags | OFlag::O_NONBLOCK))?;
    Ok(thread::spawn(move || {
        let mut file = File::from(fd);
        let mut kept = Vec::new();
        let mut dropped: u64 = 0;
        let mut buf = [0u8; 64 * 1024];
        loop {
            match file.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    let room = OUTPUT_CAP.saturating_sub(kept.len()).min(n);
                    kept.extend_from_slice(&buf[..room]);
                    dropped += (n - room) as u64;
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    if stop.load(Ordering::SeqCst) {
                        break;
                    }
                    thread::sleep(Duration::from_millis(10));
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(_) => break,
            }
        }
        let mut text = String::from_utf8_lossy(&kept).into_owned();
        if dropped > 0 {
            text.push_str(&format!("\n<{dropped} more bytes not kept>\n"));
        }
        text
    }))
}

/// Resolves an absolute `path` the way the kernel does: one component at a
/// time, following each symlink it meets before applying the next `..`.
/// Components that do not exist yet are taken as written. Returns `None` for
/// a relative path or a symlink loop.
pub fn resolve(path: &Path) -> Option<PathBuf> {
    if !path.is_absolute() {
        return None;
    }
    let mut pending: VecDeque<OsString> = VecDeque::new();
    push_components(&mut pending, path, false);
    let mut out = PathBuf::from("/");
    let mut follows = 0;
    while let Some(part) = pending.pop_front() {
        if part == ".." {
            out.pop();
            continue;
        }
        let next = out.join(&part);
        match fs::symlink_metadata(&next) {
            // A component that cannot be inspected might be a link out, so
            // the path does not resolve.
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => return None,
            Ok(meta) if meta.file_type().is_symlink() => {
                follows += 1;
                // The kernel gives up on a loop at about this depth.
                if follows > 40 {
                    return None;
                }
                let target = fs::read_link(&next).ok()?;
                if target.is_absolute() {
                    out = PathBuf::from("/");
                }
                push_components(&mut pending, &target, true);
            }
            _ => out = next,
        }
    }
    Some(out)
}

fn push_components(pending: &mut VecDeque<OsString>, path: &Path, front: bool) {
    let parts: Vec<OsString> = path
        .components()
        .filter_map(|c| match c {
            Component::Normal(s) => Some(s.to_owned()),
            Component::ParentDir => Some("..".into()),
            _ => None,
        })
        .collect();
    if front {
        for part in parts.into_iter().rev() {
            pending.push_front(part);
        }
    } else {
        pending.extend(parts);
    }
}

/// Why a symlink the fixture laid down leads out of `home`, if one does.
/// Chains are followed, so a link into the sandbox that reaches another link
/// out of it is caught.
pub fn escaping_link(home: &Path, fixture: &Fixture) -> Option<String> {
    fixture.symlink.iter().find_map(|l| {
        let target = resolve(&home.join(&l.path));
        match target {
            Some(t) if t.starts_with(home) => None,
            Some(t) => Some(format!("the symlink {} leads to {}", l.path, t.display())),
            None => Some(format!("the symlink {} does not resolve", l.path)),
        }
    })
}

/// How the gate ended.
pub enum Gate {
    Passed,
    Refused(String),
    TimedOut,
}

/// Runs `config validate` through `cmd`, which executes nothing, and
/// requires it to succeed and each directory selfie resolved to lie inside
/// `home`. A binary that read any other config, or a config that points out
/// of the sandbox or does not load, fails here before a fixture's first run.
///
/// # Errors
///
/// Fails when the binary cannot be run.
pub fn gate(cmd: &mut Command, home: &Path, logs: &Path, fixture: &Fixture) -> Result<Gate> {
    let captured = capture(
        cmd.args(["--no-color", "config", "validate"]),
        &logs.join("gate"),
        Duration::from_secs(fixture.timeout_secs),
    )?;
    match captured.outcome {
        Outcome::TimedOut => return Ok(Gate::TimedOut),
        outcome if !outcome.success() => {
            return Ok(Gate::Refused(format!(
                "`config validate` failed ({outcome}), so the config does not load"
            )));
        }
        _ => {}
    }
    Ok(
        match directories_inside(&captured.stdout, home, &fixture.config()) {
            Ok(()) => Gate::Passed,
            Err(why) => Gate::Refused(why),
        },
    )
}

// Each directory `config validate` printed must appear exactly once and
// resolve inside `home`.
fn directories_inside(stdout: &str, home: &Path, config: &str) -> Result<(), String> {
    let printed = |name: &str| -> Vec<PathBuf> {
        let prefix = format!("{name}: ");
        stdout
            .lines()
            .filter_map(|l| l.trim().strip_prefix(prefix.as_str()))
            .map(PathBuf::from)
            .collect()
    };
    let inside = |dir: &Path| resolve(dir).is_some_and(|d| d.starts_with(home));
    for name in DIRECTORIES {
        match printed(name).as_slice() {
            [dir] if inside(dir) => {}
            [dir] => {
                return Err(format!(
                    "the binary resolved {name} to {}, outside the sandbox",
                    dir.display()
                ));
            }
            // An older binary prints fewer directories. One it did not print
            // is acceptable only when the config leaves it at its default,
            // and that default lies inside. The dotfiles default sits beside
            // the package directory, not under HOME.
            [] if name == "dotfiles_directory" && !fixture::sets(config, name) => {
                let beside = printed("package_directory")
                    .first()
                    .and_then(|p| p.parent().map(|parent| parent.join("dotfiles")));
                if !beside.is_some_and(|d| inside(&d)) {
                    return Err(
                        "`config validate` printed no dotfiles_directory, and its default \
                         beside the package directory is not inside the sandbox"
                            .into(),
                    );
                }
            }
            [] if !fixture::sets(config, name) => {}
            [] => {
                return Err(format!(
                    "`config validate` printed no {name}, which the config sets"
                ));
            }
            // A value can carry newlines, so a second line may be forged.
            many => {
                return Err(format!(
                    "`config validate` printed {name} {} times; only one can be selfie's",
                    many.len()
                ));
            }
        }
    }
    Ok(())
}

static TIMESTAMP: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(?:\.\d+)?(?:Z|[+-]\d{2}:\d{2})")
        .expect("the pattern is a literal")
});

/// Replaces what differs between two runs of the same binary: the sandbox
/// HOME and the directory around it, which holds TMPDIR, each in both
/// spellings macOS produces, and RFC 3339 timestamps.
pub fn normalize(text: &str, home: &Path, boxed: &Path) -> String {
    let mut out = text.to_owned();
    // HOME lies inside the box, so it is replaced first.
    for (path, token) in [(home, "<HOME>"), (boxed, "<BOX>")] {
        let canonical = path.display().to_string();
        out = out.replace(&canonical, token);
        if let Some(alias) = canonical.strip_prefix("/private") {
            out = out.replace(alias, token);
        }
    }
    TIMESTAMP.replace_all(&out, "<TIME>").into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    fn scratch() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = fs::canonicalize(dir.path()).unwrap();
        (dir, path)
    }

    #[test]
    fn a_link_is_followed_before_the_parent_step_after_it() {
        let (_h, home) = scratch();
        let (_o, outside) = scratch();
        fs::create_dir(outside.join("a")).unwrap();
        symlink(outside.join("a"), home.join("link")).unwrap();
        // Lexically `home/link/..` is `home`; the kernel says `outside`.
        let resolved = resolve(&home.join("link").join("..").join("state")).unwrap();
        assert_eq!(resolved, outside.join("state"));
    }

    #[test]
    fn climbing_and_links_out_are_seen_and_paths_inside_are_kept() {
        let (_h, home) = scratch();
        let (_o, outside) = scratch();
        symlink(&outside, home.join("pkgs")).unwrap();
        let through = resolve(&home.join("pkgs").join("more")).unwrap();
        assert!(!through.starts_with(&home), "{}", through.display());
        let climbing = resolve(&home.join("a").join("..").join("..").join("x")).unwrap();
        assert!(!climbing.starts_with(&home), "{}", climbing.display());
        let inside = resolve(&home.join("state").join("selfie")).unwrap();
        assert_eq!(inside, home.join("state").join("selfie"));
        assert_eq!(resolve(Path::new("relative/dir")), None);
    }

    #[test]
    fn a_link_loop_does_not_resolve() {
        let (_h, home) = scratch();
        symlink(home.join("b"), home.join("a")).unwrap();
        symlink(home.join("a"), home.join("b")).unwrap();
        assert_eq!(resolve(&home.join("a")), None);
    }

    #[test]
    fn a_chain_of_links_that_ends_outside_is_an_escape() {
        let (_h, home) = scratch();
        let (_o, outside) = scratch();
        let f = Fixture::parse(&format!(
            "[[symlink]]\npath = \"first\"\nto = \"@HOME@/second\"\n\
             [[symlink]]\npath = \"second\"\nto = \"{}\"\n\
             [[symlink]]\npath = \"fine\"\nto = \"@HOME@/nowhere\"\n[[run]]\nargs = []\n",
            outside.display()
        ))
        .unwrap();
        let sandbox = home.join("s");
        fixture::lay_out(&sandbox, &f, None).unwrap();
        // `first` leads out only through `second`, so naming it proves the
        // chain was followed.
        let why = escaping_link(&sandbox, &f).unwrap();
        assert!(why.contains("symlink first"), "{why}");
    }

    #[test]
    fn the_sandbox_path_and_timestamps_are_normalized() {
        let home = Path::new("/private/tmp/x/box/home");
        let boxed = Path::new("/private/tmp/x/box");
        let text = "a /private/tmp/x/box/home/p, /tmp/x/box/home/q and /tmp/x/box/tmp/r at \
                    2026-09-27T22:19:01.463718+00:00";
        assert_eq!(
            normalize(text, home, boxed),
            "a <HOME>/p, <HOME>/q and <BOX>/tmp/r at <TIME>"
        );
    }

    #[test]
    fn the_gate_wants_each_directory_once_and_inside() {
        let (_h, home) = scratch();
        let line = |name: &str, dir: &Path| format!("  {name}: {}\n", dir.display());
        let all =
            |dir: &Path| -> String { DIRECTORIES.iter().map(|n| line(n, &dir.join(n))).collect() };
        assert_eq!(directories_inside(&all(&home), &home, ""), Ok(()));
        let outside = Path::new("/private/var/elsewhere");
        let escaped = format!("{}{}", all(&home), line("state_directory", outside))
            .replace(&line("state_directory", &home.join("state_directory")), "");
        assert!(directories_inside(&escaped, &home, "").is_err());
        // A value carrying newlines forges a line ahead of the real one.
        let forged = format!(
            "  environment: e\n{}{}",
            all(&home),
            line("state_directory", outside)
        );
        let why = directories_inside(&forged, &home, "").unwrap_err();
        assert!(why.contains("2 times"), "{why}");
    }

    #[test]
    fn a_directory_an_older_binary_omits_passes_only_at_an_inside_default() {
        let (_h, home) = scratch();
        let package = format!("  package_directory: {}\n", home.join("packages").display());
        assert_eq!(directories_inside(&package, &home, ""), Ok(()));
        let set = "state_directory: /x\n";
        assert!(directories_inside(&package, &home, set).is_err());
        // With the package directory at HOME itself, the dotfiles default
        // would sit beside HOME.
        let at_home = format!("  package_directory: {}\n", home.display());
        assert!(directories_inside(&at_home, &home, "").is_err());
    }
}
