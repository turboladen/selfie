//! The fixture format, the refusals made before anything is laid down, and
//! laying a fixture down in a sandbox.

use std::ffi::OsStr;
use std::fs::{self, File, OpenOptions};
use std::io::{ErrorKind, Read, Write};
use std::os::fd::OwnedFd;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Component, Path, PathBuf};
use std::sync::LazyLock;

use anyhow::{Context, Result, bail};
use nix::errno::Errno;
use nix::fcntl::{AtFlags, OFlag, openat, readlinkat};
use nix::sys::stat::{FchmodatFlags, Mode, SFlag, fchmod, fchmodat, fstat, fstatat};
use regex::Regex;
use serde::Deserialize;

use crate::workdir;

/// This stands for the sandbox's path in file content, run arguments and
/// link targets.
pub const HOME_TOKEN: &str = "@HOME@";
const OBSERVE_CAP: u64 = 1 << 20;
const DEFAULT_CONFIG: &str = "environment: sandbox\npackage_directory: @HOME@/packages\n";
const CONFIG_DIR: &str = ".config/selfie";
const CONFIG_FILES: [&str; 2] = [".config/selfie/config.yaml", ".config/selfie/config.yml"];

/// These global flags would move a directory away from the one the config
/// sets, where the gate cannot see it.
const DIRECTORY_FLAGS: [&str; 3] = [
    "--package-directory",
    "--dotfiles-directory",
    "--state-directory",
];

/// One fixture file: what to lay down in the sandbox, what to run, and what
/// to look at afterwards.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Fixture {
    /// These paths are reported after every run, relative to the sandbox HOME.
    #[serde(default)]
    pub observe: Vec<String>,
    /// This many seconds are allowed for the gate and for each run.
    #[serde(default = "default_run_timeout")]
    pub timeout_secs: u64,
    #[serde(default)]
    pub dir: Vec<DirSpec>,
    #[serde(default)]
    pub file: Vec<FileSpec>,
    #[serde(default)]
    pub symlink: Vec<LinkSpec>,
    pub run: Vec<RunSpec>,
}

impl Default for Fixture {
    fn default() -> Self {
        Self {
            observe: Vec::new(),
            timeout_secs: default_run_timeout(),
            dir: Vec::new(),
            file: Vec::new(),
            symlink: Vec::new(),
            run: Vec::new(),
        }
    }
}

fn default_run_timeout() -> u64 {
    60
}

/// A directory to create, with an optional mode.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DirSpec {
    pub path: String,
    pub mode: Option<u32>,
}

/// A file to create, with its content and an optional mode.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileSpec {
    pub path: String,
    #[serde(default)]
    pub content: String,
    pub mode: Option<u32>,
}

/// A symlink to create. Its target must resolve inside the sandbox.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LinkSpec {
    pub path: String,
    pub to: String,
}

/// One invocation of the binary under test.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunSpec {
    pub args: Vec<String>,
}

impl Fixture {
    /// Parses a fixture and checks everything that can be checked before it
    /// is laid down: its paths, its runs, its timeout.
    ///
    /// # Errors
    ///
    /// Fails when the text is not a valid fixture.
    pub fn parse(text: &str) -> Result<Self> {
        let fixture: Fixture = toml::from_str(text)?;
        for path in fixture
            .dir
            .iter()
            .map(|d| &d.path)
            .chain(fixture.file.iter().map(|f| &f.path))
            .chain(fixture.symlink.iter().map(|l| &l.path))
            .chain(&fixture.observe)
        {
            relative(path)?;
        }
        if fixture.run.is_empty() {
            bail!("a fixture needs at least one [[run]]");
        }
        if fixture.timeout_secs == 0 {
            bail!("timeout_secs must be at least 1");
        }
        Ok(fixture)
    }

