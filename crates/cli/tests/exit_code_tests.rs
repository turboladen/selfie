//! The exit code each command reports for a clean run, a finding, and a failure.
//!
//! Every assertion names the exact code. A test asserting only "non-zero" passes
//! for a finding (3) and a failure (1) alike, so it cannot tell the two apart.

pub mod common;

use common::{
    SELFIE_ENV, sandboxed_command, setup_default_test_config,
    setup_test_config_with_state_directory as sandbox,
};

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

// ── spec validate ───────────────────────────────────────────────────────────

#[test]
fn spec_validate_exits_clean_for_a_valid_spec() {
    let temp = sandbox();
    write_spec(
        &temp,
        "tool",
        &format!("name: tool\nenvironments:\n  {SELFIE_ENV}:\n    install: \"true\"\n"),
    );

    let (code, output) = run(&temp, &["spec", "validate", "tool"]);
    assert_eq!(code, Some(CLEAN), "{output}");
}

// Backticks draw a warning: the spec is usable, and the warning is the finding.
#[test]
fn spec_validate_exits_three_on_a_warning() {
    let temp = sandbox();
    write_spec(
        &temp,
        "tool",
        &format!("name: tool\nenvironments:\n  {SELFIE_ENV}:\n    install: \"echo `true`\"\n"),
    );

    let (code, output) = run(&temp, &["spec", "validate", "tool"]);
    assert_eq!(code, Some(FOUND), "{output}");

    let (code, output) = run(&temp, &["spec", "validate", "--all"]);
    assert_eq!(code, Some(FOUND), "{output}");
}

// An unparsable dotfiles/ spec that a packages/ spec of the same name shadows is
// reported as a warning, and a run that reports one is not clean.
#[test]
fn spec_validate_all_exits_three_over_a_shadowed_unparsable_spec() {
    let temp = sandbox();
    let dotfiles = temp.path().join("dotfiles");
    std::fs::create_dir_all(&dotfiles).unwrap();
    let config = temp.path().join(".config/selfie/config.yaml");
    let mut text = std::fs::read_to_string(&config).unwrap();
    text.push_str(&format!("dotfiles_directory: {}\n", dotfiles.display()));
    std::fs::write(config, text).unwrap();
    write_spec(
        &temp,
        "tool",
        &format!("name: tool\nenvironments:\n  {SELFIE_ENV}:\n    install: \"true\"\n"),
    );
    std::fs::write(dotfiles.join("tool.yaml"), "name: tool\nenvironments: [\n").unwrap();

    let (code, output) = run(&temp, &["spec", "validate", "--all"]);
    assert_eq!(code, Some(FOUND), "{output}");
    // The one validated spec is clean, and the summary does not count it as a
    // spec with warnings.
    assert!(
        output.contains("1 package(s) validated successfully"),
        "{output}"
    );
    assert!(output.contains("other warning(s)"), "{output}");
}

// A command-sourced dotfile draws a notice that apply will run commands. Every
// such spec carries one, so it must not count as a warning.
#[test]
fn spec_validate_exits_clean_over_a_notice_alone() {
    let temp = sandbox();
    write_spec(
        &temp,
        "tool",
        &format!(
            "name: tool\ndotfiles:\n  - command: \"echo value\"\n    target: \"~/.toolrc\"\nenvironments:\n  {SELFIE_ENV}:\n    install: \"true\"\n"
        ),
    );

    let (code, output) = run(&temp, &["spec", "validate", "tool"]);
    assert_eq!(code, Some(CLEAN), "{output}");
    assert!(
        output.contains("'selfie apply' executes 1 command(s)"),
        "the notice must still be shown: {output}"
    );
}

// An invalid homepage is an error found by validation itself, on a spec that
// parses.
#[test]
fn spec_validate_exits_one_on_an_error() {
    let temp = sandbox();
    write_spec(
        &temp,
        "tool",
        &format!(
            "name: tool\nhomepage: \"not a url\"\nenvironments:\n  {SELFIE_ENV}:\n    install: \"true\"\n"
        ),
    );

    let (code, output) = run(&temp, &["spec", "validate", "tool"]);
    assert_eq!(code, Some(FAILED), "{output}");
    assert!(output.contains("Invalid URL"), "{output}");
}

// A spec missing its install command does not parse, which is a failure before
// validation runs.
#[test]
fn spec_validate_exits_one_on_a_spec_that_does_not_parse() {
    let temp = sandbox();
    write_spec(
        &temp,
        "tool",
        &format!("name: tool\nenvironments:\n  {SELFIE_ENV}:\n    check: \"true\"\n"),
    );

    let (code, output) = run(&temp, &["spec", "validate", "tool"]);
    assert_eq!(code, Some(FAILED), "{output}");
}

// ── config validate ─────────────────────────────────────────────────────────

// The default config names no state directory, so no "not there yet" warning
// can make the clean case a finding.
#[test]
fn config_validate_exits_clean_for_a_clean_file() {
    let temp = setup_default_test_config();

    let (code, output) = run(&temp, &["config", "validate"]);
    assert_eq!(code, Some(CLEAN), "{output}");
    assert!(output.contains("Configuration is valid."), "{output}");
}

// An unknown top-level key is a warning: the file is usable, and says so.
#[test]
fn config_validate_exits_three_on_a_warning() {
    let temp = setup_default_test_config();
    let config = temp.path().join(".config/selfie/config.yaml");
    let mut text = std::fs::read_to_string(&config).unwrap();
    text.push_str("configs_directory: /tmp\n");
    std::fs::write(config, text).unwrap();

    let (code, output) = run(&temp, &["config", "validate"]);
    assert_eq!(code, Some(FOUND), "{output}");
    assert!(
        output.contains("Configuration is usable, with warnings."),
        "{output}"
    );
    assert!(!output.contains("Configuration is valid."), "{output}");
}

// ── spec create ─────────────────────────────────────────────────────────────

#[test]
fn spec_create_exits_clean_when_it_writes_the_spec() {
    let temp = sandbox();

    let (code, output) = run(&temp, &["spec", "create", "tool"]);
    assert_eq!(code, Some(CLEAN), "{output}");
    assert!(temp.path().join("packages/tool.yml").exists(), "{output}");
}

// Without a terminal the "already exists" menu cannot be answered, so the create
// declines. It wrote nothing, which a script must not read as success.
#[test]
fn spec_create_exits_one_when_it_declines() {
    let temp = sandbox();
    write_spec(
        &temp,
        "tool",
        &format!("name: tool\nenvironments:\n  {SELFIE_ENV}:\n    install: \"true\"\n"),
    );

    let (code, output) = run(&temp, &["spec", "create", "tool"]);
    assert_eq!(code, Some(FAILED), "{output}");
    assert!(output.contains("already exists"), "{output}");
}
