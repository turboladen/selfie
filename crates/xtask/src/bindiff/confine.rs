//! Confining the binary under test with macOS `sandbox-exec`, so a fixture
//! cannot make it write outside its sandbox or read the real home directory.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use nix::unistd::{Uid, User};

const SANDBOX_EXEC: &str = "/usr/bin/sandbox-exec";

/// Fails unless this host can confine a process with `sandbox-exec`.
///
/// # Errors
///
/// Fails on any host other than macOS, and on a macOS without
/// `sandbox-exec`.
pub fn available() -> Result<()> {
    if !cfg!(target_os = "macos") || !Path::new(SANDBOX_EXEC).is_file() {
        bail!(
            "bindiff confines selfie with macOS {SANDBOX_EXEC}, which this host lacks; \
             it refuses to run selfie unconfined"
        );
    }
    Ok(())
}

/// The home directories the confined binary may not read: `$HOME`, and the
/// password database's entry for this user when it differs. Each is
/// canonical.
///
/// # Errors
///
/// Fails when neither can be found, since the profile would then deny no
/// reads at all.
pub fn real_homes() -> Result<Vec<PathBuf>> {
    let from_env = env::var_os("HOME").map(PathBuf::from);
    let from_passwd = User::from_uid(Uid::current()).ok().flatten().map(|u| u.dir);
    let mut homes: Vec<PathBuf> = [from_env, from_passwd]
        .into_iter()
        .flatten()
        .filter(|p| p.is_absolute())
        .filter_map(|p| fs::canonicalize(p).ok())
        .collect();
    homes.dedup();
    if homes.is_empty() {
        bail!("no home directory could be found, so the binary cannot be kept from reading it");
    }
    Ok(homes)
}

/// What a confined process may touch.
pub struct Confinement<'a> {
    /// The only directory it may write, which holds its HOME and TMPDIR.
    pub writable: &'a Path,
    /// Directories it may not read, except inside `readable`.
    pub homes: &'a [PathBuf],
    /// The work directory, readable even when it lies inside a home.
    pub readable: &'a Path,
    /// Directories it may not read at all.
    pub unreadable: &'a [PathBuf],
}

// sandbox-exec matches the real path a process touches, so a rule written
// with an alias such as `/tmp` for `/private/tmp` matches nothing: a write
// rule would deny everything and a read rule would deny nothing. Every path
// is canonicalized, and one the profile language cannot quote is refused.
fn quoted(path: &Path) -> Result<String> {
    let canonical = fs::canonicalize(path)
        .with_context(|| format!("{} must exist to be named in a profile", path.display()))?;
    let text = canonical
        .to_str()
        .with_context(|| format!("{} is not UTF-8", canonical.display()))?;
    if text.contains('"') || text.contains('\\') {
        bail!("{text} cannot be quoted in a sandbox profile");
    }
    Ok(format!("\"{text}\""))
}

