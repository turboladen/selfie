//! `cargo xtask mutate`: apply each mutation in a spec in its own archive and
//! score the tests it names.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt;
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use similar::TextDiff;

use crate::cargo::{self, Run, TestStatus};
use crate::git::Repo;
use crate::proc::{self, Outcome};
use crate::workdir::{self, Summary};

/// Arguments for `cargo xtask mutate`.
#[derive(clap::Args)]
pub struct Args {
    /// The mutation spec, a TOML file. See the crate's README for its format.
    #[arg(required_unless_present = "self_test")]
    spec: Option<PathBuf>,
    /// Where archives, targets and logs go. Must be absolute, new or empty,
    /// and outside every checkout. Defaults to a new directory under `TMPDIR`.
    #[arg(long)]
    work_dir: Option<PathBuf>,
    /// Run only the mutations with these ids. An id not in the spec is an
    /// error.
    #[arg(long, value_name = "ID")]
    only: Vec<String>,
    /// Run the built-in controls, which mutate files of this crate at HEAD,
    /// and require each to score as designed.
    #[arg(long, conflicts_with_all = ["spec", "only"])]
    self_test: bool,
}

/// The contents of a spec file.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Spec {
    /// This names the commit that is archived and whose files the anchors
    /// come from.
    #[serde(default = "default_rev")]
    rev: String,
    #[serde(default = "default_build_timeout")]
    build_timeout_secs: u64,
    #[serde(default = "default_test_timeout")]
    test_timeout_secs: u64,
    mutation: Vec<Mutation>,
}

fn default_rev() -> String {
    "HEAD".into()
}

fn default_build_timeout() -> u64 {
    1800
}

