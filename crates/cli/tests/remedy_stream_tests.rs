pub mod common;

use std::fs;

use common::{SELFIE_ENV, package_repo_with_remote, run_sandboxed, setup_default_test_config};

// A failure's explanation and its remedy are about the run, so they go to
// stderr together, and a failing command leaves stdout empty. Advice that
// follows an answer goes to stderr too, while the answer stays on stdout.

fn write_spec(temp_dir: &tempfile::TempDir, name: &str, body: &str) {
    fs::write(
        temp_dir
            .path()
            .join("packages")
            .join(format!("{name}.yaml")),
        body,
    )
    .unwrap();
}

#[test]
fn an_unsupported_environment_is_explained_on_stderr_alone() {
    let temp_dir = setup_default_test_config();
    write_spec(
        &temp_dir,
        "elsewhere",
        "name: elsewhere\nenvironments:\n  other-env:\n    install: \"true\"\n    check: \"true\"\n    audit: \"echo brew\"\n",
    );

    for command in ["check", "audit"] {
        let out = run_sandboxed(&temp_dir, &["package", command, "elsewhere"]);

        assert_eq!(out.code, Some(1), "{command}: {}", out.stderr);
        assert_eq!(out.stdout, "", "{command}");
        for needle in [
            "doesn't support environment 'test-env'",
            "Available environments",
            "other-env",
            "Suggestion",
        ] {
            assert!(
                out.stderr.contains(needle),
                "{command}: {needle}\n{}",
                out.stderr
            );
        }
    }
}

// A package whose environment is declared but has no check command is not an
// unsupported environment, and is not called one.
#[test]
fn a_missing_check_command_is_named_as_such() {
    let temp_dir = setup_default_test_config();
    write_spec(
        &temp_dir,
        "nocheck",
        &format!(
            "name: nocheck\nenvironments:\n  {SELFIE_ENV}:\n    install: \"true\"\n  other-env:\n    install: \"true\"\n    check: \"true\"\n"
        ),
    );

    // At any verbosity: `-v` shows result cards, and a failure has none.
    let verbose = run_sandboxed(&temp_dir, &["-v", "package", "check", "nocheck"]);
    assert_eq!(verbose.code, Some(1), "{}", verbose.stderr);
    assert_eq!(verbose.stdout, "");

    let out = run_sandboxed(&temp_dir, &["package", "check", "nocheck"]);

    assert_eq!(out.code, Some(1), "{}", out.stderr);
    assert_eq!(out.stdout, "");
    assert!(
        out.stderr.contains("No check command defined"),
        "{}",
        out.stderr
    );
    assert!(
        out.stderr
            .contains("Environments with check commands: other-env"),
        "{}",
        out.stderr
    );
    assert!(!out.stderr.contains("doesn't support"), "{}", out.stderr);
}

#[test]
fn a_missing_editor_is_explained_on_stderr_alone() {
    let temp_dir = setup_default_test_config();
    write_spec(
        &temp_dir,
        "bat",
        &format!("name: bat\nenvironments:\n  {SELFIE_ENV}:\n    install: \"true\"\n"),
    );

    let out = run_sandboxed(&temp_dir, &["spec", "edit", "bat"]);

    assert_eq!(out.code, Some(1), "{}", out.stderr);
    assert_eq!(out.stdout, "");
    assert!(
        out.stderr
            .contains("EDITOR environment variable is not set"),
        "{}",
        out.stderr
    );
    assert!(out.stderr.contains("Suggestion"), "{}", out.stderr);
}

// Removing a package another depends on succeeds, and warns, with its advice,
// on stderr.
#[test]
fn a_broken_dependency_warning_keeps_its_advice() {
    let temp_dir = setup_default_test_config();
    write_spec(
        &temp_dir,
        "lib",
        &format!("name: lib\nenvironments:\n  {SELFIE_ENV}:\n    install: \"true\"\n"),
    );
    write_spec(
        &temp_dir,
        "app",
        &format!(
            "name: app\nenvironments:\n  {SELFIE_ENV}:\n    install: \"true\"\n    dependencies:\n      - lib\n"
        ),
    );

    let out = run_sandboxed(&temp_dir, &["spec", "remove", "lib", "--yes"]);

    assert_eq!(out.code, Some(0), "{}", out.stderr);
    assert!(out.stderr.contains("broken dependencies"), "{}", out.stderr);
    assert!(
        out.stderr.contains("You may need to update"),
        "{}",
        out.stderr
    );
    assert!(
        !out.stdout.contains("You may need to update"),
        "{}",
        out.stdout
    );
}

// Drift is `sync status`'s answer, so its list stays on stdout; the advice
// after it does not.
#[test]
fn sync_status_keeps_its_drift_list_and_moves_its_advice() {
    let temp_dir = setup_default_test_config();
    let packages = temp_dir.path().join("packages");
    fs::create_dir_all(packages.join("bat")).unwrap();
    fs::write(packages.join("bat").join("config"), "cfg\n").unwrap();
    write_spec(
        &temp_dir,
        "bat",
        &format!(
            "name: bat\ndotfiles:\n  - source: bat/config\n    target: ~/.batrc\nenvironments:\n  {SELFIE_ENV}:\n    install: \"true\"\n"
        ),
    );
    package_repo_with_remote(&temp_dir);
    assert_eq!(run_sandboxed(&temp_dir, &["apply", "--yes"]).code, Some(0));
    fs::write(temp_dir.path().join(".batrc"), "edited\n").unwrap();

    let out = run_sandboxed(&temp_dir, &["sync", "status"]);

    assert!(out.stdout.contains(".batrc"), "{}", out.stdout);
    assert!(!out.stdout.contains("Run 'selfie apply'"), "{}", out.stdout);
    assert!(out.stderr.contains("Run 'selfie apply'"), "{}", out.stderr);
}

// A push refused for a failing spec explains itself on stderr: the error, the
// header and the table.
#[test]
fn sync_push_explains_a_validation_failure_on_stderr() {
    let temp_dir = setup_default_test_config();
    write_spec(
        &temp_dir,
        "bat",
        &format!("name: bat\nenvironments:\n  {SELFIE_ENV}:\n    install: \"true\"\n"),
    );
    package_repo_with_remote(&temp_dir);
    write_spec(&temp_dir, "noenv", "name: noenv\ndescription: x\n");

    let out = run_sandboxed(&temp_dir, &["sync", "push"]);

    assert_eq!(out.code, Some(1), "{}{}", out.stdout, out.stderr);
    assert_eq!(out.stdout, "");
    for needle in ["failed validation", "Validation", "noenv", "environments"] {
        assert!(out.stderr.contains(needle), "{needle}\n{}", out.stderr);
    }
}