impl Confinement<'_> {
    /// Renders the profile.
    ///
    /// # Errors
    ///
    /// Fails when a path does not exist, or cannot be quoted.
    pub fn profile(&self) -> Result<String> {
        if self.homes.is_empty() {
            bail!("no home directory is known, so reads of it cannot be denied");
        }
        let mut text = String::from("(version 1)\n(allow default)\n");
        text.push_str(&format!(
            "(deny file-write*\n  (require-not (require-any\n    (subpath {})\n    \
             (literal \"/dev/null\") (literal \"/dev/dtracehelper\")\n    \
             (literal \"/dev/stdout\") (literal \"/dev/stderr\") (regex #\"^/dev/fd/\"))))\n",
            quoted(self.writable)?
        ));
        // A terminal would let a command queue input that the user's shell
        // runs unconfined after bindiff exits.
        text.push_str(
            "(deny file-read* file-write* file-ioctl (regex #\"^/dev/(tty|pty|console)\"))\n",
        );
        // A socket reaches a service that acts with the user's full
        // authority, and a signal reaches the user's other processes.
        text.push_str("(deny network*)\n(deny signal)\n(allow signal (target same-sandbox))\n");
        // A Mach service, LaunchServices or an Apple event runs code outside
        // the sandbox on the process's behalf, and another process's command
        // line can carry a secret into a transcript.
        text.push_str(
            "(deny mach-lookup)\n(deny lsopen)\n(deny appleevent-send)\n\
             (deny process-info* (target others))\n",
        );
        {
            let homes = self
                .homes
                .iter()
                .map(|h| Ok(format!("(subpath {})", quoted(h)?)))
                .collect::<Result<Vec<_>>>()?
                .join(" ");
            text.push_str(&format!(
                "(deny file-read*\n  (require-all\n    (require-any {homes})\n    \
                 (require-not (subpath {}))))\n",
                quoted(self.readable)?
            ));
        }
        // A separate rule, so the work-directory exception above cannot
        // reopen a directory named here.
        for path in self.unreadable {
            text.push_str(&format!("(deny file-read* (subpath {}))\n", quoted(path)?));
        }
        Ok(text)
    }

    /// Writes the profile to `path`, which must lie outside `writable` so
    /// the confined process cannot edit it, and returns a command running
    /// `bin` under it.
    ///
    /// # Errors
    ///
    /// Fails when the profile cannot be rendered or written.
    pub fn command(&self, bin: &Path, path: &Path) -> Result<Command> {
        if fs::canonicalize(path.parent().unwrap_or(path))?
            .starts_with(fs::canonicalize(self.writable)?)
        {
            bail!(
                "the profile {} would be writable by the process it confines",
                path.display()
            );
        }
        fs::write(path, self.profile()?)?;
        let mut cmd = Command::new(SANDBOX_EXEC);
        cmd.arg("-f").arg(path).arg(bin);
        Ok(cmd)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_profile_without_a_home_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let confinement = Confinement {
            writable: dir.path(),
            homes: &[],
            readable: dir.path(),
            unreadable: &[],
        };
        assert!(confinement.profile().is_err());
    }

    #[test]
    fn a_profile_names_only_canonical_paths() {
        let homes = [std::env::temp_dir()];
        // `/tmp` is an alias on macOS; the rule must name the real path.
        let dir = tempfile::Builder::new().tempdir_in("/tmp").unwrap();
        let alias = Path::new("/tmp").join(dir.path().file_name().unwrap());
        let canonical = fs::canonicalize(&alias).unwrap();
        let confinement = Confinement {
            writable: &alias,
            homes: &homes,
            readable: &alias,
            unreadable: &[],
        };
        let profile = confinement.profile().unwrap();
        assert!(
            profile.contains(&format!("(subpath \"{}\")", canonical.display())),
            "{profile}"
        );
        if canonical != alias {
            assert!(
                !profile.contains(&format!("\"{}\"", alias.display())),
                "{profile}"
            );
        }
    }

    #[test]
    fn a_path_that_cannot_be_canonicalized_or_quoted_is_refused() {
        let homes = [std::env::temp_dir()];
        let missing = Path::new("/nonexistent/xtask/sandbox");
        let confinement = Confinement {
            writable: missing,
            homes: &homes,
            readable: missing,
            unreadable: &[],
        };
        assert!(confinement.profile().is_err());
        let dir = tempfile::tempdir().unwrap();
        let odd = dir.path().join("a\"b");
        fs::create_dir(&odd).unwrap();
        let confinement = Confinement {
            writable: &odd,
            homes: &homes,
            readable: &odd,
            unreadable: &[],
        };
        assert!(confinement.profile().is_err());
    }

    #[test]
    fn the_profile_cannot_sit_where_the_process_may_write() {
        let homes = [std::env::temp_dir()];
        let dir = tempfile::tempdir().unwrap();
        let confinement = Confinement {
            writable: dir.path(),
            homes: &homes,
            readable: dir.path(),
            unreadable: &[],
        };
        let inside = dir.path().join("p.sb");
        assert!(
            confinement
                .command(Path::new("/bin/true"), &inside)
                .is_err()
        );
    }
}

// These run the real sandbox-exec, which only macOS has.
#[cfg(all(test, target_os = "macos"))]
mod live {
    use super::*;

    fn quote(path: &Path) -> String {
        shlex::try_quote(path.to_str().unwrap())
            .unwrap()
            .into_owned()
    }

    // Runs `script` under `confinement` and returns its status.
    fn confined(confinement: &Confinement, profile: &Path, script: &str) -> bool {
        confinement
            .command(Path::new("/bin/sh"), profile)
            .unwrap()
            .args(["-c", script])
            .status()
            .unwrap()
            .success()
    }