fn default_test_timeout() -> u64 {
    600
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct Mutation {
    id: String,
    /// The mutated file's path is relative to the repository root.
    file: String,
    anchor: String,
    replacement: String,
    package: String,
    /// Cargo's selection of one test target: `["--lib"]`, or a flag such as
    /// `--test` followed by the target's name.
    target: Vec<String>,
    /// Each test is named by its path exactly as libtest prints it.
    tests: Vec<String>,
    #[serde(default)]
    expect: Expect,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Expect {
    #[default]
    Caught,
    Survived,
}

/// Why a mutation never ran, or ran without showing anything.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Why {
    NotPlainPath,
    NoTests,
    EmptyAnchor,
    ReplacementIsAnchor,
    AnchorCount(usize),
    Unreadable(String),
    BuildTimedOut,
    CompileError,
    NotStarted,
    BuildFailed(Outcome),
    NotCompiled,
    ArchiveDiffers,
    TestsTimedOut,
    EndedBefore(String, Outcome),
    DidNotRun(String),
    Ignored(String),
    Baseline(Box<Why>),
    BaselineTest(String, Option<TestStatus>),
    Error(String),
}

impl fmt::Display for Why {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Why::NotPlainPath => write!(f, "the file is not a plain relative path"),
            Why::NoTests => write!(f, "no tests named"),
            Why::EmptyAnchor => write!(f, "the anchor is empty"),
            Why::ReplacementIsAnchor => write!(f, "the replacement equals the anchor"),
            Why::AnchorCount(n) => write!(f, "the anchor matches {n} times in the committed file"),
            Why::Unreadable(e) => write!(f, "the committed file could not be read: {e}"),
            Why::BuildTimedOut => write!(f, "the build timed out"),
            Why::CompileError => write!(f, "compile error"),
            Why::NotStarted => write!(f, "cargo never started the package"),
            Why::BuildFailed(o) => write!(f, "the build failed ({o})"),
            Why::NotCompiled => write!(
                f,
                "the file is not compiled into this build, so its tests cannot reach it"
            ),
            Why::ArchiveDiffers => write!(f, "the archived file differs from the committed blob"),
            Why::TestsTimedOut => write!(f, "the tests timed out"),
            Why::EndedBefore(t, o) => {
                write!(f, "the test process ended ({o}) before reporting {t}")
            }
            Why::DidNotRun(t) => write!(f, "{t} did not run"),
            Why::Ignored(t) => write!(f, "{t} was ignored"),
            Why::Baseline(why) => write!(f, "baseline: {why}"),
            Why::BaselineTest(t, None) => write!(f, "baseline: {t} did not run"),
            Why::BaselineTest(t, Some(s)) => write!(f, "baseline: {t} is {s:?}, not Ok"),
            Why::Error(e) => write!(f, "{e}"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Verdict {
    Caught,
    Survived,
    Mixed(Vec<(String, TestStatus)>),
    NeverRan(Why),
}

impl Verdict {
    fn matches(&self, expect: Expect) -> bool {
        matches!(
            (self, expect),
            (Verdict::Caught, Expect::Caught) | (Verdict::Survived, Expect::Survived)
        )
    }
}

impl fmt::Display for Verdict {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Verdict::Caught => write!(f, "CAUGHT"),
            Verdict::Survived => write!(f, "SURVIVED"),
            Verdict::Mixed(statuses) => {
                let detail: Vec<String> = statuses
                    .iter()
                    .map(|(t, s)| format!("{t}: {s:?}"))
                    .collect();
                write!(f, "MIXED ({})", detail.join(", "))
            }
            Verdict::NeverRan(why) => write!(f, "NEVER-RAN ({why})"),
        }
    }
}

// A phase is the outcome of building one target and running tests in it.
enum Phase {
    NotBuilt(Why),
    TimedOut,
    Tested {
        results: HashMap<String, TestStatus>,
        outcome: Outcome,
    },
}

fn score(phase: &Phase, tests: &[String]) -> Verdict {
    let (results, outcome) = match phase {
        Phase::NotBuilt(why) => return Verdict::NeverRan(why.clone()),
        Phase::TimedOut => return Verdict::NeverRan(Why::TestsTimedOut),
        Phase::Tested { results, outcome } => (results, *outcome),
    };
    let mut statuses = Vec::new();
    for test in tests {
        match results.get(test) {
            // A test process that dies (a stack overflow, an abort) takes its
            // unfinished reports with it, so the named test cannot be credited
            // with the failure.
            None if !outcome.success() => {
                return Verdict::NeverRan(Why::EndedBefore(test.clone(), outcome));
            }
            None => return Verdict::NeverRan(Why::DidNotRun(test.clone())),
            Some(TestStatus::Ignored) => return Verdict::NeverRan(Why::Ignored(test.clone())),
            Some(status) => statuses.push((test.clone(), *status)),
        }
    }
    if statuses.iter().all(|(_, s)| *s == TestStatus::Failed) {
        Verdict::Caught
    } else if statuses.iter().all(|(_, s)| *s == TestStatus::Ok) {
        Verdict::Survived
    } else {
        Verdict::Mixed(statuses)
    }
}

// This counts every position the anchor starts at, overlapping ones included.
// `str::matches` counts only disjoint matches, which would accept an anchor
// that occurs twice.
fn occurrences(text: &str, anchor: &str) -> usize {
    let mut count = 0;
    let mut from = 0;
    while let Some(at) = text[from..].find(anchor) {
        count += 1;
        from += at + text[from + at..].chars().next().map_or(1, char::len_utf8);
    }
    count
}

// A prepared mutation passed every check that can be made before building.
struct Prepared {
    mutation: Mutation,
    committed: String,
}

// These refusals cost nothing, so they all run before the first build.
fn prepare(
    mutation: &Mutation,
    committed: impl FnOnce() -> Result<String>,
) -> Result<Prepared, Why> {
    if workdir::plain_relative(&mutation.file).is_none() {
        return Err(Why::NotPlainPath);
    }
    if mutation.tests.is_empty() {
        return Err(Why::NoTests);
    }
    if mutation.anchor.is_empty() {
        return Err(Why::EmptyAnchor);
    }
    if mutation.anchor == mutation.replacement {
        return Err(Why::ReplacementIsAnchor);
    }
    let committed = committed().map_err(|e| Why::Unreadable(format!("{e:#}")))?;
    let count = occurrences(&committed, &mutation.anchor);
    if count != 1 {
        return Err(Why::AnchorCount(count));
    }
    Ok(Prepared {
        mutation: mutation.clone(),
        committed,
    })
}

// A build selection is one package and its one test target.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct Selection {
    package: String,
    target: Vec<String>,
}

impl Selection {
    fn of(m: &Mutation) -> Self {
        Self {
            package: m.package.clone(),
            target: m.target.clone(),
        }
    }
}

// Each selection must name exactly one test binary. With several, libtest's
// reports would share test paths, and one binary's result could stand in for
// another's.
fn one_target(target: &[String]) -> bool {
    match target {
        [lib] => lib == "--lib",
        [flag, name] => {
            ["--bin", "--test", "--example", "--bench"].contains(&flag.as_str())
                && !name.is_empty()
                && !name.starts_with('-')
        }
        _ => false,
    }
}