    /// The text of the config file the sandbox will hold: the fixture's own
    /// `config.yaml` and `config.yml`, or the default.
    pub fn config(&self) -> String {
        let named: Vec<&str> = self
            .file
            .iter()
            .filter(|f| CONFIG_FILES.contains(&f.path.as_str()))
            .map(|f| f.content.as_str())
            .collect();
        // Both spellings are joined, since which one selfie reads is not
        // this harness's to decide.
        if named.is_empty() {
            DEFAULT_CONFIG.into()
        } else {
            named.join("\n")
        }
    }

    /// Why the fixture may not run, if it may not. These are cheap early
    /// refusals; the binary under test is confined by the OS whether or not
    /// they catch anything.
    ///
    /// A run may not override a directory, because the gate reads only the
    /// config, and may not name a path that leaves `@HOME@`. A config file
    /// may not name such a path, and neither may a spec's `target:` or
    /// `source:`. Other content, such as a dotfile's body or a shell command,
    /// is not read as a path.
    pub fn refusal(&self) -> Option<String> {
        for arg in self.run.iter().flat_map(|r| &r.args) {
            if overrides_directory(arg) {
                return Some(format!(
                    "the run argument {arg:?} overrides a directory; set directories in the \
                     fixture's config"
                ));
            }
            if arg.split('=').any(escapes_home) {
                return Some(format!(
                    "the run argument {arg:?} names a path that leaves {HOME_TOKEN}"
                ));
            }
        }
        for f in &self.file {
            let found = if is_config(&f.path) {
                tokens(&f.content).find(|t| escapes_home(t))
            } else if is_yaml(&f.path) {
                spec_paths(&f.content).into_iter().find(|t| escapes_home(t))
            } else {
                None
            };
            if let Some(path) = found {
                return Some(format!(
                    "{} names {path}, which leaves {HOME_TOKEN}",
                    f.path
                ));
            }
        }
        None
    }
}

fn is_config(path: &str) -> bool {
    Path::new(path).parent() == Some(Path::new(CONFIG_DIR))
}

fn is_yaml(path: &str) -> bool {
    Path::new(path)
        .extension()
        .is_some_and(|e| e == "yaml" || e == "yml")
}

/// Whether `config` might set `key`: whether the key's name, or any escape
/// that could spell it, appears anywhere in its text. A YAML key can be
/// spelled many ways (quoted, escaped, in a flow mapping, through a merge),
/// so any mention counts.
pub fn sets(config: &str, key: &str) -> bool {
    // A double-quoted YAML key can spell its name with escapes, so any
    // backslash counts as well.
    config.contains(key) || config.contains('\\')
}

static SPEC_PATH: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?m)^\s*(?:-\s*)?(?:target|source)\s*:\s*["']?([^"'\s#]+)"#)
        .expect("the pattern is a literal")
});

// A long directory flag, alone or with `=value`, or a cluster of short flags
// containing `-p`, which is `--package-directory`.
fn overrides_directory(arg: &str) -> bool {
    let long = DIRECTORY_FLAGS
        .iter()
        .any(|f| arg == *f || arg.starts_with(&format!("{f}=")));
    let short =
        arg.len() > 1 && arg.starts_with('-') && !arg.starts_with("--") && arg[1..].contains('p');
    long || short
}

fn tokens(text: &str) -> impl Iterator<Item = &str> {
    text.split(|c: char| c.is_whitespace() || "\"'[]{},=".contains(c))
        .filter(|t| !t.is_empty())
}

// The value of every `target:` and `source:` line, quotes removed.
fn spec_paths(text: &str) -> Vec<&str> {
    SPEC_PATH
        .captures_iter(text)
        .filter_map(|c| c.get(1).map(|m| m.as_str()))
        .collect()
}

// A path leaves HOME when it is absolute, when it names another user's home
// with `~user`, when `@HOME@` runs straight into more text, or when it
// climbs with `..` anywhere.
fn escapes_home(t: &str) -> bool {
    let at_home = t == HOME_TOKEN || t.starts_with(&format!("{HOME_TOKEN}/"));
    let tilde = t == "~" || t.starts_with("~/");
    t.starts_with('/')
        || (t.starts_with(HOME_TOKEN) && !at_home)
        || (t.starts_with('~') && !tilde)
        || Path::new(t).components().any(|c| c == Component::ParentDir)
}

