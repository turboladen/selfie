//! `cargo xtask bindiff`: run the base and HEAD binaries over identical
//! sandboxes and diff what they print, exit with and leave behind.

mod confine;
mod fixture;
mod sandbox;

use std::fs::{self, File};
use std::io::Read;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use similar::TextDiff;

use crate::cargo::{self, Run};
use crate::git::Repo;
use crate::proc::{self, Outcome};
use crate::workdir::{self, Summary};
use confine::Confinement;
use fixture::{FileSpec, Fixture, LinkSpec, RunSpec};
use sandbox::Gate;

const STARTER_FIXTURES: &str = "crates/xtask/fixtures/bindiff";

/// Arguments for `cargo xtask bindiff`.
#[derive(clap::Args)]
pub struct Args {
    /// The branch or commit to compare with HEAD, usually `main`. Its merge
    /// base with HEAD is what gets built.
    #[arg(required_unless_present = "self_test")]
    base: Option<String>,
    /// A directory of fixture files, one `*.toml` per fixture. Defaults to
    /// the starter fixtures as committed at HEAD.
    fixtures: Option<PathBuf>,
    /// Where archives, targets, sandboxes and transcripts go. Must be
    /// absolute, new or empty, and outside every checkout. Defaults to a new
    /// directory under `TMPDIR`.
    #[arg(long)]
    work_dir: Option<PathBuf>,
    /// Build HEAD once and require the harness to see no difference between
    /// HEAD and itself, to see a planted one and one between two different
    /// binaries, and to keep every escaping fixture inside its sandbox.
    #[arg(long, conflicts_with_all = ["base", "fixtures"])]
    self_test: bool,
}

fn command_line(args: &[String]) -> String {
    let quoted =
        shlex::try_join(args.iter().map(String::as_str)).unwrap_or_else(|_| format!("{args:?}"));
    format!("selfie {quoted}")
}

enum Compared {
    Identical,
    Differs(String),
    Refused(String),
    NeverRan(String),
    Error(String),
}

impl Compared {
    fn label(&self) -> String {
        match self {
            Compared::Identical => "identical".into(),
            Compared::Differs(_) => "DIFFERS".into(),
            Compared::Refused(why) => format!("REFUSED ({why})"),
            Compared::NeverRan(what) => format!("NEVER-RAN ({what} timed out)"),
            Compared::Error(e) => format!("ERROR ({e})"),
        }
    }
}

// One side's sandbox: `boxed` is the only directory the binary may write,
// and holds its HOME and TMPDIR. Logs, transcripts and the profile sit
// beside it in `dir`, where the binary cannot reach them.
struct Side<'a> {
    name: &'static str,
    bin: &'a Path,
    dir: PathBuf,
    boxed: PathBuf,
    home: PathBuf,
}

#[derive(Clone, Copy)]
struct Comparison<'a> {
    base: &'a Path,
    head: &'a Path,
    work: &'a Path,
    // The whole work directory, which a confined binary may read even when
    // it lies inside a home directory.
    top: &'a Path,
    homes: &'a [PathBuf],
    unreadable: &'a [PathBuf],
    // Only the self-test turns this off, to show what the gate and the OS
    // stop on their own.
    refuse_early: bool,
    base_extra: Option<&'a FileSpec>,
}