struct Engine<'a> {
    repo: &'a Repo,
    rev: String,
    sha: String,
    work: PathBuf,
    build_deadline: Duration,
    test_deadline: Duration,
}

impl<'a> Engine<'a> {
    fn new(
        repo: &'a Repo,
        rev: &str,
        work: &Path,
        build_deadline: Duration,
        test_deadline: Duration,
    ) -> Result<Self> {
        Ok(Self {
            repo,
            rev: rev.to_owned(),
            sha: repo.commit(rev)?,
            work: work.to_path_buf(),
            build_deadline,
            test_deadline,
        })
    }

    fn cargo_test(src: &Path, target_dir: &Path, log: &Path, sel: &Selection) -> Result<Command> {
        let file = File::create(log)?;
        let mut cmd = Command::new("cargo");
        cargo::isolate(cargo::scrub_env(&mut cmd), target_dir)
            .current_dir(src)
            .args(["test", "--no-fail-fast", "-p", &sel.package])
            .args(&sel.target)
            .stdout(file.try_clone()?)
            .stderr(file);
        Ok(cmd)
    }

    // The target directory is removed however the phase ends, so an error
    // midway cannot leave a multi-gigabyte build behind.
    fn build_and_test(
        &self,
        src: &Path,
        dir: &Path,
        sel: &Selection,
        tests: &[String],
        mutated: Option<&str>,
    ) -> Result<Phase> {
        let target = dir.join("target");
        let phase = self.phase(src, dir, &target, sel, tests, mutated);
        if target.exists() {
            fs::remove_dir_all(&target)?;
        }
        phase
    }

    // The build runs with `--no-run` first, so a compile error is told apart
    // from a test failure and the test deadline covers only the tests. When
    // `mutated` names a file, the build must have compiled it, or the tests
    // could not have reached the mutation.
    fn phase(
        &self,
        src: &Path,
        dir: &Path,
        target: &Path,
        sel: &Selection,
        tests: &[String],
        mutated: Option<&str>,
    ) -> Result<Phase> {
        let build_log = dir.join("build.log");
        let outcome = proc::run(
            Self::cargo_test(src, target, &build_log, sel)?.arg("--no-run"),
            self.build_deadline,
        )?;
        let log = String::from_utf8_lossy(&fs::read(&build_log)?).into_owned();
        let why = match cargo::classify(outcome, &log, &sel.package) {
            Run::TimedOut => Why::BuildTimedOut,
            Run::CompileError => Why::CompileError,
            Run::NotStarted => Why::NotStarted,
            Run::Failed(outcome) => Why::BuildFailed(outcome),
            Run::Built => match mutated {
                Some(file) if !compiled(target, src, file)? => Why::NotCompiled,
                _ => return self.test(src, dir, target, sel, tests),
            },
        };
        Ok(Phase::NotBuilt(why))
    }

    fn test(
        &self,
        src: &Path,
        dir: &Path,
        target: &Path,
        sel: &Selection,
        tests: &[String],
    ) -> Result<Phase> {
        let test_log = dir.join("test.log");
        let outcome = proc::run(
            Self::cargo_test(src, target, &test_log, sel)?
                .arg("--")
                .arg("--exact")
                .args(tests),
            self.test_deadline,
        )?;
        if outcome == Outcome::TimedOut {
            return Ok(Phase::TimedOut);
        }
        let log = String::from_utf8_lossy(&fs::read(&test_log)?).into_owned();
        Ok(Phase::Tested {
            results: cargo::test_results(&log),
            outcome,
        })
    }

    fn archive(&self, dir: &Path) -> Result<PathBuf> {
        fs::create_dir_all(dir.parent().context("a run directory has a parent")?)?;
        fs::create_dir(dir).with_context(|| format!("could not create {}", dir.display()))?;
        let src = dir.join("src");
        self.repo.archive(&self.sha, &src)?;
        Ok(src)
    }

    fn baseline(
        &self,
        i: usize,
        sel: &Selection,
        tests: &[String],
        summary: &mut Summary,
    ) -> Result<Phase> {
        let dir = self.work.join("baselines").join(i.to_string());
        let src = self.archive(&dir)?;
        let phase = self.build_and_test(&src, &dir, sel, tests, None)?;
        summary.line(&format!(
            "baseline -p {} {}: {}\n    logs: {}",
            sel.package,
            sel.target.join(" "),
            describe_baseline(&phase, tests),
            dir.display()
        ))?;
        Ok(phase)
    }

