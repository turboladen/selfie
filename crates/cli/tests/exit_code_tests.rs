//! The exit code each command reports for a clean run, a finding, and a failure.
//!
//! Every assertion names the exact code. A test asserting only "non-zero" passes
//! for a finding (3) and a failure (1) alike, so it cannot tell the two apart.

pub mod common;

use common::{SELFIE_ENV, sandboxed_command, setup_test_config_with_state_directory as sandbox};

const CLEAN: i32 = 0;
const FAILED: i32 = 1;
const FOUND: i32 = 3;

fn write_spec(temp: &tempfile::TempDir, name: &str, yaml: &str) {
    let packages = temp.path().join("packages");
    std::fs::create_dir_all(&packages).unwrap();
    std::fs::write(packages.join(format!("{name}.yaml")), yaml).unwrap();
}

// A package deploying `rc` to `target`, with inert commands.
fn write_dotfile_package(temp: &tempfile::TempDir, name: &str, target: &str) {
    let source_dir = temp.path().join("packages").join(name);
    std::fs::create_dir_all(&source_dir).unwrap();
    std::fs::write(source_dir.join("rc"), "REPO\n").unwrap();
    write_spec(
        temp,
        name,
        &format!(
            "name: {name}\ndotfiles:\n  - source: {name}/rc\n    target: \"{target}\"\nenvironments:\n  {SELFIE_ENV}:\n    install: \"true\"\n"
        ),
    );
}

// A package whose one dotfile entry carries a key selfie does not know, which
// apply and drift both refuse.
fn write_refused_package(temp: &tempfile::TempDir) {
    let source_dir = temp.path().join("packages/refused");
    std::fs::create_dir_all(&source_dir).unwrap();
    std::fs::write(source_dir.join("rc"), "REPO\n").unwrap();
    write_spec(
        temp,
        "refused",
        &format!(
            "name: refused\ndotfiles:\n  - source: refused/rc\n    target: \"~/.refusedrc\"\n    audt: \"600\"\nenvironments:\n  {SELFIE_ENV}:\n    install: \"true\"\n"
        ),
    );
}

fn write_audited_package(temp: &tempfile::TempDir, name: &str, audit: &str) {
    write_spec(
        temp,
        name,
        &format!(
            "name: {name}\nenvironments:\n  {SELFIE_ENV}:\n    install: \"true\"\n    audit: \"{audit}\"\n"
        ),
    );
}