impl Comparison<'_> {
    // A fixture's own trouble is its own verdict, so the rest still run.
    fn fixture(&self, name: &str, fixture: &Fixture) -> Compared {
        self.compare(name, fixture)
            .unwrap_or_else(|e| Compared::Error(format!("{e:#}")))
    }

    fn confined(&self, side: &Side) -> Result<Command> {
        let confinement = Confinement {
            writable: &side.boxed,
            homes: self.homes,
            readable: self.top,
            unreadable: self.unreadable,
        };
        let mut cmd = confinement.command(side.bin, &side.dir.join("profile.sb"))?;
        sandbox::sandboxed(&mut cmd, &side.home, &side.boxed);
        Ok(cmd)
    }

    fn lay_out<'b>(
        &self,
        name: &str,
        fixture: &Fixture,
        side: &'static str,
        bin: &'b Path,
        extra: Option<&FileSpec>,
    ) -> Result<Side<'b>> {
        let dir = self.work.join(side).join(name);
        fs::create_dir_all(&dir)?;
        let boxed = dir.join("box");
        fs::create_dir(&boxed)?;
        fs::create_dir(boxed.join("tmp"))?;
        let home = boxed.join("home");
        fixture::lay_out(&home, fixture, extra)?;
        Ok(Side {
            name: side,
            bin,
            dir,
            boxed: fs::canonicalize(&boxed)?,
            home: fs::canonicalize(&home)?,
        })
    }

    fn compare(&self, name: &str, fixture: &Fixture) -> Result<Compared> {
        if self.refuse_early
            && let Some(why) = fixture.refusal()
        {
            return Ok(Compared::Refused(why));
        }
        let sides = [
            self.lay_out(name, fixture, "base", self.base, self.base_extra)?,
            self.lay_out(name, fixture, "head", self.head, None)?,
        ];
        // Both sides are checked before either runs, so a base binary with a
        // weaker gate cannot act before the head's gate refuses.
        for side in &sides {
            if self.refuse_early
                && let Some(why) = sandbox::escaping_link(&side.home, fixture)
            {
                return Ok(Compared::Refused(why));
            }
            let mut cmd = self.confined(side)?;
            match sandbox::gate(&mut cmd, &side.home, &side.dir, fixture)? {
                Gate::Passed => {}
                Gate::Refused(why) => {
                    return Ok(Compared::Refused(format!("{} gate: {why}", side.name)));
                }
                Gate::TimedOut => return Ok(Compared::NeverRan(format!("the {} gate", side.name))),
            }
        }
        let mut transcripts = Vec::new();
        for side in &sides {
            match self.run(side, fixture)? {
                Ok(transcript) => transcripts.push(transcript),
                Err(never_ran) => return Ok(never_ran),
            }
        }
        let (base, head) = (&transcripts[0], &transcripts[1]);
        if base == head {
            return Ok(Compared::Identical);
        }
        let diff = TextDiff::from_lines(base, head)
            .unified_diff()
            .context_radius(3)
            .header(&format!("base/{name}"), &format!("head/{name}"))
            .to_string();
        Ok(Compared::Differs(diff))
    }

    // Runs every step of the fixture on one side and returns its normalized
    // transcript, or the verdict for a run that hit its deadline.
    fn run(&self, side: &Side, fixture: &Fixture) -> Result<Result<String, Compared>> {
        let deadline = Duration::from_secs(fixture.timeout_secs);
        let mut transcript = String::new();
        for (i, run) in fixture.run.iter().enumerate() {
            let args: Vec<String> = run
                .args
                .iter()
                .map(|a| fixture::with_home(a, &side.home))
                .collect();
            let captured = sandbox::capture(
                self.confined(side)?.args(&args),
                &side.dir.join(format!("run-{i}")),
                deadline,
            )?;
            // A run that timed out on both sides would compare equal
            // without having shown anything.
            if captured.outcome == Outcome::TimedOut {
                return Ok(Err(Compared::NeverRan(format!("{} run {i}", side.name))));
            }
            transcript.push_str(&format!(
                "$ {}\nexit: {}\n--- stdout\n{}--- stderr\n{}",
                command_line(&run.args),
                captured.outcome,
                captured.stdout,
                captured.stderr
            ));
            for path in &fixture.observe {
                let seen = fixture::observe(&side.home, path);
                transcript.push_str(&format!("--- {path}: {seen}\n"));
            }
            transcript.push('\n');
        }
        let transcript = sandbox::normalize(&transcript, &side.home, &side.boxed);
        fs::write(side.dir.join("transcript.txt"), &transcript)?;
        Ok(Ok(transcript))
    }
}

// Builds `selfie` at `sha` from its own archive and target directory, and
// returns the executable cargo reported.
fn build(repo: &Repo, sha: &str, dir: &Path) -> Result<PathBuf> {
    fs::create_dir(dir)?;
    let src = dir.join("src");
    repo.archive(sha, &src)?;
    let json = dir.join("build.json");
    let log = dir.join("build.log");
    let target = dir.join("target");
    let outcome = proc::run(
        cargo::isolate(cargo::scrub_env(&mut Command::new("cargo")), &target)
            .args([
                "build",
                "-p",
                "selfie-cli",
                "--message-format=json-render-diagnostics",
            ])
            .current_dir(&src)
            .stdout(File::create(&json)?)
            .stderr(File::create(&log)?),
        Duration::from_secs(1800),
    )?;
    let text = String::from_utf8_lossy(&fs::read(&log)?).into_owned();
    if cargo::classify(outcome, &text, "selfie-cli") != Run::Built {
        bail!(
            "could not build selfie-cli at {sha} ({outcome}); see {}",
            log.display()
        );
    }
    let messages = String::from_utf8_lossy(&fs::read(&json)?).into_owned();
    cargo::executable(&messages, "selfie")
        .with_context(|| format!("cargo reported no selfie binary; see {}", json.display()))
}

