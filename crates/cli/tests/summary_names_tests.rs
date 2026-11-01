pub mod common;

// A command's success summary names the environment as the header and the
// table of the same run do: escaped, so one name has one spelling. Each
// fixture's environment holds a character a name escapes and a sentence
// shown as prose does not.

use std::fs;

use common::{RunOutput, run_sandboxed};
use tempfile::TempDir;

// The environment, as YAML spells it and as a name shows it.
const SPELLED: &str = "a\\u00A0b";
const SHOWN: &str = r"a\u{a0}b";

// A sandbox whose config and one spec use `environment`, as YAML spells it.
// The spec's commands are inert, and it deploys one dotfile.
fn sandbox(environment: &str) -> TempDir {
    let temp = common::setup_test_config("");
    let home = temp.path();
    let packages = home.join("packages");
    fs::create_dir_all(home.join("dotfiles")).unwrap();
    fs::create_dir_all(home.join("state")).unwrap();
    fs::write(packages.join("tool.conf"), "x = 1\n").unwrap();
    fs::write(
        packages.join("tool.yml"),
        format!(
            "name: tool\ndotfiles:\n  - source: tool.conf\n    target: \"~/.toolrc\"\n\
             environments:\n  \"{environment}\":\n    install: \"true\"\n    check: \"true\"\n    \
             audit: \"echo brew\"\n"
        ),
    )
    .unwrap();
    fs::write(
        home.join(".config/selfie/config.yaml"),
        format!(
            "environment: \"{environment}\"\npackage_directory: {}\nstate_directory: {}\n",
            packages.display(),
            home.join("state").display()
        ),
    )
    .unwrap();
    temp
}

fn run(temp: &TempDir, args: &[&str]) -> RunOutput {
    let mut all = vec!["--no-color"];
    all.extend_from_slice(args);
    run_sandboxed(temp, &all)
}

// Asserts that `args` prints a summary line holding `summary` that names the
// environment escaped, and that no line anywhere holds a raw U+00A0.
fn assert_summary_names_the_environment_escaped(args: &[&str], summary: &str) {
    let temp = sandbox(SPELLED);
    let output = run(&temp, args);
    let both = format!("{}{}", output.stdout, output.stderr);

    assert!(!both.contains('\u{A0}'), "{args:?}: a raw U+00A0: {both:?}");
    let summaries: Vec<&str> = both.lines().filter(|line| line.contains(summary)).collect();
    assert_eq!(summaries.len(), 1, "{args:?}: one summary line: {both}");
    assert!(
        summaries[0].contains(&format!("environment '{SHOWN}'")),
        "{args:?}: the summary must name the environment escaped: {both}"
    );
}

#[test]
fn spec_list_names_the_environment_escaped() {
    assert_summary_names_the_environment_escaped(&["spec", "list"], "Spec listing completed");
}

#[test]
fn spec_validate_names_the_environment_escaped() {
    assert_summary_names_the_environment_escaped(
        &["spec", "validate", "tool"],
        "validation completed",
    );
}

#[test]
fn spec_validate_all_names_the_environment_escaped() {
    assert_summary_names_the_environment_escaped(
        &["spec", "validate", "--all"],
        "Spec validation completed",
    );
}

#[test]
fn package_check_names_the_environment_escaped() {
    assert_summary_names_the_environment_escaped(&["package", "check", "tool"], "check completed");
}

#[test]
fn package_audit_names_the_environment_escaped() {
    assert_summary_names_the_environment_escaped(&["package", "audit", "tool"], "audit completed");
}

#[test]
fn package_audit_all_names_the_environment_escaped() {
    assert_summary_names_the_environment_escaped(
        &["package", "audit", "--all"],
        "Audit completed for",
    );
}

#[test]
fn package_install_names_the_environment_escaped() {
    assert_summary_names_the_environment_escaped(
        &["package", "install", "tool"],
        "was already installed",
    );
}

#[test]
fn package_list_names_the_environment_escaped() {
    assert_summary_names_the_environment_escaped(&["package", "list"], "Package listing completed");
}

#[test]
fn dotfiles_drift_names_the_environment_escaped() {
    assert_summary_names_the_environment_escaped(&["dotfiles", "drift"], "Dotfile drift check");
}

#[test]
fn apply_names_the_environment_escaped() {
    assert_summary_names_the_environment_escaped(&["apply"], "Dotfiles applied");
}

// The table and the summary of one listing spell the environment the same
// way for a line break and for U+200B, and a line break in it does not split
// the summary.
#[test]
fn spec_list_spells_the_environment_once_in_table_and_summary() {
    for (spelled, raw, shown) in [
        ("a\\nb", '\n', r"a\nb"),
        ("a\\u200Bb", '\u{200B}', r"a\u{200b}b"),
    ] {
        let temp = sandbox(spelled);
        let output = run(&temp, &["spec", "list"]);
        let both = format!("{}{}", output.stdout, output.stderr);

        assert!(
            !both.contains(&format!("a{raw}b")),
            "{spelled}: raw in {both:?}"
        );
        let summary = both
            .lines()
            .find(|line| line.contains("Spec listing completed"))
            .unwrap_or_else(|| panic!("{spelled}: no summary: {both}"));
        assert!(
            summary.ends_with(&format!("in environment '{shown}'")),
            "{spelled}: {summary:?}"
        );
        // The table's cell, the count line and the summary.
        assert_eq!(
            both.lines().filter(|line| line.contains(shown)).count(),
            3,
            "{spelled}: {both}"
        );
    }
}

// A path in a summary is a path: a line feed in the name of the file
// `dotfiles track` tracks shows escaped, and the summary stays one line.
#[test]
fn dotfiles_track_names_the_tracked_path_escaped() {
    let temp = sandbox(SPELLED);
    fs::write(temp.path().join(".a\nb"), "x\n").unwrap();
    let output = run(&temp, &["dotfiles", "track", "rcspec", "~/.a\nb"]);
    let both = format!("{}{}", output.stdout, output.stderr);

    let summaries: Vec<&str> = both
        .lines()
        .filter(|line| line.contains("Now tracking"))
        .collect();
    assert_eq!(summaries.len(), 1, "one summary line: {both}");
    assert!(
        summaries[0].ends_with(r"Now tracking '~/.a\nb' in spec 'rcspec'"),
        "{both}"
    );
    assert_eq!(both.matches(r"~/.a\nb").count(), 1, "{both}");
}
