//! Reading cargo's output: whether a crate was actually built, and which
//! tests ran.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail};
use regex::Regex;

use crate::proc::Outcome;

/// What a cargo log says about building one package.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Build {
    /// rustc reported an error, or cargo said it could not compile a crate.
    CompileError,
    /// No `Compiling <package> v` or `Checking <package> v` line appeared, so
    /// cargo never started the package and nothing about it was checked.
    NotStarted,
    /// Cargo started the package and reported no compile error.
    Built,
}

/// How one cargo invocation that builds `package` ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Run {
    /// Cargo built the package and exited 0.
    Built,
    /// rustc reported an error, or cargo said it could not compile a crate.
    CompileError,
    /// Cargo never started the package.
    NotStarted,
    /// The deadline passed first.
    TimedOut,
    /// Cargo built the package and then exited unsuccessfully.
    Failed(Outcome),
}

/// Classifies a cargo run from how it ended and what it logged.
pub fn classify(outcome: Outcome, log: &str, package: &str) -> Run {
    if outcome == Outcome::TimedOut {
        return Run::TimedOut;
    }
    match build(log, package) {
        Build::CompileError => Run::CompileError,
        Build::NotStarted => Run::NotStarted,
        Build::Built if outcome.success() => Run::Built,
        Build::Built => Run::Failed(outcome),
    }
}

/// Classifies `log` for `package`.
pub fn build(log: &str, package: &str) -> Build {
    // The compile error is looked for first. Cargo prints the `Compiling` line
    // when it starts a crate, so the line also appears above a build that fails.
    if log.contains("could not compile") || log.contains("error[E") {
        return Build::CompileError;
    }
    // Both verbs: `cargo test` and `cargo build` print `Compiling`, while
    // `cargo clippy` and `cargo check` print `Checking`. The trailing ` v`
    // keeps `selfie` from matching `selfie-cli`.
    let line = Regex::new(&format!(
        r"(?m)^\s*(Compiling|Checking) {} v",
        regex::escape(package)
    ))
    .expect("the pattern is built from an escaped name");
    if line.is_match(log) {
        Build::Built
    } else {
        Build::NotStarted
    }
}

/// Prepares `cmd` to run a nested cargo: removes the variables an outer
/// `cargo run` or rustup sets, so the nested cargo takes its target directory
/// and toolchain from the tree it runs in, and forces the plain, non-quiet
/// output the verdicts are read from.
pub fn scrub_env(cmd: &mut Command) -> &mut Command {
    cmd.env_remove("CARGO_TARGET_DIR")
        .env_remove("CARGO_BUILD_TARGET_DIR")
        .env_remove("RUSTUP_TOOLCHAIN")
        // A build target moves every artifact, dep-info included, under a
        // directory named for the triple.
        .env_remove("CARGO_BUILD_TARGET")
        .env_remove("CARGO_MAKEFLAGS")
        .env_remove("MAKEFLAGS")
        .env_remove("MFLAGS")
        .env("CARGO_TERM_COLOR", "never")
        // A quiet cargo prints no `Compiling` line, and every verdict here
        // depends on that line.
        .env("CARGO_TERM_QUIET", "false")
}

/// How libtest reported one test.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TestStatus {
    /// The test passed.
    Ok,
    /// The test failed.
    Failed,
    /// The test was ignored and did not run.
    Ignored,
}

/// Every `test <path> ... <status>` line in `log`, keyed by the test's path
/// exactly as libtest printed it: `module::name` for a test in a module, and
/// the bare name for one at the root of its target.
pub fn test_results(log: &str) -> HashMap<String, TestStatus> {
    let line = Regex::new(r"^test (\S+)(?: - should panic)? \.\.\. (ok|FAILED|ignored)")
        .expect("the pattern is a literal");
    let mut results = HashMap::new();
    // libtest prints every result line before `failures:`, then reprints each
    // failing test's captured output at column 0. That output can hold a line
    // reading `test result:` followed by a forged result, so reading stops at
    // the first `failures:`. Each run selects one test target, so no second
    // binary's results follow.
    for text in log.lines() {
        if text == "failures:" {
            break;
        }
        if let Some(c) = line.captures(text) {
            let status = match &c[2] {
                "ok" => TestStatus::Ok,
                "FAILED" => TestStatus::Failed,
                _ => TestStatus::Ignored,
            };
            results.insert(c[1].to_owned(), status);
        }
    }
    results
}

/// Every source file rustc recorded in the dep-info files under
/// `target_dir`'s `debug/deps`, or `<triple>/debug/deps` when a build target
/// is set, as rustc wrote each path: relative to the workspace root for a
/// workspace member, absolute otherwise.
///
/// # Errors
///
/// Fails when no such directory exists or one cannot be read.
pub fn compiled_sources(target_dir: &Path) -> Result<HashSet<String>> {
    // `build.target` in the tree's own cargo config sets a triple that
    // scrubbing the environment cannot remove, so both layouts are searched.
    let mut dirs = vec![target_dir.join("debug")];
    for entry in std::fs::read_dir(target_dir)
        .with_context(|| format!("could not read {}", target_dir.display()))?
    {
        dirs.push(entry?.path().join("debug"));
    }
    let mut sources = HashSet::new();
    let mut searched = false;
    for deps in dirs.iter().map(|d| d.join("deps")).filter(|d| d.is_dir()) {
        searched = true;
        for entry in std::fs::read_dir(&deps)
            .with_context(|| format!("could not read {}", deps.display()))?
        {
            let path = entry?.path();
            if path.extension().is_some_and(|e| e == "d") {
                let text = String::from_utf8_lossy(&std::fs::read(&path)?).into_owned();
                sources.extend(
                    text.split_whitespace()
                        .map(|token| token.trim_end_matches(':').to_owned()),
                );
            }
        }
    }
    if !searched {
        bail!("no debug/deps directory under {}", target_dir.display());
    }
    Ok(sources)
}