// A fixture's name becomes a directory, so it must be one plain component.
fn parse(file_name: &str, text: &str) -> Result<(String, Fixture)> {
    let stem = file_name.strip_suffix(".toml").unwrap_or(file_name);
    if workdir::plain_relative(stem).is_none() || stem.contains('/') {
        bail!("fixture file {file_name:?} needs a plain name");
    }
    let fixture =
        Fixture::parse(text).with_context(|| format!("fixture {file_name} is not valid"))?;
    Ok((stem.to_owned(), fixture))
}

// The starter fixtures are read from HEAD's commit, like the binary, so an
// uncommitted edit is not measured.
fn committed_fixtures(repo: &Repo, sha: &str) -> Result<Vec<(String, Fixture)>> {
    let names = repo.list(sha, STARTER_FIXTURES)?;
    names
        .iter()
        .filter(|n| n.ends_with(".toml"))
        .map(|n| parse(n, &repo.show(sha, &format!("{STARTER_FIXTURES}/{n}"))?))
        .collect()
}

fn fixtures_in(dir: &Path) -> Result<Vec<(String, Fixture)>> {
    let mut paths: Vec<PathBuf> = fs::read_dir(dir)
        .with_context(|| format!("could not read {}", dir.display()))?
        .map(|e| e.map(|e| e.path()))
        .collect::<Result<_, _>>()?;
    paths.retain(|p| p.extension().is_some_and(|e| e == "toml"));
    paths.sort();
    if paths.is_empty() {
        bail!("{} holds no *.toml fixtures", dir.display());
    }
    paths
        .iter()
        .map(|p| {
            let text =
                fs::read_to_string(p).with_context(|| format!("could not read {}", p.display()))?;
            parse(&p.file_name().unwrap_or_default().to_string_lossy(), &text)
        })
        .collect()
}

// Each fixture's verdict and diff are written as soon as it finishes, so an
// interrupted run keeps what it already compared.
fn compare_all(
    comparison: &Comparison,
    fixtures: &[(String, Fixture)],
    summary: &mut Summary,
) -> Result<Vec<Compared>> {
    let mut results = Vec::new();
    for (name, fixture) in fixtures {
        let result = comparison.fixture(name, fixture);
        summary.line(&format!("== {name}: {}", result.label()))?;
        if let Compared::Differs(diff) = &result {
            summary.line(diff.trim_end())?;
        }
        results.push(result);
    }
    Ok(results)
}