    #[test]
    fn a_confined_child_writes_inside_and_is_denied_outside() {
        let root = tempfile::tempdir().unwrap();
        let inside = root.path().join("box");
        let outside = root.path().join("outside");
        let home = root.path().join("home");
        for dir in [&inside, &outside, &home] {
            fs::create_dir(dir).unwrap();
        }
        let confinement = Confinement {
            writable: &inside,
            homes: &[home],
            readable: root.path(),
            unreadable: &[],
        };
        let script = format!(
            "echo in > {}; sh -c {}; true",
            quote(&inside.join("a")),
            shlex::try_quote(&format!("echo out > {}", quote(&outside.join("b")))).unwrap()
        );
        assert!(confined(&confinement, &root.path().join("p.sb"), &script));
        assert!(inside.join("a").is_file(), "the write inside was denied");
        assert!(!outside.join("b").exists(), "a child wrote outside");
    }

    #[test]
    fn a_home_is_unreadable_except_inside_the_work_directory() {
        let root = tempfile::tempdir().unwrap();
        let home = fs::canonicalize(root.path()).unwrap().join("home");
        let work = home.join("work");
        let inside = work.join("box");
        fs::create_dir_all(&inside).unwrap();
        fs::write(home.join("secret"), "s").unwrap();
        fs::write(work.join("shared"), "w").unwrap();
        let confinement = Confinement {
            writable: &inside,
            homes: std::slice::from_ref(&home),
            readable: &work,
            unreadable: &[],
        };
        let profile = root.path().join("p.sb");
        let read = |path: &Path| confined(&confinement, &profile, &format!("cat {}", quote(path)));
        assert!(!read(&home.join("secret")), "a file in the home was read");
        assert!(
            read(&work.join("shared")),
            "the work directory was not readable"
        );
    }

    // Runs `script` confined and returns its stderr.
    fn stderr_of(script: &str) -> String {
        let root = tempfile::tempdir().unwrap();
        let inside = root.path().join("box");
        fs::create_dir(&inside).unwrap();
        let confinement = Confinement {
            writable: &inside,
            homes: std::slice::from_ref(&inside),
            readable: root.path(),
            unreadable: &[],
        };
        let out = confinement
            .command(Path::new("/bin/sh"), &root.path().join("p.sb"))
            .unwrap()
            .args(["-c", script])
            .current_dir(&inside)
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stderr).into_owned()
    }

    // Without a terminal the open fails too, but not with EPERM.
    #[test]
    fn a_confined_process_cannot_open_a_terminal() {
        let err = stderr_of("exec 3</dev/tty");
        assert!(err.contains("Operation not permitted"), "{err}");
    }

    #[test]
    fn a_confined_process_cannot_reach_a_unix_socket() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("s");
        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        listener.set_nonblocking(true).unwrap();
        let err = stderr_of(&format!("echo hi | nc -w 1 -U {}", quote(&path)));
        std::thread::sleep(std::time::Duration::from_millis(500));
        assert!(
            listener.accept().is_err(),
            "a confined process connected: {err}"
        );
    }

    // `ps` asks a Mach service too, so this also holds without the
    // process-info rule, which stays as a second barrier.
    #[test]
    fn a_confined_process_cannot_see_another_process() {
        let err = stderr_of(&format!("ps -o command= -p {}", std::process::id()));
        assert!(!err.is_empty(), "ps read another process's command line");
    }

    // A user name comes from opendirectoryd over Mach; without it `id`
    // falls back to the number.
    #[test]
    fn a_confined_process_cannot_reach_a_mach_service() {
        let err = stderr_of("id -un >&2");
        assert!(
            err.trim().chars().all(|c| c.is_ascii_digit()),
            "a directory service answered: {err}"
        );
    }

    #[test]
    fn a_confined_process_cannot_signal_another() {
        let root = tempfile::tempdir().unwrap();
        let inside = root.path().join("box");
        fs::create_dir(&inside).unwrap();
        let confinement = Confinement {
            writable: &inside,
            homes: &[root.path().join("box")],
            readable: root.path(),
            unreadable: &[],
        };
        let mut victim = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let script = format!("kill -0 {}", victim.id());
        let denied = !confined(&confinement, &root.path().join("p.sb"), &script);
        victim.kill().unwrap();
        victim.wait().unwrap();
        assert!(denied, "a confined process signaled one outside");
    }
}