/// Returns `path` if it stays inside the sandbox: relative, with no `.` or
/// `..`.
///
/// # Errors
///
/// Fails on any other path.
pub fn relative(path: &str) -> Result<&Path> {
    workdir::plain_relative(path).with_context(|| {
        format!("fixture path {path:?} must be relative, without `.` or `..` components")
    })
}

/// Replaces `@HOME@` with the sandbox's path.
pub fn with_home(text: &str, home: &Path) -> String {
    text.replace(HOME_TOKEN, &home.display().to_string())
}

// Creates each missing directory from `home` down to `rel`, refusing to pass
// through a symlink or a file. `create_dir_all` would follow a link a fixture
// made and create directories wherever it points.
fn ensure_dir(home: &Path, rel: &Path) -> Result<PathBuf> {
    let mut current = home.to_path_buf();
    for component in rel.components() {
        current.push(component);
        match fs::symlink_metadata(&current) {
            Ok(meta) if meta.file_type().is_symlink() => {
                bail!(
                    "{} is a symlink; refusing to write through it",
                    current.display()
                )
            }
            Ok(meta) if meta.is_dir() => {}
            Ok(_) => bail!("{} is not a directory", current.display()),
            Err(e) if e.kind() == ErrorKind::NotFound => fs::create_dir(&current)?,
            Err(e) => return Err(e.into()),
        }
    }
    Ok(current)
}

/// Lays the fixture down in `home`, which must not exist yet, with `extra`
/// written as one more file.
///
/// # Errors
///
/// Fails when a write would pass through a symlink, a path already exists,
/// or the file system refuses.
pub fn lay_out(home: &Path, fixture: &Fixture, extra: Option<&FileSpec>) -> Result<()> {
    // Directories and files come before any symlink, and every write refuses
    // to pass through a link, so nothing lands outside the sandbox. Directory
    // modes come last, deepest first, so a directory that loses its own
    // permissions can still be filled and its children still reached.
    fs::create_dir(home)?;
    let default_config = FileSpec {
        path: format!("{CONFIG_DIR}/config.yaml"),
        content: DEFAULT_CONFIG.into(),
        mode: None,
    };
    let mut files: Vec<&FileSpec> = fixture.file.iter().chain(extra).collect();
    if !fixture.file.iter().any(|f| is_config(&f.path)) {
        files.push(&default_config);
        ensure_dir(home, Path::new("packages"))?;
    }
    for d in &fixture.dir {
        ensure_dir(home, relative(&d.path)?)?;
    }
    for f in files {
        let rel = relative(&f.path)?;
        let parent = ensure_dir(home, rel.parent().unwrap_or(Path::new("")))?;
        let path = parent.join(rel.file_name().context("a file path needs a name")?);
        // `create_new` refuses an existing path, a symlink included.
        let mut out = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .with_context(|| format!("could not create {}", path.display()))?;
        out.write_all(with_home(&f.content, home).as_bytes())?;
        if let Some(mode) = f.mode {
            fs::set_permissions(&path, fs::Permissions::from_mode(mode))?;
        }
    }
    for l in &fixture.symlink {
        let rel = relative(&l.path)?;
        let parent = ensure_dir(home, rel.parent().unwrap_or(Path::new("")))?;
        let name = rel.file_name().context("a link path needs a name")?;
        symlink(with_home(&l.to, home), parent.join(name))?;
    }
    let mut modes: Vec<(&str, u32)> = fixture
        .dir
        .iter()
        .filter_map(|d| d.mode.map(|m| (d.path.as_str(), m)))
        .collect();
    modes.sort_by_key(|(path, _)| std::cmp::Reverse(Path::new(path).components().count()));
    for (path, mode) in modes {
        fs::set_permissions(home.join(path), fs::Permissions::from_mode(mode))?;
    }
    Ok(())
}