    // A mutation's tests run as the same set its baseline ran, so the two
    // runs differ only by the mutation. It is scored on its own named tests.
    fn mutate(&self, p: &Prepared, dir: &Path, run_tests: &[String]) -> Result<Verdict> {
        let m = &p.mutation;
        let src = self.archive(dir)?;
        let path = src.join(&m.file);
        let archived = fs::read_to_string(&path)?;
        // `git archive` applies `.gitattributes` and line-ending settings that
        // `git show` does not, so the two can differ.
        if archived != p.committed {
            return Ok(Verdict::NeverRan(Why::ArchiveDiffers));
        }
        let mutated = archived.replacen(&m.anchor, &m.replacement, 1);
        fs::write(&path, &mutated)?;
        // The diff shows where the substitution landed, which a match count
        // alone does not.
        let diff = TextDiff::from_lines(&archived, &mutated)
            .unified_diff()
            .context_radius(3)
            .header(&format!("a/{}", m.file), &format!("b/{}", m.file))
            .to_string();
        fs::write(dir.join("mutation.diff"), diff)?;
        let sel = Selection::of(m);
        let phase = self.build_and_test(&src, dir, &sel, run_tests, Some(&m.file))?;
        Ok(score(&phase, &m.tests))
    }

    fn run(
        &self,
        mutations: &[Mutation],
        summary: &mut Summary,
        show_expect: bool,
    ) -> Result<Vec<Scored>> {
        summary.line(&format!(
            "mutate: {} mutation(s) against {} ({})",
            mutations.len(),
            self.rev,
            self.sha
        ))?;
        summary.line(&format!("work dir: {}", self.work.display()))?;
        if self.sha == self.repo.commit("HEAD")? && self.repo.is_dirty()? {
            summary.line(
                "note: the working tree differs from HEAD; the committed tree is what gets mutated",
            )?;
        }

        let mut scored = Vec::new();
        let mut prepared = Vec::new();
        for m in mutations {
            match prepare(m, || self.repo.show(&self.sha, &m.file)) {
                Ok(p) => prepared.push(p),
                Err(why) => {
                    let verdict = Verdict::NeverRan(why);
                    summary.line(&format!("{} {verdict}  (refused before building)", m.id))?;
                    scored.push(Scored::new(m, verdict));
                }
            }
        }

        let mut groups: BTreeMap<Selection, Vec<String>> = BTreeMap::new();
        for p in &prepared {
            let tests = groups.entry(Selection::of(&p.mutation)).or_default();
            for t in &p.mutation.tests {
                if !tests.contains(t) {
                    tests.push(t.clone());
                }
            }
        }
        let mut baselines = HashMap::new();
        for (i, (sel, tests)) in groups.iter().enumerate() {
            baselines.insert(sel.clone(), self.baseline(i, sel, tests, summary)?);
        }

        for p in &prepared {
            let m = &p.mutation;
            let sel = Selection::of(m);
            let dir = self.work.join("mutations").join(&m.id);
            let verdict = match baseline_refusal(&baselines[&sel], &m.tests) {
                Some(why) => Verdict::NeverRan(why),
                // One mutation's trouble is its own verdict, not the end of
                // the run, so the rest are still scored.
                None => self
                    .mutate(p, &dir, &groups[&sel])
                    .unwrap_or_else(|e| Verdict::NeverRan(Why::Error(format!("{e:#}")))),
            };
            let mut line = format!("{} {verdict}", m.id);
            if show_expect {
                line.push_str(&format!("  (expected {:?})", m.expect));
            }
            if dir.exists() {
                line.push_str(&format!("\n    logs: {}", dir.display()));
            }
            summary.line(&line)?;
            scored.push(Scored::new(m, verdict));
        }
        Ok(scored)
    }
}

// rustc writes a workspace member's sources relative to the workspace root
// and anything else as an absolute path, so both spellings are looked for.
fn compiled(target: &Path, src: &Path, file: &str) -> Result<bool> {
    let sources = cargo::compiled_sources(target)?;
    Ok(sources.contains(file) || sources.contains(&src.join(file).display().to_string()))
}

fn describe_baseline(phase: &Phase, tests: &[String]) -> String {
    match phase {
        Phase::NotBuilt(why) => format!("did not build ({why})"),
        Phase::TimedOut => "timed out".into(),
        Phase::Tested { results, .. } => {
            let passed = tests
                .iter()
                .filter(|t| results.get(*t) == Some(&TestStatus::Ok))
                .count();
            format!("{passed} of {} named tests passed", tests.len())
        }
    }
}

