pub mod common;

use std::fs;

use common::{SELFIE_ENV, package_repo_with_remote, sandboxed_command, setup_default_test_config};

// Every run prints its result: a command that finishes prints at least one line
// on stdout saying so, even when there is nothing else to show. Each row of the
// output contract's command table is run once at default verbosity.

// Two packages with inert commands, one with a dotfile, in a git repository
// with a bare remote, and a file in the sandbox home to track.
fn sandbox() -> tempfile::TempDir {
    let temp_dir = setup_default_test_config();
    let root = temp_dir.path();
    let packages = root.join("packages");
    fs::create_dir_all(packages.join("bat")).unwrap();
    fs::write(packages.join("bat").join("config"), "cfg\n").unwrap();
    // `check: "true"` succeeds without output, so the result line is the only
    // thing its check prints.
    fs::write(
        packages.join("bat.yaml"),
        format!(
            "name: bat\ndotfiles:\n  - source: bat/config\n    target: ~/.config/bat/config\n\
             environments:\n  {SELFIE_ENV}:\n    install: \"true\"\n    check: \"true\"\n    \
             audit: \"echo brew\"\n"
        ),
    )
    .unwrap();
    fs::write(
        packages.join("fd.yaml"),
        format!(
            "name: fd\nenvironments:\n  {SELFIE_ENV}:\n    install: \"true\"\n    \
             audit: \"echo brew\"\n"
        ),
    )
    .unwrap();
    fs::write(root.join(".tracked-rc"), "rc\n").unwrap();
    fs::create_dir_all(root.join("dotfiles")).unwrap();

    package_repo_with_remote(&temp_dir);

    temp_dir
}

#[test]
fn every_command_prints_its_result_on_stdout() {
    let temp_dir = sandbox();

    let cases: &[&[&str]] = &[
        &["apply", "--dry-run"],
        &["apply"],
        &["dotfiles", "drift"],
        &["dotfiles", "list"],
        &["package", "install", "bat"],
        &["package", "check", "bat"],
        &["package", "audit", "bat"],
        &["package", "audit", "--all"],
        &["package", "list"],
        &["package", "status", "bat"],
        &["spec", "info", "bat"],
        &["spec", "list"],
        &["spec", "search", "bat"],
        &["spec", "validate", "bat"],
        &["spec", "validate", "--all"],
        &["spec", "create", "newpkg"],
        &["spec", "edit", "newpkg"],
        &["spec", "remove", "newpkg", "--yes"],
        &["config", "validate"],
        &["sync", "status"],
        &["sync", "pull"],
        &["sync", "push"],
        &["dotfiles", "track", "rc", "~/.tracked-rc"],
    ];

    for args in cases {
        let output = sandboxed_command(&temp_dir)
            .env("EDITOR", "true")
            .args(*args)
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);

        // Clean or found: a failure, a usage error, a panic, a cancel or a
        // signal all fail here.
        assert!(
            matches!(output.status.code(), Some(0 | 3)),
            "{args:?} did not finish: {:?}\nstdout:\n{stdout}\nstderr:\n{stderr}",
            output.status.code()
        );
        assert!(
            stdout.lines().any(|line| !line.trim().is_empty()),
            "{args:?} printed no result on stdout\nstderr:\n{stderr}"
        );
    }
}

// A check command that succeeds silently still gets a result line, naming the
// package and the environment it was checked in.
#[test]
fn a_silent_passing_check_names_the_package_and_environment() {
    let temp_dir = sandbox();

    let output = sandboxed_command(&temp_dir)
        .args(["package", "check", "bat"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);

    assert_eq!(output.status.code(), Some(0), "{stdout}");
    assert!(
        stdout.lines().any(|line| line.contains("'bat'")
            && line.contains("check completed")
            && line.contains("in environment 'test-env'")),
        "{stdout}"
    );
}

// A validator's issues, notes and verdict are its answer, so they are on stdout
// whatever the verdict, and the exit code carries the outcome.
#[test]
fn a_validator_prints_its_verdict_on_stdout() {
    let temp_dir = sandbox();
    let packages = temp_dir.path().join("packages");
    fs::write(packages.join("noenv.yaml"), "name: noenv\ndescription: x\n").unwrap();

    let spec = sandboxed_command(&temp_dir)
        .args(["spec", "validate", "noenv"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&spec.stdout);
    let stderr = String::from_utf8_lossy(&spec.stderr);
    assert_eq!(spec.status.code(), Some(1), "{stdout}{stderr}");
    assert!(
        stdout.contains("validation failed with 1 error(s)"),
        "{stdout}"
    );
    assert!(!stderr.contains("validation failed"), "{stderr}");

    let all = sandboxed_command(&temp_dir)
        .args(["spec", "validate", "--all"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&all.stdout);
    assert_eq!(all.status.code(), Some(1), "{stdout}");
    assert!(stdout.contains("1 with errors"), "{stdout}");

    // A `cli:` key the section does not know makes the file usable, with a
    // warning.
    let config_path = temp_dir.path().join(".config/selfie/config.yaml");
    let mut config = fs::read_to_string(&config_path).unwrap();
    config.push_str("cli:\n  verbos: true\n");
    fs::write(&config_path, config).unwrap();
    let config = sandboxed_command(&temp_dir)
        .args(["config", "validate"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&config.stdout);
    let stderr = String::from_utf8_lossy(&config.stderr);
    assert_eq!(config.status.code(), Some(3), "{stdout}{stderr}");
    assert!(stdout.contains("usable, with warnings"), "{stdout}");
    assert!(
        stdout.contains("verbos"),
        "the notice is part of the answer: {stdout}"
    );
    assert!(!stderr.contains("verbos"), "{stderr}");
}

// `sync status` relays drift, which depends on the environment, so its drift
// line names it, on stdout.
#[test]
fn sync_status_names_the_environment_of_its_drift_line() {
    let temp_dir = sandbox();

    let output = sandboxed_command(&temp_dir)
        .args(["sync", "status"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);

    assert!(
        stdout
            .lines()
            .any(|line| line.contains("drift") && line.contains("in environment 'test-env'")),
        "{stdout}"
    );
}