/// Reports what is at `rel` without following any symlink on the way. Only a
/// regular file is read, since a fifo or socket would block the read.
pub fn observe(home: &Path, rel: &str) -> String {
    // Every step goes through a directory descriptor with O_NOFOLLOW, so a
    // process the binary left running cannot swap a checked component for a
    // symlink before it is used. The harness is not confined, and a path
    // check followed by an ordinary open would read wherever that link led.
    let parts: Vec<&OsStr> = Path::new(rel).iter().collect();
    let mut dir = match open(None, home.as_os_str(), OFlag::O_DIRECTORY) {
        Ok(fd) => fd,
        Err(e) => return format!("unreadable ({e})"),
    };
    let mut walked = home.to_path_buf();
    for (i, name) in parts.iter().enumerate() {
        walked.push(name);
        let last = i + 1 == parts.len();
        let stat = match fstatat(&dir, *name, AtFlags::AT_SYMLINK_NOFOLLOW) {
            Ok(stat) => stat,
            Err(Errno::ENOENT) => return "absent".into(),
            Err(e) => return format!("unreadable ({e})"),
        };
        let mode = u32::from(stat.st_mode) & 0o7777;
        match SFlag::from_bits_truncate(stat.st_mode) & SFlag::S_IFMT {
            SFlag::S_IFLNK if last => {
                return match readlinkat(&dir, *name) {
                    Ok(to) => format!("symlink -> {}", Path::new(&to).display()),
                    Err(e) => format!("symlink, unreadable ({e})"),
                };
            }
            SFlag::S_IFLNK => return format!("under symlink {}", walked.display()),
            SFlag::S_IFDIR if last => return format!("dir mode={mode:o}"),
            SFlag::S_IFDIR => match open(Some(&dir), name, OFlag::O_DIRECTORY) {
                Ok(fd) => dir = fd,
                Err(e) => return format!("unreadable ({e})"),
            },
            SFlag::S_IFREG if last => {
                return format!(
                    "file mode={mode:o} content={}",
                    read_owned(&dir, name, mode)
                );
            }
            _ if last => return format!("special file mode={mode:o}"),
            _ => return "absent".into(),
        }
    }
    "absent".into()
}

fn open(dir: Option<&OwnedFd>, name: &OsStr, extra: OFlag) -> nix::Result<OwnedFd> {
    let flags = OFlag::O_RDONLY | OFlag::O_NOFOLLOW | OFlag::O_NONBLOCK | OFlag::O_CLOEXEC | extra;
    match dir {
        Some(dir) => openat(dir, name, flags, Mode::empty()),
        None => nix::fcntl::open(name, flags, Mode::empty()),
    }
}

