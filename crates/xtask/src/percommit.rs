//! `cargo xtask percommit`: `just clippy` on every commit below the tip.

use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::time::Duration;

use anyhow::{Context, Result, bail};

use crate::cargo::{self, Build};
use crate::git::Repo;
use crate::proc::{self, Outcome};
use crate::workdir;

/// Arguments for `cargo xtask percommit`.
#[derive(clap::Args)]
pub struct Args {
    /// The branch or commit the range starts from, usually `main`. Every commit
    /// after its merge base with HEAD is checked. HEAD itself is left to
    /// `just check`, unless `--include-tip` is given or the working tree
    /// differs from HEAD.
    base: String,
    /// Where archives, targets and logs go. Must be absolute, new or empty,
    /// and outside every checkout. Defaults to a new directory under `TMPDIR`.
    #[arg(long)]
    work_dir: Option<PathBuf>,
    /// Check HEAD as well, even when the working tree matches it.
    #[arg(long)]
    include_tip: bool,
    /// Keep each commit's target directory instead of deleting it once scored.
    #[arg(long)]
    keep_targets: bool,
    /// Before the real run, inject a type error and a clippy warning into the
    /// merge base and require each to fail the check on the injected line.
    #[arg(long)]
    self_test: bool,
    /// Minutes allowed for one commit's `just clippy`.
    #[arg(long, default_value_t = 30, value_parser = clap::value_parser!(u64).range(1..=1440))]
    timeout_mins: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Fail {
    CompileError,
    Exit(Outcome),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NeverRan {
    TimedOut,
    NoBuildLine,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Verdict {
    Ok,
    Fail(Fail),
    NeverRan(NeverRan),
}

impl fmt::Display for Verdict {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Verdict::Ok => write!(f, "ok"),
            Verdict::Fail(Fail::CompileError) => write!(f, "FAIL (compile error)"),
            Verdict::Fail(Fail::Exit(outcome)) => write!(f, "FAIL ({outcome})"),
            Verdict::NeverRan(NeverRan::TimedOut) => write!(f, "NEVER-RAN (timed out)"),
            Verdict::NeverRan(NeverRan::NoBuildLine) => write!(
                f,
                "NEVER-RAN (no `Compiling selfie v` or `Checking selfie v` line)"
            ),
        }
    }
}

fn score(log: &str, outcome: Outcome) -> Verdict {
    if outcome == Outcome::TimedOut {
        return Verdict::NeverRan(NeverRan::TimedOut);
    }
    match cargo::build(log, "selfie") {
        Build::CompileError => Verdict::Fail(Fail::CompileError),
        Build::NotStarted => Verdict::NeverRan(NeverRan::NoBuildLine),
        Build::Built if outcome.success() => Verdict::Ok,
        Build::Built => Verdict::Fail(Fail::Exit(outcome)),
    }
}

// The commits to check, oldest first, given `range` (the output of
// `rev-list --reverse base..HEAD`, so HEAD is last whenever it is non-empty).
fn commits_to_check(mut range: Vec<String>, head: &str, with_tip: bool) -> Vec<String> {
    let tip_present = range.last().is_some_and(|last| last == head);
    if with_tip {
        if !tip_present {
            range.push(head.to_owned());
        }
    } else if tip_present {
        range.pop();
    }
    range
}

// An injection is text appended to one file of the archive before it is
// checked, so the run is known to have something to fail on.
struct Injection {
    name: &'static str,
    file: &'static str,
    text: &'static str,
}

// rustc quotes the offending source line, trailing comment included, so an
// error diagnostic quoting this marker failed on the injected line and not
// on something the commit already carried.
const MARKER: &str = "xtask-self-test-marker";

// `lib.rs` is always compiled, so the build cannot skip either control. The
// second is a warn-level lint, which is an error only while `-D warnings` is
// in force.
const CONTROLS: [Injection; 2] = [
    Injection {
        name: "type-error",
        file: "crates/selfie/src/lib.rs",
        text: "\nconst _XTASK_SELF_TEST: u32 = \"not a number\"; // xtask-self-test-marker\n",
    },
    Injection {
        name: "clippy-warning",
        file: "crates/selfie/src/lib.rs",
        text: "\nfn _xtask_self_test(v: &[u8]) -> bool {\n    v.len() == 0 // xtask-self-test-marker\n}\n",
    },
];

// Whether the marker is quoted inside an `error` diagnostic. A `warning`
// quoting it would mean the lint fired without being denied.
fn marker_in_error(log: &str) -> bool {
    let mut in_error = false;
    for line in log.lines() {
        if line.starts_with("error") {
            in_error = true;
        } else if line.starts_with("warning") {
            in_error = false;
        }
        if line.contains(MARKER) && in_error {
            return true;
        }
    }
    false
}

struct Checked {
    verdict: Verdict,
    log: PathBuf,
    src: PathBuf,
}

struct Runner<'a> {
    repo: &'a Repo,
    keep_targets: bool,
    deadline: Duration,
    // HEAD's Justfile, written over each archive's own so every commit is
    // held to the recipe CI runs at the tip. A commit that weakened its own
    // `clippy` recipe would otherwise be checked by the weakened one.
    justfile: String,
}

