//! Reading cargo's output: whether a crate was actually built.

use std::path::Path;
use std::process::Command;

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
        .env_remove("CARGO_MAKEFLAGS")
        .env_remove("MAKEFLAGS")
        .env_remove("MFLAGS")
        .env("CARGO_TERM_COLOR", "never")
        // A quiet cargo prints no `Compiling` line, and every verdict here
        // depends on that line.
        .env("CARGO_TERM_QUIET", "false")
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
}
