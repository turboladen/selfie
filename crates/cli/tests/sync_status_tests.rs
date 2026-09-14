pub mod common;

use std::fs;
use std::process::Command as StdCommand;

use common::{sandboxed_command, setup_default_test_config};

// A local `git` invocation against the sandbox's package directory. Config is
// set per-repo rather than globally, so the test commits without touching the
// developer's own git identity or signing key.
fn git(dir: &std::path::Path, args: &[&str]) {
    let status = StdCommand::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .status()
        .unwrap();
    assert!(status.success(), "git {args:?} failed in {dir:?}");
}

fn sandbox_with_one_unparsable_spec_in_a_git_repo() -> tempfile::TempDir {
    let temp_dir = setup_default_test_config();
    let packages_dir = temp_dir.path().join("packages");
    fs::write(packages_dir.join("broken.yml"), "environments: {oops\n").unwrap();

    git(temp_dir.path(), &["init", "-q", "-b", "main"]);
    git(temp_dir.path(), &["config", "user.email", "t@t.example"]);
    git(temp_dir.path(), &["config", "user.name", "t"]);
    git(temp_dir.path(), &["config", "commit.gpgsign", "false"]);
    git(temp_dir.path(), &["add", "-A"]);
    git(temp_dir.path(), &["commit", "-q", "-m", "init"]);

    temp_dir
}

// Reproduces the defect Copilot found on this branch: `sync status` printed
// "No dotfile drift" directly under the warning naming a spec it could not
// load. The unit tests on `no_drift_line` cover the renderer's gate in
// isolation; this drives the real binary so a wrong gate cannot pass by
// itself.
#[test]
fn sync_status_does_not_report_success_over_a_spec_it_could_not_load() {
    let temp_dir = sandbox_with_one_unparsable_spec_in_a_git_repo();

    let output = sandboxed_command(&temp_dir)
        .args(["sync", "status"])
        .output()
        .unwrap();
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );

    assert!(
        text.contains("broken.yml"),
        "must name the spec it could not load: {text}"
    );
    assert!(
        !text.contains("No dotfile drift"),
        "must not claim a clean repository over an unloaded spec: {text}"
    );
    // The drift check is non-fatal by design -- `sync status` still reports
    // git status when drift cannot be fully checked -- so an unloaded spec
    // does not fail the command. This asserts the exit code selfie actually
    // returns rather than a code chosen to fit an assumption.
    assert_eq!(output.status.code(), Some(0), "output was: {text}");
}