// This says why the baseline cannot vouch for `tests`, if it cannot.
fn baseline_refusal(phase: &Phase, tests: &[String]) -> Option<Why> {
    match phase {
        Phase::NotBuilt(why) => Some(Why::Baseline(Box::new(why.clone()))),
        Phase::TimedOut => Some(Why::Baseline(Box::new(Why::TestsTimedOut))),
        Phase::Tested { results, .. } => tests.iter().find_map(|t| match results.get(t) {
            Some(TestStatus::Ok) => None,
            status => Some(Why::BaselineTest(t.clone(), status.copied())),
        }),
    }
}

struct Scored {
    id: String,
    expect: Expect,
    verdict: Verdict,
}

impl Scored {
    fn new(m: &Mutation, verdict: Verdict) -> Self {
        Self {
            id: m.id.clone(),
            expect: m.expect,
            verdict,
        }
    }
}

// An id names the mutation's directory, so it must be one plain component.
fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && !id.starts_with('.')
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
}

fn load(path: &Path) -> Result<Spec> {
    let text =
        fs::read_to_string(path).with_context(|| format!("could not read {}", path.display()))?;
    let spec: Spec =
        toml::from_str(&text).with_context(|| format!("{} is not a valid spec", path.display()))?;
    if spec.build_timeout_secs == 0 || spec.test_timeout_secs == 0 {
        bail!("timeouts must be at least one second");
    }
    // Ids become directory names, and macOS file systems ignore case.
    let mut seen = HashSet::new();
    for m in &spec.mutation {
        if !valid_id(&m.id) {
            bail!(
                "mutation id {:?} must be letters, digits, `-`, `_` or `.`",
                m.id
            );
        }
        if !seen.insert(m.id.to_ascii_lowercase()) {
            bail!("mutation id {} appears twice, ignoring case", m.id);
        }
        if !one_target(&m.target) {
            bail!(
                "mutation {} must name one test target, such as [\"--lib\"] or \
                 [\"--test\", \"name\"]",
                m.id
            );
        }
    }
    Ok(spec)
}

fn select(spec: Spec, only: &[String]) -> Result<Vec<Mutation>> {
    if let Some(unknown) = only
        .iter()
        .find(|id| !spec.mutation.iter().any(|m| &m.id == *id))
    {
        bail!("--only {unknown}: no mutation has that id");
    }
    let chosen: Vec<Mutation> = spec
        .mutation
        .into_iter()
        .filter(|m| only.is_empty() || only.contains(&m.id))
        .collect();
    // A run that measured nothing must not report success.
    if chosen.is_empty() {
        bail!("the spec selects no mutations");
    }
    Ok(chosen)
}