fn run(temp: &tempfile::TempDir, args: &[&str]) -> (Option<i32>, String) {
    let output = sandboxed_command(temp).args(args).output().unwrap();
    (
        output.status.code(),
        format!(
            "stdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        ),
    )
}

// Deploys `app` to `~/.apprc` and then edits the deployed file.
fn drifted() -> tempfile::TempDir {
    let temp = sandbox();
    write_dotfile_package(&temp, "app", "~/.apprc");
    let (code, output) = run(&temp, &["apply", "-y"]);
    assert_eq!(code, Some(CLEAN), "{output}");
    std::fs::write(temp.path().join(".apprc"), "EDITED\n").unwrap();
    temp
}

// ── dotfiles drift ──────────────────────────────────────────────────────────

#[test]
fn drift_exits_clean_when_nothing_drifted() {
    let temp = sandbox();
    write_dotfile_package(&temp, "app", "~/.apprc");
    let (code, output) = run(&temp, &["apply", "-y"]);
    assert_eq!(code, Some(CLEAN), "{output}");

    let (code, output) = run(&temp, &["dotfiles", "drift"]);
    assert_eq!(code, Some(CLEAN), "{output}");
}

#[test]
fn drift_exits_three_when_a_target_drifted() {
    let temp = drifted();

    let (code, output) = run(&temp, &["dotfiles", "drift"]);
    assert_eq!(code, Some(FOUND), "{output}");
    assert!(output.contains("1 drifted"), "{output}");
}

#[test]
fn drift_exits_one_when_it_refused_an_entry() {
    let temp = sandbox();
    write_refused_package(&temp);

    let (code, output) = run(&temp, &["dotfiles", "drift"]);
    assert_eq!(code, Some(FAILED), "{output}");
}

// A refusal outranks a finding: the answer has a hole in it, whatever else it
// found.
#[test]
fn drift_exits_one_when_it_refused_an_entry_and_found_drift() {
    let temp = drifted();
    write_refused_package(&temp);

    let (code, output) = run(&temp, &["dotfiles", "drift"]);
    assert_eq!(code, Some(FAILED), "{output}");
    assert!(output.contains("1 drifted"), "{output}");
}

// ── apply ───────────────────────────────────────────────────────────────────

// The README promises a conflict does not make apply exit non-zero: apply is
// asked to deploy, and leaving a target for the user to decide about is part of
// that job.
#[test]
fn apply_exits_clean_over_a_conflict_it_left_alone() {
    let temp = sandbox();
    write_dotfile_package(&temp, "app", "~/.apprc");
    std::fs::write(temp.path().join(".apprc"), "MINE\n").unwrap();

    let (code, output) = run(&temp, &["apply"]);
    assert_eq!(code, Some(CLEAN), "{output}");
    assert!(output.contains("1 conflict(s)"), "{output}");
    assert_eq!(
        std::fs::read_to_string(temp.path().join(".apprc")).unwrap(),
        "MINE\n",
        "the conflicting target must be left alone"
    );
}

#[test]
fn apply_exits_one_when_it_refused_an_entry() {
    let temp = sandbox();
    write_refused_package(&temp);

    let (code, output) = run(&temp, &["apply", "-y"]);
    assert_eq!(code, Some(FAILED), "{output}");
}

// ── package audit ───────────────────────────────────────────────────────────

#[test]
fn audit_exits_clean_when_the_only_source_is_the_package() {
    let temp = sandbox();
    write_audited_package(&temp, "tool", "echo tool");

    let (code, output) = run(&temp, &["package", "audit", "tool"]);
    assert_eq!(code, Some(CLEAN), "{output}");
}

#[test]
fn audit_exits_three_on_a_conflict() {
    let temp = sandbox();
    write_audited_package(&temp, "tool", "echo other-manager");

    let (code, output) = run(&temp, &["package", "audit", "tool"]);
    assert_eq!(code, Some(FOUND), "{output}");
}

#[test]
fn audit_exits_three_when_nothing_provides_the_package() {
    let temp = sandbox();
    write_audited_package(&temp, "tool", "true");

    let (code, output) = run(&temp, &["package", "audit", "tool"]);
    assert_eq!(code, Some(FOUND), "{output}");
}

#[test]
fn audit_exits_one_when_its_command_fails() {
    let temp = sandbox();
    write_audited_package(&temp, "tool", "false");

    let (code, output) = run(&temp, &["package", "audit", "tool"]);
    assert_eq!(code, Some(FAILED), "{output}");
}

#[test]
fn audit_all_exits_clean_when_every_audit_is_clean() {
    let temp = sandbox();
    write_audited_package(&temp, "tool", "echo tool");
    // No audit command: ordinary across many packages, so it counts for nothing.
    write_spec(
        &temp,
        "quiet",
        &format!("name: quiet\nenvironments:\n  {SELFIE_ENV}:\n    install: \"true\"\n"),
    );

    let (code, output) = run(&temp, &["package", "audit", "--all"]);
    assert_eq!(code, Some(CLEAN), "{output}");
}

#[test]
fn audit_all_exits_three_when_one_package_has_a_conflict() {
    let temp = sandbox();
    write_audited_package(&temp, "tool", "echo tool");
    write_audited_package(&temp, "other", "echo other-manager");

    let (code, output) = run(&temp, &["package", "audit", "--all"]);
    assert_eq!(code, Some(FOUND), "{output}");
    // The run says what it concluded, not only the per-package lines.
    assert!(output.contains("1 with conflicts"), "{output}");
}

// A spec it could not load is a package it could not audit, so the answer has a
// hole in it even though the rest found a conflict.
#[test]
fn audit_all_exits_one_over_a_spec_it_could_not_load() {
    let temp = sandbox();
    write_audited_package(&temp, "other", "echo other-manager");
    write_spec(&temp, "broken", "name: broken\nenvironments: [\n");

    let (code, output) = run(&temp, &["package", "audit", "--all"]);
    assert_eq!(code, Some(FAILED), "{output}");
}

// Like `package check` with no check command: nothing can answer the question.
#[test]
fn audit_exits_one_without_an_audit_command() {
    let temp = sandbox();
    write_spec(
        &temp,
        "tool",
        &format!("name: tool\nenvironments:\n  {SELFIE_ENV}:\n    install: \"true\"\n"),
    );

    let (code, output) = run(&temp, &["package", "audit", "tool"]);
    assert_eq!(code, Some(FAILED), "{output}");
}

// A package with no entry for this environment. The audit and check handlers
// render this failure themselves, so it reaches the exit code only because the
// verdict is taken before any handler sees the event.
#[test]
fn audit_exits_one_for_a_package_not_declared_here() {
    let temp = sandbox();
    write_spec(
        &temp,
        "elsewhere",
        "name: elsewhere\nenvironments:\n  other-env:\n    install: \"true\"\n    audit: \"echo elsewhere\"\n",
    );

    let (code, output) = run(&temp, &["package", "audit", "elsewhere"]);
    assert_eq!(code, Some(FAILED), "{output}");
}

// ── package check ───────────────────────────────────────────────────────────

fn write_checked_package(temp: &tempfile::TempDir, check: &str) {
    write_spec(
        temp,
        "tool",
        &format!(
            "name: tool\nenvironments:\n  {SELFIE_ENV}:\n    install: \"true\"\n    check: \"{check}\"\n"
        ),
    );
}

#[test]
fn check_exits_clean_when_installed() {
    let temp = sandbox();
    write_checked_package(&temp, "true");

    let (code, output) = run(&temp, &["package", "check", "tool"]);
    assert_eq!(code, Some(CLEAN), "{output}");
}

#[test]
fn check_exits_three_when_not_installed() {
    let temp = sandbox();
    write_checked_package(&temp, "false");

    let (code, output) = run(&temp, &["package", "check", "tool"]);
    assert_eq!(code, Some(FOUND), "{output}");
}

// A check that exits 127, as `tool --version` does when tool is missing, has
// answered: not installed.
#[test]
fn check_exits_three_when_its_command_is_missing() {
    let temp = sandbox();
    write_checked_package(&temp, "exit 127");

    let (code, output) = run(&temp, &["package", "check", "tool"]);
    assert_eq!(code, Some(FOUND), "{output}");
}

// A check killed by a signal exited non-zero like any other: not installed. The
// shell kills itself, so nothing outside the check is touched.
#[test]
fn check_exits_three_when_its_command_is_killed() {
    let temp = sandbox();
    write_checked_package(&temp, "kill -TERM $$");

    let (code, output) = run(&temp, &["package", "check", "tool"]);
    assert_eq!(code, Some(FOUND), "{output}");
}

#[test]
fn check_exits_one_without_a_check_command() {
    let temp = sandbox();
    write_spec(
        &temp,
        "tool",
        &format!("name: tool\nenvironments:\n  {SELFIE_ENV}:\n    install: \"true\"\n"),
    );

    let (code, output) = run(&temp, &["package", "check", "tool"]);
    assert_eq!(code, Some(FAILED), "{output}");
}

#[test]
fn check_exits_one_for_a_package_not_declared_here() {
    let temp = sandbox();
    write_spec(
        &temp,
        "elsewhere",
        "name: elsewhere\nenvironments:\n  other-env:\n    install: \"true\"\n    check: \"true\"\n",
    );

    let (code, output) = run(&temp, &["package", "check", "elsewhere"]);
    assert_eq!(code, Some(FAILED), "{output}");
}