// Reads a regular file the sandbox owns, lending it owner-read for the read
// if it lacks it and restoring its mode after. A failed restore is reported,
// since the next run would otherwise see a mode the binary did not set.
fn read_owned(dir: &OwnedFd, name: &OsStr, mode: u32) -> String {
    let lend = mode & 0o400 == 0;
    // Lending acts on the entry itself, never a link's target.
    if lend {
        let lent = Mode::from_bits_truncate((mode | 0o400) as nix::sys::stat::mode_t);
        let _ = fchmodat(dir, name, lent, FchmodatFlags::NoFollowSymlink);
    }
    let restore = |fd: Option<&File>| {
        let back = Mode::from_bits_truncate(mode as nix::sys::stat::mode_t);
        match fd {
            Some(fd) => fchmod(fd, back),
            None => fchmodat(dir, name, back, FchmodatFlags::NoFollowSymlink),
        }
    };
    let fd = match open(Some(dir), name, OFlag::empty()) {
        Ok(fd) => fd,
        Err(e) => {
            if lend {
                let _ = restore(None);
            }
            return format!("<unreadable: {e}>");
        }
    };
    let regular = fstat(&fd)
        .is_ok_and(|s| SFlag::from_bits_truncate(s.st_mode) & SFlag::S_IFMT == SFlag::S_IFREG);
    let mut file = File::from(fd);
    let content = if regular {
        // The binary chose this file's size, so only a bounded prefix is
        // read.
        let mut bytes = Vec::new();
        match (&mut file).take(OBSERVE_CAP).read_to_end(&mut bytes) {
            Ok(_) => {
                let size = fstat(&file).map_or(0, |s| s.st_size);
                let shown = format!("{:?}", String::from_utf8_lossy(&bytes));
                if u64::try_from(size).unwrap_or(0) > OBSERVE_CAP {
                    format!("{shown} (first {OBSERVE_CAP} of {size} bytes)")
                } else {
                    shown
                }
            }
            Err(e) => format!("<unreadable: {}>", e.kind()),
        }
    } else {
        "<changed while observed>".into()
    };
    if lend && restore(Some(&file)).is_err() {
        return format!("{content} (its mode could not be restored)");
    }
    content
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn fixture(text: &str) -> Fixture {
        Fixture::parse(text).unwrap()
    }

    #[test]
    fn a_fixture_path_must_stay_inside_the_sandbox() {
        assert!(relative("a/b").is_ok());
        for bad in ["", "/etc/passwd", "a/../../b", "../b", "./a"] {
            assert!(relative(bad).is_err(), "{bad:?} was accepted");
        }
        assert!(Fixture::parse("observe = [\"../x\"]\n[[run]]\nargs = []\n").is_err());
    }

    #[test]
    fn a_zero_timeout_or_no_run_is_refused() {
        assert!(Fixture::parse("timeout_secs = 0\n[[run]]\nargs = []\n").is_err());
        assert!(Fixture::parse("observe = []\n").is_err());
    }

    #[test]
    fn a_run_may_not_override_a_directory() {
        for bad in [
            "--package-directory",
            "--state-directory=/x",
            "--dotfiles-directory",
            "-p",
            "-p/abs",
            "-vp",
        ] {
            let f = fixture(&format!("[[run]]\nargs = [{bad:?}, \"x\"]\n"));
            assert!(f.refusal().is_some(), "{bad:?} was accepted");
        }
        let fine =
            fixture("[[run]]\nargs = [\"--no-color\", \"-e\", \"sandbox\", \"apply\", \"-y\"]\n");
        assert_eq!(fine.refusal(), None);
    }

    #[test]
    fn a_config_path_that_leaves_home_is_refused() {
        for bad in [
            "state_directory: /srv/state",
            "state_directory: \"@HOME@/../x\"",
            "state_directory: @HOME@../x",
            "state_directory: ~other/x",
            "state_directory: ../x",
        ] {
            let f = fixture(&format!(
                "[[file]]\npath = \".config/selfie/config.yaml\"\ncontent = {bad:?}\n\
                 [[run]]\nargs = []\n"
            ));
            assert!(f.refusal().is_some(), "{bad:?} was accepted");
        }
        let fine = fixture(
            "[[file]]\npath = \".config/selfie/config.yaml\"\n\
             content = \"package_directory: @HOME@/p\\nstate_directory: '~/s'\\n\"\n\
             [[run]]\nargs = []\n",
        );
        assert_eq!(fine.refusal(), None);
    }

    #[test]
    fn only_a_spec_target_or_source_is_read_as_a_path() {
        let escaping = fixture(
            "[[file]]\npath = \"packages/p.yaml\"\n\
             content = \"dotfiles:\\n  - source: p/rc\\n    target: /Users/me/.zshrc\\n\"\n\
             [[run]]\nargs = []\n",
        );
        assert!(escaping.refusal().is_some());
        let climbing = fixture(
            "[[file]]\npath = \"packages/p.yaml\"\ncontent = \"- source: ../../secret\\n\"\n\
             [[run]]\nargs = []\n",
        );
        assert!(climbing.refusal().is_some());
        let ordinary = fixture(
            "[[file]]\npath = \"packages/p/rc\"\ncontent = \"export PATH=/usr/local/bin:$PATH\\n\"\n\
             [[file]]\npath = \"packages/p.yaml\"\n\
             content = \"environments:\\n  e:\\n    check: test -x /usr/bin/true\\n\"\n\
             [[run]]\nargs = []\n",
        );
        assert_eq!(ordinary.refusal(), None);
    }

    #[test]
    fn any_spelling_of_a_config_key_counts_as_set() {
        for spelled in [
            "state_directory: @HOME@/s\n",
            "{state_directory: /x}\n",
            "\"state_directory\": /x\n",
            "base: &b\n  state_directory: /x\n<<: *b\n",
        ] {
            assert!(sets(spelled, "state_directory"), "{spelled:?}");
        }
        assert!(sets("\"state\\x5fdirectory\": x\n", "state_directory"));
        assert!(!sets("environment: e\n", "state_directory"));
    }

    #[test]
    fn nothing_is_written_through_a_symlink_the_fixture_made() {
        let root = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        let f = fixture(&format!(
            "[[symlink]]\npath = \"link\"\nto = \"{}\"\n\
             [[symlink]]\npath = \"link/inner\"\nto = \"x\"\n[[run]]\nargs = []\n",
            elsewhere.path().display()
        ));
        let err = lay_out(&root.path().join("home"), &f, None).unwrap_err();
        assert!(err.to_string().contains("symlink"), "{err}");
        assert!(fs::read_dir(elsewhere.path()).unwrap().next().is_none());
    }

    #[test]
    fn a_fixture_is_laid_out_with_home_substituted_and_modes_applied() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("home");
        let f = fixture(
            "[[file]]\npath = \"t/secret\"\ncontent = \"at @HOME@\\n\"\nmode = 0o200\n\
             [[symlink]]\npath = \"t/gone\"\nto = \"@HOME@/nowhere\"\n[[run]]\nargs = []\n",
        );
        lay_out(&home, &f, None).unwrap();
        let seen = observe(&home, "t/secret");
        let content = format!("{:?}", format!("at {}\n", home.display()));
        assert_eq!(seen, format!("file mode=200 content={content}"));
        // Observing lent the file read permission and gave it back.
        let mode = fs::metadata(home.join("t/secret"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o200);
        let link = observe(&home, "t/gone");
        assert_eq!(link, format!("symlink -> {}/nowhere", home.display()));
        assert!(
            home.join(".config/selfie/config.yaml").is_file(),
            "no default config"
        );
    }

    #[test]
    fn directory_modes_are_applied_deepest_first() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("home");
        let f = fixture(
            "[[dir]]\npath = \"a\"\nmode = 0o600\n[[dir]]\npath = \"a/b\"\nmode = 0o700\n\
             [[run]]\nargs = []\n",
        );
        lay_out(&home, &f, None).unwrap();
        fs::set_permissions(home.join("a"), fs::Permissions::from_mode(0o700)).unwrap();
        let mode = fs::metadata(home.join("a/b")).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);
    }

    #[test]
    fn observing_never_follows_a_link_or_reads_a_fifo() {
        let root = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        fs::write(elsewhere.path().join("f"), "outside").unwrap();
        let home = root.path().join("home");
        let f = fixture(&format!(
            "[[symlink]]\npath = \"out\"\nto = \"{}\"\n[[run]]\nargs = []\n",
            elsewhere.path().display()
        ));
        lay_out(&home, &f, None).unwrap();
        let seen = observe(&home, "out/f");
        assert!(seen.starts_with("under symlink"), "{seen}");
        assert_eq!(observe(&home, "missing"), "absent");
        let status = Command::new("mkfifo")
            .arg(home.join("pipe"))
            .status()
            .unwrap();
        assert!(status.success());
        let seen = observe(&home, "pipe");
        assert!(seen.starts_with("special file"), "{seen}");
    }
}