/// Runs a mutation spec, or the self-test, and prints one verdict per mutation.
///
/// # Errors
///
/// Fails when the spec is invalid, a baseline cannot be archived, or the
/// self-test fails. A mutation that cannot run is scored NEVER-RAN instead.
pub fn run(args: &Args) -> Result<ExitCode> {
    let repo = Repo::discover()?;
    if args.self_test {
        let work = workdir::resolve(args.work_dir.as_deref(), "mutate", &repo.checkouts()?)?;
        let mut summary = Summary::create(&work.join("summary.txt"))?;
        return self_test(&repo, &work, &mut summary);
    }
    // The spec is read before the work directory exists, so a bad spec
    // leaves nothing behind.
    let spec = load(args.spec.as_deref().context("a spec file is required")?)?;
    let rev = spec.rev.clone();
    let deadlines = (
        Duration::from_secs(spec.build_timeout_secs),
        Duration::from_secs(spec.test_timeout_secs),
    );
    let mutations = select(spec, &args.only)?;
    let work = workdir::resolve(args.work_dir.as_deref(), "mutate", &repo.checkouts()?)?;
    let mut summary = Summary::create(&work.join("summary.txt"))?;
    let engine = Engine::new(&repo, &rev, &work, deadlines.0, deadlines.1)?;
    let scored = engine.run(&mutations, &mut summary, true)?;
    let mismatched: Vec<&Scored> = scored
        .iter()
        .filter(|s| !s.verdict.matches(s.expect))
        .collect();
    for s in &mismatched {
        summary.line(&format!(
            "MISMATCH {}: {} but expected {:?}",
            s.id, s.verdict, s.expect
        ))?;
    }
    summary.line(&format!(
        "{} of {} matched their expectation\nsummary: {}",
        scored.len() - mismatched.len(),
        scored.len(),
        work.join("summary.txt").display()
    ))?;
    Ok(if mismatched.is_empty() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

const CONTROL_FILE: &str = "crates/xtask/src/control.rs";

fn self_test(repo: &Repo, work: &Path, summary: &mut Summary) -> Result<ExitCode> {
    let pidfile = work.join("hang.pid");
    let control = |id: &str, anchor: &str, replacement: &str, test: &str| Mutation {
        id: id.into(),
        file: CONTROL_FILE.into(),
        anchor: anchor.into(),
        replacement: replacement.into(),
        package: "xtask".into(),
        target: vec!["--bin".into(), "xtask".into()],
        tests: vec![test.into()],
        expect: Expect::Caught,
    };
    let answer = "control::the_answer_is_42";
    let returns = "control::a_call_returns";
    let hang = format!(
        "fn returns_promptly() {{\n    let child = std::process::Command::new(\"sleep\").arg(\"600\").spawn().unwrap();\n    std::fs::write({:?}, child.id().to_string()).unwrap();\n    loop {{\n        std::thread::sleep(std::time::Duration::from_secs(60));\n    }}\n}}",
        pidfile.display().to_string()
    );
    // The README is part of this crate but never compiled.
    let outside = Mutation {
        file: "crates/xtask/README.md".into(),
        ..control(
            "file-outside-build",
            "# xtask\n",
            "# xtask, mutated\n",
            answer,
        )
    };
    // Each control carries a test of the verdict it must score. A control that
    // scores anything else means the runner cannot be trusted.
    type Check = fn(&Verdict) -> bool;
    let controls: Vec<(Mutation, Check)> = vec![
        (
            control("value-flip", "    42\n}", "    43\n}", answer),
            |v| *v == Verdict::Caught,
        ),
        (
            Mutation {
                expect: Expect::Survived,
                ..control(
                    "comment-only",
                    "// The comment-only control edits this line and nothing else.",
                    "// This comment was edited, and nothing else was.",
                    answer,
                )
            },
            |v| *v == Verdict::Survived,
        ),
        (
            control("anchor-missing", "    41\n}", "    43\n}", answer),
            |v| *v == Verdict::NeverRan(Why::AnchorCount(0)),
        ),
        (
            control("type-error", "    42\n}", "    \"forty-two\"\n}", answer),
            |v| *v == Verdict::NeverRan(Why::CompileError),
        ),
        (
            control("abort", "    42\n}", "    std::process::abort()\n}", answer),
            |v| matches!(v, Verdict::NeverRan(Why::EndedBefore(..))),
        ),
        (
            control("hang", "fn returns_promptly() {}", &hang, returns),
            |v| *v == Verdict::NeverRan(Why::TestsTimedOut),
        ),
        (
            control(
                "missing-test",
                "    42\n}",
                "    43\n}",
                "control::no_such_test",
            ),
            |v| matches!(v, Verdict::NeverRan(Why::BaselineTest(_, None))),
        ),
        (outside, |v| *v == Verdict::NeverRan(Why::NotCompiled)),
    ];
    let mutations: Vec<Mutation> = controls.iter().map(|(m, _)| m.clone()).collect();
    let engine = Engine::new(
        repo,
        "HEAD",
        work,
        Duration::from_secs(1800),
        Duration::from_secs(20),
    )?;
    let scored = engine.run(&mutations, summary, false)?;

    let mut failures = Vec::new();
    for (m, check) in &controls {
        match scored.iter().find(|s| s.id == m.id) {
            None => failures.push(format!("{}: never scored", m.id)),
            Some(got) if !check(&got.verdict) => {
                failures.push(format!("{}: scored {}", m.id, got.verdict));
            }
            Some(_) => {}
        }
    }
    let hung_pid = fs::read_to_string(&pidfile)
        .ok()
        .and_then(|p| p.trim().parse().ok());
    match hung_pid {
        Some(pid) if !proc::is_gone(pid) => {
            failures.push(format!(
                "hang: the test's child {pid} outlived the deadline"
            ));
        }
        Some(_) => {}
        None => failures.push("hang: the mutated test never wrote its child's pid".into()),
    }
    if failures.is_empty() {
        summary.line(&format!(
            "self-test: all {} controls scored as designed",
            controls.len()
        ))?;
        Ok(ExitCode::SUCCESS)
    } else {
        for f in &failures {
            summary.line(&format!("self-test FAILED {f}"))?;
        }
        bail!("the mutation runner's self-test failed; this instrument cannot be trusted")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SPEC: &str = r#"
[[mutation]]
id = "m1"
file = "crates/selfie/src/a.rs"
anchor = '''
fn a() {
    1
'''
replacement = '''
fn a() {
    2
'''
package = "selfie"
target = ["--lib"]
tests = ["a::tests::one"]
"#;

    fn spec(text: &str) -> Result<Spec> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("spec.toml");
        fs::write(&path, text)?;
        load(&path)
    }

    fn mutation() -> Mutation {
        spec(SPEC).unwrap().mutation.remove(0)
    }

    fn tested(pairs: &[(&str, TestStatus)], outcome: Outcome) -> Phase {
        Phase::Tested {
            results: pairs.iter().map(|(t, s)| ((*t).to_owned(), *s)).collect(),
            outcome,
        }
    }

    fn failed(pairs: &[(&str, TestStatus)]) -> Phase {
        tested(pairs, Outcome::Exited(Some(101)))
    }

    fn names(tests: &[&str]) -> Vec<String> {
        tests.iter().map(|t| (*t).to_owned()).collect()
    }

    #[test]
    fn a_spec_takes_defaults_and_keeps_multi_line_anchors() {
        let spec = spec(SPEC).unwrap();
        assert_eq!(spec.rev, "HEAD");
        assert_eq!(spec.test_timeout_secs, 600);
        let m = &spec.mutation[0];
        assert_eq!(m.anchor, "fn a() {\n    1\n");
        assert_eq!(m.expect, Expect::Caught);
    }

    #[test]
    fn a_spec_with_an_unknown_key_is_refused() {
        assert!(spec(&SPEC.replace("tests =", "test =")).is_err());
    }

    #[test]
    fn a_mutation_must_name_exactly_one_test_target() {
        for bad in [
            "[]",
            "[\"--tests\"]",
            "[\"--lib\", \"--bins\"]",
            "[\"--test\"]",
            "[\"--all-targets\"]",
        ] {
            let text = SPEC.replace("target = [\"--lib\"]", &format!("target = {bad}"));
            let err = spec(&text).unwrap_err();
            assert!(
                format!("{err:#}").contains("one test target"),
                "{bad}: {err:#}"
            );
        }
        assert!(one_target(&names(&["--test", "cli"])));
        assert!(one_target(&names(&["--lib"])));
    }

    #[test]
    fn a_zero_timeout_is_refused() {
        let text = format!("test_timeout_secs = 0\n{SPEC}");
        assert!(spec(&text).is_err());
    }

    #[test]
    fn an_id_must_be_one_plain_component() {
        for bad in ["../x", "a/b", ".hidden", ""] {
            let text = SPEC.replace("id = \"m1\"", &format!("id = {bad:?}"));
            assert!(spec(&text).is_err(), "{bad:?} was accepted");
        }
        assert!(valid_id("every-rule_2.b"));
        let twice = format!("{SPEC}{}", SPEC.replace("id = \"m1\"", "id = \"M1\""));
        let err = spec(&twice).unwrap_err();
        assert!(err.to_string().contains("ignoring case"), "{err}");
    }

    #[test]
    fn an_unknown_only_id_is_an_error() {
        let err = select(spec(SPEC).unwrap(), &names(&["m1", "m2"])).unwrap_err();
        assert!(err.to_string().contains("m2"), "{err}");
        let kept = select(spec(SPEC).unwrap(), &names(&["m1"])).unwrap();
        assert_eq!(kept.len(), 1);
    }

    #[test]
    fn an_empty_selection_is_an_error() {
        let empty = Spec {
            rev: default_rev(),
            build_timeout_secs: 1,
            test_timeout_secs: 1,
            mutation: Vec::new(),
        };
        assert!(select(empty, &[]).is_err());
    }

    #[test]
    fn a_path_that_climbs_out_is_refused() {
        let mut m = mutation();
        m.file = "crates/selfie/../cli/src/a.rs".into();
        assert_eq!(
            prepare(&m, || Ok(String::new())).err(),
            Some(Why::NotPlainPath)
        );
    }

    #[test]
    fn an_anchor_must_match_exactly_once() {
        let m = mutation();
        let once = "fn a() {\n    1\n}\n";
        assert!(prepare(&m, || Ok(once.into())).is_ok());
        let none = prepare(&m, || Ok(String::new())).err();
        assert_eq!(none, Some(Why::AnchorCount(0)));
        let twice = prepare(&m, || Ok(once.repeat(2))).err();
        assert_eq!(twice, Some(Why::AnchorCount(2)));
    }

    #[test]
    fn overlapping_occurrences_are_counted() {
        let text = "        }\n        }\n        }\n";
        assert_eq!(occurrences(text, "        }\n        }\n"), 2);
        assert_eq!(text.matches("        }\n        }\n").count(), 1);
        assert_eq!(occurrences("a\u{e9} a\u{e9}", "a\u{e9}"), 2);
    }

    #[test]
    fn every_named_test_failing_is_caught() {
        let phase = failed(&[("a::x", TestStatus::Failed), ("a::y", TestStatus::Failed)]);
        assert_eq!(score(&phase, &names(&["a::x", "a::y"])), Verdict::Caught);
    }

    #[test]
    fn every_named_test_passing_survived() {
        let phase = failed(&[("a::x", TestStatus::Ok), ("b::z", TestStatus::Failed)]);
        assert_eq!(score(&phase, &names(&["a::x"])), Verdict::Survived);
    }

    #[test]
    fn a_split_result_is_mixed() {
        let phase = failed(&[("a::x", TestStatus::Failed), ("a::y", TestStatus::Ok)]);
        assert!(matches!(
            score(&phase, &names(&["a::x", "a::y"])),
            Verdict::Mixed(_)
        ));
    }

    #[test]
    fn a_bare_name_does_not_match_the_qualified_test() {
        let phase = failed(&[("a::tests::x", TestStatus::Failed)]);
        let verdict = score(&phase, &names(&["x"]));
        let ended = Why::EndedBefore("x".into(), Outcome::Exited(Some(101)));
        assert_eq!(verdict, Verdict::NeverRan(ended));
        let passed = tested(&[("a::tests::x", TestStatus::Ok)], Outcome::Exited(Some(0)));
        let verdict = score(&passed, &names(&["x"]));
        assert_eq!(verdict, Verdict::NeverRan(Why::DidNotRun("x".into())));
    }

    #[test]
    fn an_ignored_test_never_ran() {
        let phase = failed(&[("a::x", TestStatus::Ignored)]);
        assert!(matches!(
            score(&phase, &names(&["a::x"])),
            Verdict::NeverRan(_)
        ));
    }

    #[test]
    fn a_test_process_that_died_before_reporting_never_ran() {
        // Another test reported before the process died, which must not
        // turn the named test's silence into a verdict.
        let died = tested(&[("a::other", TestStatus::Ok)], Outcome::Exited(None));
        let verdict = score(&died, &names(&["a::x"]));
        let ended = Why::EndedBefore("a::x".into(), Outcome::Exited(None));
        assert_eq!(verdict, Verdict::NeverRan(ended));
    }

    #[test]
    fn a_timeout_or_build_failure_never_ran() {
        assert!(matches!(
            score(&Phase::TimedOut, &names(&["a::x"])),
            Verdict::NeverRan(_)
        ));
        let phase = Phase::NotBuilt(Why::CompileError);
        assert_eq!(
            score(&phase, &names(&["a::x"])),
            Verdict::NeverRan(Why::CompileError)
        );
    }

    #[test]
    fn only_caught_and_survived_can_match() {
        assert!(Verdict::Caught.matches(Expect::Caught));
        assert!(Verdict::Survived.matches(Expect::Survived));
        assert!(!Verdict::Survived.matches(Expect::Caught));
        assert!(!Verdict::NeverRan(Why::NoTests).matches(Expect::Survived));
        assert!(!Verdict::Mixed(Vec::new()).matches(Expect::Caught));
    }

    #[test]
    fn a_baseline_vouches_only_for_passing_tests() {
        let phase = failed(&[("a::x", TestStatus::Ok), ("a::y", TestStatus::Failed)]);
        assert_eq!(baseline_refusal(&phase, &names(&["a::x"])), None);
        assert!(baseline_refusal(&phase, &names(&["a::y"])).is_some());
        assert!(baseline_refusal(&phase, &names(&["a::z"])).is_some());
    }

    #[test]
    fn a_file_counts_as_compiled_in_either_spelling() {
        let target = tempfile::tempdir().unwrap();
        let deps = target.path().join("debug").join("deps");
        fs::create_dir_all(&deps).unwrap();
        fs::write(
            deps.join("x-1.d"),
            "/t/x-1: crates/selfie/src/a.rs /abs/dep/src/lib.rs\n\ncrates/selfie/src/a.rs:\n",
        )
        .unwrap();
        let src = Path::new("/abs/dep");
        assert!(compiled(target.path(), src, "crates/selfie/src/a.rs").unwrap());
        assert!(compiled(target.path(), src, "src/lib.rs").unwrap());
        assert!(!compiled(target.path(), src, "crates/selfie/src/b.rs").unwrap());
    }
}