/// Points `cmd`'s cargo at `dir` for both its final and its intermediate
/// artifacts, so nothing from another build can be reused.
pub fn isolate<'a>(cmd: &'a mut Command, dir: &Path) -> &'a mut Command {
    // `build.build-dir` in a user's cargo config would otherwise move the
    // intermediate artifacts, dep-info included, to one directory shared by
    // every build, which is the stale-artifact hazard this crate exists to
    // avoid.
    cmd.env("CARGO_TARGET_DIR", dir)
        .env("CARGO_BUILD_BUILD_DIR", dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_compile_line_above_a_failure_is_still_a_failure() {
        let log = "   Compiling selfie v0.1.0 (/x)\nerror[E0308]: mismatched types\n\
                   error: could not compile `selfie` (lib) due to 1 previous error\n";
        assert_eq!(build(log, "selfie"), Build::CompileError);
    }

    #[test]
    fn a_clippy_denial_is_a_compile_error() {
        let log = "    Checking selfie v0.1.0 (/x)\nerror: length comparison to zero\n\
                   error: could not compile `selfie` (lib) due to 1 previous error\n";
        assert_eq!(build(log, "selfie"), Build::CompileError);
    }

    #[test]
    fn either_verb_counts_as_built() {
        assert_eq!(
            build("    Checking selfie v0.1.0 (/x)\n", "selfie"),
            Build::Built
        );
        assert_eq!(
            build("   Compiling selfie v0.1.0 (/x)\n", "selfie"),
            Build::Built
        );
    }

    #[test]
    fn a_log_without_the_line_never_started_the_package() {
        let log = "    Finished `dev` profile [unoptimized + debuginfo] target(s) in 0.1s\n";
        assert_eq!(build(log, "selfie"), Build::NotStarted);
        assert_eq!(build("", "selfie"), Build::NotStarted);
    }

    #[test]
    fn a_package_name_does_not_match_a_longer_one() {
        let log = "   Compiling selfie-cli v0.1.0 (/x)\n    Checking selfie-mcp v0.1.0 (/x)\n";
        assert_eq!(build(log, "selfie"), Build::NotStarted);
        assert_eq!(build(log, "selfie-cli"), Build::Built);
    }

    #[test]
    fn the_line_must_start_a_line() {
        let log = "note: Compiling selfie v0.1.0 was mentioned in passing\n";
        assert_eq!(build(log, "selfie"), Build::NotStarted);
    }

    #[test]
    fn test_lines_are_keyed_by_the_path_libtest_prints() {
        let log = "test track::tests::refuses_it ... FAILED\n\
                   test top_level ... ok\n\
                   test m::panics - should panic ... ok\n\
                   test m::skipped ... ignored\n\
                   ---- track::tests::refuses_it stdout ----\n";
        let results = test_results(log);
        assert_eq!(results.len(), 4);
        assert_eq!(results["track::tests::refuses_it"], TestStatus::Failed);
        assert_eq!(results["top_level"], TestStatus::Ok);
        assert_eq!(results["m::panics"], TestStatus::Ok);
        assert_eq!(results["m::skipped"], TestStatus::Ignored);
        // A bare name never stands in for a qualified one.
        assert!(!results.contains_key("refuses_it"));
    }

    #[test]
    fn a_test_line_must_start_a_line() {
        assert!(test_results("note: test a::b ... ok\n").is_empty());
    }

    fn dep_info(deps: &Path, text: &str) {
        std::fs::create_dir_all(deps).unwrap();
        std::fs::write(deps.join("x-1.d"), text).unwrap();
    }

    #[test]
    fn dep_info_is_found_under_a_target_triple() {
        let target = tempfile::tempdir().unwrap();
        let triple = target.path().join("aarch64-apple-darwin/debug/deps");
        dep_info(&triple, "/t/x-1: crates/selfie/src/a.rs\n");
        dep_info(&target.path().join("debug/deps"), "/t/x-1: src/b.rs\n");
        let sources = compiled_sources(target.path()).unwrap();
        assert!(sources.contains("crates/selfie/src/a.rs"));
        assert!(sources.contains("src/b.rs"));
    }

    #[test]
    fn a_target_without_dep_info_is_an_error() {
        let target = tempfile::tempdir().unwrap();
        std::fs::create_dir(target.path().join("release")).unwrap();
        assert!(compiled_sources(target.path()).is_err());
    }

    #[test]
    fn a_forged_summary_in_captured_output_does_not_end_the_failures_section() {
        let log = "test a::x ... FAILED\n\nfailures:\n\n---- a::x stdout ----\n\
                   test result: ok. 1 passed; 0 failed\n\
                   test a::x ... ok\n\nfailures:\n    a::x\n\n\
                   test result: FAILED. 0 passed; 1 failed\n";
        assert_eq!(test_results(log)["a::x"], TestStatus::Failed);
    }

    #[test]
    fn output_reprinted_under_failures_is_not_a_result() {
        let log = "test a::x ... FAILED\n\nfailures:\n\n---- a::x stdout ----\n\
                   test a::x ... ok\n\nfailures:\n    a::x\n\n\
                   test result: FAILED. 0 passed; 1 failed\n";
        assert_eq!(test_results(log)["a::x"], TestStatus::Failed);
    }
}