impl Runner<'_> {
    fn check(
        &self,
        sha: &str,
        dir: &Path,
        inject: Option<&Injection>,
        target: &Path,
    ) -> Result<Checked> {
        fs::create_dir(dir).with_context(|| format!("could not create {}", dir.display()))?;
        let src = dir.join("src");
        self.repo.archive(sha, &src)?;
        fs::write(src.join("Justfile"), &self.justfile)?;
        if let Some(inject) = inject {
            let mut file = OpenOptions::new()
                .append(true)
                .open(src.join(inject.file))
                .with_context(|| format!("could not open {} to inject into", inject.file))?;
            file.write_all(inject.text.as_bytes())?;
        }
        let log = dir.join("clippy.log");
        let file = File::create(&log)?;
        let outcome = proc::run(
            cargo::isolate(cargo::scrub_env(&mut Command::new("just")), target)
                .arg("clippy")
                .current_dir(&src)
                .stdout(file.try_clone()?)
                .stderr(file),
            self.deadline,
        )?;
        // The log is read lossily, because one stray byte from a build script
        // must not abort the run before this commit is scored.
        let verdict = score(&String::from_utf8_lossy(&fs::read(&log)?), outcome);
        Ok(Checked { verdict, log, src })
    }

    fn drop_target(&self, target: &Path) -> Result<()> {
        if !self.keep_targets && target.exists() {
            fs::remove_dir_all(target)?;
        }
        Ok(())
    }
}

// The versions are asked inside an archive, so a toolchain file in that
// commit is honored. A toolchain file naming an uninstalled toolchain makes
// rustup download it, hence the deadline.
fn toolchain(src: &Path) -> Result<String> {
    let mut answers = Vec::new();
    for (program, args) in [
        ("rustc", &["-V"][..]),
        ("cargo", &["-V"]),
        ("cargo", &["clippy", "-V"]),
        ("just", &["--version"]),
    ] {
        let (outcome, stdout) = proc::output(
            cargo::scrub_env(&mut Command::new(program))
                .args(args)
                .current_dir(src),
            Duration::from_secs(600),
        )?;
        answers.push(match stdout.lines().next() {
            Some(first) if outcome.success() => first.to_owned(),
            _ => format!("{program} unavailable ({outcome})"),
        });
    }
    Ok(answers.join("; "))
}

fn self_test(runner: &Runner, sha: &str, work: &Path, summary: &mut Summary) -> Result<()> {
    // Both controls share one target directory. Each archive's injected
    // `lib.rs` is newer than anything built before it, so cargo rebuilds it.
    let target = work.join("self-test-target");
    for control in &CONTROLS {
        let dir = work.join(format!("self-test-{}", control.name));
        let checked = runner.check(sha, &dir, Some(control), &target)?;
        let text = String::from_utf8_lossy(&fs::read(&checked.log)?).into_owned();
        let on_marker = marker_in_error(&text);
        summary.line(&format!(
            "self-test {}: {}{}\n    log: {}",
            control.name,
            checked.verdict,
            if on_marker {
                ", on the injected line"
            } else {
                ""
            },
            checked.log.display()
        ))?;
        if checked.verdict != Verdict::Fail(Fail::CompileError) || !on_marker {
            bail!(
                "self-test control {} did not fail with an error on the injected line; \
                 this instrument cannot be trusted",
                control.name
            );
        }
    }
    runner.drop_target(&target)?;
    summary.line("self-test: both controls failed with an error on the injected line")
}