fn covered(
    summary: &mut Summary,
    fixtures: &[(String, Fixture)],
    results: &[Compared],
) -> Result<ExitCode> {
    summary.line("fixtures covered:")?;
    for ((name, fixture), result) in fixtures.iter().zip(results) {
        summary.line(&format!("  {name}: {}", result.label()))?;
        for run in &fixture.run {
            summary.line(&format!("      {}", command_line(&run.args)))?;
        }
    }
    Ok(if results.iter().any(|r| matches!(r, Compared::Error(_))) {
        ExitCode::from(2)
    } else if results.iter().all(|r| matches!(r, Compared::Identical)) {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

/// Builds both binaries, runs every fixture against each under macOS
/// `sandbox-exec`, and prints the differences and the list of fixtures
/// covered.
///
/// # Errors
///
/// Fails on a host without `sandbox-exec`, when the fixtures cannot be read,
/// when the merge base is HEAD itself, or when a binary cannot be built. A
/// fixture that is refused, times out or cannot be laid down is reported
/// with its own verdict instead.
pub fn run(args: &Args) -> Result<ExitCode> {
    confine::available()?;
    // Asked before anything is built, since without a home to protect the
    // run cannot go ahead.
    let homes = confine::real_homes()?;
    let repo = Repo::discover()?;
    let head = repo.commit("HEAD")?;
    if args.self_test {
        let work = workdir::resolve(args.work_dir.as_deref(), "bindiff", &repo.checkouts()?)?;
        let mut summary = Summary::create(&work.join("summary.txt"))?;
        return self_test(&repo, &head, &work, &homes, &mut summary);
    }
    let Some(base_ref) = &args.base else {
        bail!("a base commit is required");
    };
    // The merge base, not the tip of `base_ref`: a base that has moved on
    // since the branch forked would show its own newer changes reversed.
    let base = repo.merge_base(&repo.commit(base_ref)?, &head)?;
    if base == head {
        bail!(
            "the merge base of {base_ref} with HEAD is HEAD itself, so there is nothing to compare"
        );
    }
    // The fixtures are read before the work directory exists, so a bad one
    // leaves nothing behind.
    let fixtures = match &args.fixtures {
        Some(dir) => fixtures_in(dir)?,
        None => committed_fixtures(&repo, &head)?,
    };
    let work = workdir::resolve(args.work_dir.as_deref(), "bindiff", &repo.checkouts()?)?;
    let mut summary = Summary::create(&work.join("summary.txt"))?;
    summary.line(&format!(
        "bindiff: merge base {base} (with {base_ref}), HEAD {head}"
    ))?;
    summary.line(&format!("work dir: {}", work.display()))?;
    if repo.is_dirty()? {
        summary
            .line("note: the working tree differs from HEAD; HEAD's committed tree is what runs")?;
    }
    // The two builds share nothing, so they run side by side.
    let (base_bin, head_bin) = thread::scope(|s| {
        let base_build = s.spawn(|| build(&repo, &base, &work.join("build-base")));
        let head_build = build(&repo, &head, &work.join("build-head"));
        let base_build = base_build
            .join()
            .map_err(|_| anyhow::anyhow!("the base build panicked"));
        Ok::<_, anyhow::Error>((base_build??, head_build?))
    })?;
    let comparison = Comparison {
        base: &base_bin,
        head: &head_bin,
        work: &work,
        top: &work,
        homes: &homes,
        unreadable: &[],
        refuse_early: true,
        base_extra: None,
    };
    let results = compare_all(&comparison, &fixtures, &mut summary)?;
    let code = covered(&mut summary, &fixtures, &results)?;
    summary.line(&format!("summary: {}", work.join("summary.txt").display()))?;
    Ok(code)
}

// A stand-in binary that passes the gate for any sandbox and prints `mark`
// for every other command.
fn stub(path: &Path, mark: &str) -> Result<()> {
    let script = format!(
        "#!/bin/sh\n\
         if [ \"$2\" = config ]; then\n\
         \x20 for d in package_directory dotfiles_directory state_directory; do\n\
         \x20   echo \"  $d: $HOME/x\"\n\
         \x20 done\n\
         else\n\
         \x20 echo {mark}\n\
         fi\n"
    );
    fs::write(path, script)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o755))?;
    Ok(())
}

fn run_spec(args: &[&str]) -> RunSpec {
    RunSpec {
        args: args.iter().map(|a| (*a).to_owned()).collect(),
    }
}

fn listing() -> Fixture {
    Fixture {
        run: vec![run_spec(&["--no-color", "package", "list"])],
        ..Fixture::default()
    }
}

fn file(path: &str, content: String) -> FileSpec {
    FileSpec {
        path: path.into(),
        content,
        mode: None,
    }
}

fn config_file(content: String) -> FileSpec {
    file(".config/selfie/config.yaml", content)
}

// A package deploying one dotfile to `target`, with its source file.
fn deploying(target: &str) -> Vec<FileSpec> {
    vec![
        file("packages/tool/rc", "setting = 1\n".into()),
        file(
            "packages/tool.yaml",
            format!(
                "_t: &t {target}\nname: tool\ndotfiles:\n  - source: tool/rc\n    target: *t\n\
                 environments:\n  sandbox:\n    install: \"true\"\n"
            ),
        ),
    ]
}

/// What an escape control must end in.
enum Want {
    /// It is refused, for a reason containing this phrase.
    Refused(&'static str),
    /// It runs, and the OS denies the escape on a line that also contains
    /// this phrase, which ties the denial to the escaping step.
    Blocked(&'static str),
}

const MARKER: &str = "xtask-outside-marker";

// The directories the escape controls aim at. Each holds a file named
// `secret` containing the marker.
struct Targets<'a> {
    // Readable by the confined binary but not writable, for write escapes.
    walled: &'a Path,
    // Neither readable nor writable, for read escapes.
    outside: &'a Path,
    // Treated as a home directory, whose reads the profile's home rule
    // denies.
    home: &'a Path,
}

// Each escape control carries the fixture, whether the early refusals run,
// and how it must end.
fn escape_controls(t: &Targets) -> Vec<(&'static str, Fixture, bool, Want)> {
    let (walled, outside, home) = (
        t.walled.display().to_string(),
        t.outside.display().to_string(),
        t.home.display().to_string(),
    );
    let config = Fixture {
        file: vec![config_file(format!(
            "environment: sandbox\npackage_directory: {walled}\n"
        ))],
        ..listing()
    };
    let link = Fixture {
        symlink: vec![LinkSpec {
            path: "state".into(),
            to: walled.clone(),
        }],
        file: vec![config_file(
            "environment: sandbox\npackage_directory: @HOME@/packages\n\
             state_directory: @HOME@/state\n"
                .into(),
        )],
        ..listing()
    };
    // The target hides behind a YAML anchor, which no text rule reads.
    let anchor = Fixture {
        file: deploying(&format!("{walled}/anchored")),
        run: vec![run_spec(&["--no-color", "apply", "--yes"])],
        ..Fixture::default()
    };
    let track = |from: &str| Fixture {
        dir: vec![fixture::DirSpec {
            path: "dotfiles".into(),
            mode: None,
        }],
        run: vec![run_spec(&[
            "--no-color",
            "dotfiles",
            "track",
            "leak",
            &format!("{from}/secret"),
        ])],
        ..Fixture::default()
    };
    // The environment's value forges three directory lines ahead of the
    // real ones in `config validate`'s output.
    let forged = Fixture {
        file: vec![config_file(format!(
            "environment: \"sandbox\\n  package_directory: @HOME@/packages\\n  \
             dotfiles_directory: @HOME@/dotfiles\\n  state_directory: @HOME@/state\"\n\
             package_directory: @HOME@/packages\nstate_directory: {walled}/state\n"
        ))],
        ..listing()
    };
    let no_load = Fixture {
        file: vec![config_file("environment: sandbox\n".into())],
        ..listing()
    };
    // Only the OS can stop this one: the gate cannot see a flag.
    let state = format!("{walled}/state");
    let flag_write = Fixture {
        file: deploying("~/.toolrc"),
        run: vec![run_spec(&[
            "--no-color",
            "--state-directory",
            &state,
            "apply",
            "--yes",
        ])],
        ..Fixture::default()
    };
    let flag = Fixture {
        run: vec![run_spec(&[
            "--no-color",
            "--state-directory",
            "../../x",
            "apply",
        ])],
        ..Fixture::default()
    };
    vec![
        (
            "config-scan",
            config.clone(),
            true,
            Want::Refused("leaves @HOME@"),
        ),
        (
            "config-gate",
            config,
            false,
            Want::Refused("outside the sandbox"),
        ),
        ("flag", flag, true, Want::Refused("overrides a directory")),
        (
            "link",
            link.clone(),
            true,
            Want::Refused("the symlink state leads to"),
        ),
        (
            "link-gate",
            link,
            false,
            Want::Refused("outside the sandbox"),
        ),
        ("anchor", anchor, true, Want::Blocked("walled/anchored")),
        (
            "track",
            track(&outside),
            true,
            Want::Refused("names a path that leaves"),
        ),
        (
            "track-os",
            track(&outside),
            false,
            Want::Blocked("Cannot read target file"),
        ),
        (
            "track-home",
            track(&home),
            false,
            Want::Blocked("Cannot read target file"),
        ),
        ("forged", forged, false, Want::Refused("times; only one")),
        ("no-load", no_load, true, Want::Refused("does not load")),
        ("flag-os", flag_write, false, Want::Blocked("walled/state")),
    ]
}

// Every entry under `dir`, not following symlinks.
fn entries_under(dir: &Path, found: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        found.push(path.clone());
        if entry.file_type().is_ok_and(|t| t.is_dir()) {
            entries_under(&path, found);
        }
    }
}

// Reads a regular file under `dir` without following a symlink the binary
// may have planted, and at most OUTPUT_CAP bytes of it. Anything else reads
// as empty.
fn read_plain(path: &Path) -> String {
    let regular = fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_file());
    if !regular {
        return String::new();
    }
    let opened = fs::OpenOptions::new()
        .read(true)
        .custom_flags(nofollow_flags())
        .open(path);
    let mut bytes = Vec::new();
    if let Ok(file) = opened {
        let _ = file
            .take(sandbox::OUTPUT_CAP as u64)
            .read_to_end(&mut bytes);
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

fn nofollow_flags() -> i32 {
    nix::fcntl::OFlag::O_NOFOLLOW.bits() | nix::fcntl::OFlag::O_NONBLOCK.bits()
}

// A blocked control must show the OS denying the escape: a failed run's
// transcript has "Operation not permitted" on the same line as `phrase`,
// which names the escaping step. Nothing either sandbox holds or printed may
// contain the marker it tried to read.
fn blocked(dir: &Path, phrase: &str) -> Result<(), String> {
    let mut entries = Vec::new();
    entries_under(dir, &mut entries);
    let denied = entries
        .iter()
        .filter(|p| p.file_name().is_some_and(|n| n == "transcript.txt"))
        .map(|p| read_plain(p))
        .any(|t| {
            t.lines()
                .any(|l| l.starts_with("exit: ") && l != "exit: exit 0")
                && t.lines()
                    .any(|l| l.contains("Operation not permitted") && l.contains(phrase))
        });
    if !denied {
        return Err(format!(
            "no failed run shows the OS denying the step naming {phrase:?}"
        ));
    }
    let printed = |p: &&PathBuf| {
        p.extension()
            .is_some_and(|e| e == "stdout" || e == "stderr")
            || p.components().any(|c| c.as_os_str() == "box")
    };
    match entries
        .iter()
        .filter(printed)
        .find(|p| read_plain(p).contains(MARKER))
    {
        Some(p) => Err(format!("{} holds the outside file's content", p.display())),
        None => Ok(()),
    }
}

// Every entry under `dir` with its type, mode and content, not following
// symlinks, to show nothing was created, removed or changed.
fn snapshot(dir: &Path) -> Vec<String> {
    let mut entries = Vec::new();
    entries_under(dir, &mut entries);
    entries.sort();
    entries
        .iter()
        .map(|p| match fs::symlink_metadata(p) {
            Ok(m) => format!(
                "{} {:?} {:o} {:?} {:?}",
                p.display(),
                m.file_type(),
                m.permissions().mode(),
                fs::read_link(p).ok(),
                m.is_file().then(|| fs::read(p).unwrap_or_default()),
            ),
            Err(e) => format!("{} {e}", p.display()),
        })
        .collect()
}

fn self_test(
    repo: &Repo,
    head: &str,
    work: &Path,
    real_homes: &[PathBuf],
    summary: &mut Summary,
) -> Result<ExitCode> {
    summary.line(&format!("bindiff self-test: HEAD {head}"))?;
    summary.line(&format!("work dir: {}", work.display()))?;
    let bin = build(repo, head, &work.join("build-head"))?;
    let fixtures = committed_fixtures(repo, head)?;
    // A stand-in home, so the home rule is exercised without touching the
    // real one. It lies outside the work directory, whose reads the profile
    // allows.
    let fake_home = tempfile::Builder::new()
        .prefix("xtask-bindiff-home-")
        .tempdir()?;
    let mut homes = real_homes.to_vec();
    homes.push(fs::canonicalize(fake_home.path())?);
    let targets = work.join("targets");
    let mut dirs = Vec::new();
    for (name, base) in [
        ("walled", targets.as_path()),
        ("outside", targets.as_path()),
    ] {
        let dir = base.join(name);
        fs::create_dir_all(&dir)?;
        dirs.push(fs::canonicalize(&dir)?);
    }
    dirs.push(homes[homes.len() - 1].clone());
    for dir in &dirs {
        fs::write(dir.join("secret"), format!("{MARKER}\n"))?;
    }
    let unreadable = [dirs[1].clone()];
    let before: Vec<Vec<String>> = dirs.iter().map(|d| snapshot(d)).collect();
    let mut failures = Vec::new();

    // Control 1: HEAD against itself must be identical on every starter
    // fixture, which shows the sandbox path and timestamps are normalized.
    let same = Comparison {
        base: &bin,
        head: &bin,
        work: &work.join("same"),
        top: work,
        homes: &homes,
        unreadable: &unreadable,
        refuse_early: true,
        base_extra: None,
    };
    for ((name, _), result) in fixtures.iter().zip(compare_all(&same, &fixtures, summary)?) {
        if !matches!(result, Compared::Identical) {
            failures.push(format!("same/{name}: {}", result.label()));
        }
    }

    // Control 2: one extra package on the base side must show as a
    // difference naming it. This proves the differ sees a difference; it is
    // not a binary diff.
    let extra = file(
        "packages/planted.yaml",
        "name: planted\nenvironments:\n  sandbox:\n    install: \"true\"\n".into(),
    );
    let planted = Comparison {
        base_extra: Some(&extra),
        work: &work.join("planted"),
        ..same
    };
    match planted.fixture("planted", &listing()) {
        // A body line, not the `--- base/planted` header, must name it.
        Compared::Differs(diff)
            if diff
                .lines()
                .any(|l| l.starts_with('-') && !l.starts_with("---") && l.contains("planted")) => {}
        other => failures.push(format!("planted: {}", other.label())),
    }

    // Control 3: two different binaries must differ, which a harness that ran
    // one binary on both sides would not show.
    let stubs = work.join("stubs");
    fs::create_dir(&stubs)?;
    let (stub_a, stub_b) = (stubs.join("a"), stubs.join("b"));
    stub(&stub_a, "stub-a")?;
    stub(&stub_b, "stub-b")?;
    let two = Comparison {
        base: &stub_a,
        head: &stub_b,
        work: &work.join("two-binaries"),
        ..same
    };
    match two.fixture("two-binaries", &listing()) {
        Compared::Differs(diff) if diff.contains("-stub-a") && diff.contains("+stub-b") => {}
        other => failures.push(format!("two-binaries: {}", other.label())),
    }

    // Control 4: every escaping fixture must stay inside its sandbox, each
    // stopped where its control says: by an early refusal, by the gate with
    // the early refusals off, or by the OS.
    let targets = Targets {
        walled: &dirs[0],
        outside: &dirs[1],
        home: &dirs[2],
    };
    for (name, fixture, refuse_early, want) in escape_controls(&targets) {
        let dir = work.join("escape").join(name);
        let control = Comparison {
            refuse_early,
            work: &dir,
            ..same
        };
        let result = control.fixture(name, &fixture);
        let verdict = match (&want, &result) {
            (Want::Refused(phrase), Compared::Refused(why)) if why.contains(phrase) => Ok(()),
            (Want::Refused(phrase), other) => Err(format!(
                "{} (wanted a refusal naming {phrase:?})",
                other.label()
            )),
            (Want::Blocked(_), Compared::Refused(_) | Compared::Error(_)) => Err(format!(
                "{} (wanted it to run and be blocked)",
                result.label()
            )),
            (Want::Blocked(phrase), _) => blocked(&dir, phrase),
        };
        if let Err(why) = verdict {
            failures.push(format!("escape/{name}: {why}"));
        }
    }
    for (dir, before) in dirs.iter().zip(&before) {
        if snapshot(dir) != *before {
            failures.push(format!("escape: {} changed", dir.display()));
        }
    }

    if failures.is_empty() {
        summary.line(&format!(
            "self-test: {} starter fixtures identical against themselves, the planted \
             difference seen, two binaries told apart, and every escaping fixture kept inside \
             its sandbox",
            fixtures.len()
        ))?;
        Ok(ExitCode::SUCCESS)
    } else {
        for f in &failures {
            summary.line(&format!("self-test FAILED {f}"))?;
        }
        bail!("the binary-diff harness's self-test failed; this instrument cannot be trusted")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_command_line_keeps_its_argument_boundaries() {
        let args = vec![
            "spec".to_owned(),
            "search".to_owned(),
            "search tool".to_owned(),
        ];
        assert_eq!(command_line(&args), "selfie spec search 'search tool'");
    }

    #[test]
    fn an_unreadable_fixture_is_named() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("bad.toml"), [0xff, 0xfe]).unwrap();
        let err = fixtures_in(dir.path()).unwrap_err();
        assert!(format!("{err:#}").contains("bad.toml"), "{err:#}");
    }

    #[test]
    fn every_escape_control_names_a_distinct_fixture() {
        let nowhere = Path::new("/nowhere");
        let controls = escape_controls(&Targets {
            walled: nowhere,
            outside: nowhere,
            home: nowhere,
        });
        let names: std::collections::HashSet<&str> = controls.iter().map(|c| c.0).collect();
        assert_eq!(names.len(), controls.len());
    }
}