/// Runs the per-commit check and prints one verdict per commit.
///
/// # Errors
///
/// Fails when the range cannot be resolved, a commit cannot be archived, or a
/// self-test control does not fail on its injected line.
pub fn run(args: &Args) -> Result<ExitCode> {
    let repo = Repo::discover()?;
    let head = repo.commit("HEAD")?;
    let base = repo.merge_base(&repo.commit(&args.base)?, &head)?;
    // `just check` in the working tree certifies the tip only when the tree
    // matches HEAD. Otherwise a file present on disk but not committed can
    // make it pass, so the tip is checked here as well.
    let dirty = repo.is_dirty()?;
    let commits = commits_to_check(repo.range(&base, &head)?, &head, args.include_tip || dirty);

    let work = workdir::resolve(args.work_dir.as_deref(), "percommit", &repo.checkouts()?)?;
    let runner = Runner {
        repo: &repo,
        keep_targets: args.keep_targets,
        deadline: Duration::from_secs(args.timeout_mins.saturating_mul(60)),
        justfile: repo.show(&head, "Justfile")?,
    };
    let mut summary = Summary::create(&work.join("summary.txt"))?;
    summary.line(&format!(
        "percommit: merge base {base} (with {}), HEAD {head}",
        args.base
    ))?;
    summary.line(&format!("work dir: {}", work.display()))?;
    summary.line("every commit is checked with HEAD's `clippy` recipe")?;
    if dirty {
        summary.line(
            "note: the working tree differs from HEAD, so `just check` there does not \
             certify HEAD; checking HEAD too",
        )?;
    }

    if args.self_test {
        self_test(&runner, &base, &work, &mut summary)?;
    }

    if commits.is_empty() {
        summary.line("no commits below the tip; nothing to check")?;
        return Ok(ExitCode::SUCCESS);
    }
    summary.line(&format!("checking {} commit(s), serially", commits.len()))?;

    let mut all_ok = true;
    for (i, sha) in commits.iter().enumerate() {
        let dir = work.join(sha);
        let target = dir.join("target");
        let checked = runner.check(sha, &dir, None, &target)?;
        runner.drop_target(&target)?;
        all_ok &= checked.verdict == Verdict::Ok;
        summary.line(&format!(
            "{} {}  {}\n    log: {}",
            &sha[..12],
            checked.verdict,
            repo.subject(sha)?,
            checked.log.display()
        ))?;
        // The toolchain is probed once per run, in the first commit's archive.
        if i == 0 {
            summary.line(&format!("    toolchain: {}", toolchain(&checked.src)?))?;
        }
    }
    summary.line(&format!("summary: {}", work.join("summary.txt").display()))?;
    Ok(if all_ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

// Every line goes to stdout and to the summary file as it is produced, so a
// run that is interrupted still leaves its verdicts behind.
struct Summary(File);

impl Summary {
    fn create(path: &Path) -> Result<Self> {
        Ok(Self(File::create_new(path)?))
    }

    fn line(&mut self, text: &str) -> Result<()> {
        println!("{text}");
        writeln!(self.0, "{text}")?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BUILT: &str = "    Checking selfie v0.1.0 (/x)\n    Finished `dev` profile\n";

    fn shas(names: &[&str]) -> Vec<String> {
        names.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn a_clean_build_that_exits_zero_is_ok() {
        assert_eq!(score(BUILT, Outcome::Exited(Some(0))), Verdict::Ok);
    }

    #[test]
    fn a_build_that_exits_non_zero_without_a_compile_error_fails() {
        let verdict = score(BUILT, Outcome::Exited(Some(1)));
        assert_eq!(verdict, Verdict::Fail(Fail::Exit(Outcome::Exited(Some(1)))));
    }

    #[test]
    fn a_zero_exit_without_the_build_line_never_ran() {
        let verdict = score("", Outcome::Exited(Some(0)));
        assert_eq!(verdict, Verdict::NeverRan(NeverRan::NoBuildLine));
    }

    #[test]
    fn a_timeout_never_ran_even_after_the_build_line() {
        let verdict = score(BUILT, Outcome::TimedOut);
        assert_eq!(verdict, Verdict::NeverRan(NeverRan::TimedOut));
    }

    #[test]
    fn a_compile_error_fails_whatever_the_exit_status() {
        let log = "    Checking selfie v0.1.0 (/x)\nerror: could not compile `selfie`\n";
        let verdict = score(log, Outcome::Exited(Some(0)));
        assert_eq!(verdict, Verdict::Fail(Fail::CompileError));
    }

    #[test]
    fn every_control_carries_the_marker() {
        assert!(CONTROLS.iter().all(|c| c.text.contains(MARKER)));
    }

    #[test]
    fn the_tip_is_left_out_unless_asked_for() {
        let range = shas(&["a", "b", "head"]);
        assert_eq!(
            commits_to_check(range.clone(), "head", false),
            shas(&["a", "b"])
        );
        assert_eq!(
            commits_to_check(range, "head", true),
            shas(&["a", "b", "head"])
        );
    }

    #[test]
    fn the_tip_is_checked_even_when_it_is_the_merge_base() {
        assert_eq!(commits_to_check(Vec::new(), "head", true), shas(&["head"]));
        assert!(commits_to_check(Vec::new(), "head", false).is_empty());
    }

    #[test]
    fn only_an_error_quoting_the_marker_proves_the_control() {
        let denied = "error: length comparison to zero\n  --> lib.rs:9:5\n   |\n\
                      9  |     v.len() == 0 // xtask-self-test-marker\n";
        assert!(marker_in_error(denied));
        let warned = "warning: length comparison to zero\n  --> lib.rs:9:5\n   |\n\
                      9  |     v.len() == 0 // xtask-self-test-marker\n\
                      error: could not compile `selfie-cli`\n";
        assert!(!marker_in_error(warned));
    }
}
