//! `selfie track` on a file some spec already covers.
//!
//! The command reports the target before it prompts for anything, so this path
//! is reachable without driving the interactive select.

pub mod common;

use common::{SELFIE_ENV, sandboxed_command, setup_default_test_config};

// Reported from the spec, not from the argument. The two differ exactly here:
// the spec holds `~/…` and the caller names the same file absolutely, which is
// what `selfie track <tab-completed path>` produces.
#[test]
fn already_tracking_names_the_target_the_spec_holds() {
    let temp = setup_default_test_config();

    // Canonicalized because the child's `~` expansion canonicalizes too, and on
    // macOS the temp dir is reached through a symlink (`/var` -> `/private/var`).
    let home = temp.path().canonicalize().unwrap();

    let config = home.join(".config").join("bat");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::write(config.join("config"), "--theme=ansi").unwrap();

    let packages = home.join("packages");
    std::fs::create_dir_all(packages.join("bat")).unwrap();
    std::fs::write(packages.join("bat/config"), "--theme=ansi").unwrap();
    std::fs::write(
        packages.join("bat.yaml"),
        format!(
            "name: bat\nenvironments:\n  {SELFIE_ENV}:\n    install: \"echo i\"\ndotfiles:\n  \
             - source: \"bat/config\"\n    target: \"~/.config/bat/config\"\n"
        ),
    )
    .unwrap();

    let absolute = config.join("config");
    let output = sandboxed_command(&temp)
        .env("HOME", &home)
        .args(["track", absolute.to_str().unwrap()])
        .output()
        .unwrap();

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("~/.config/bat/config"),
        "should report the spec's target, got:\n{stdout}"
    );
    assert!(
        !stdout.contains(absolute.to_str().unwrap()),
        "should not echo the caller's absolute path, got:\n{stdout}"
    );
}

// The pre-check reports what a spec already tracks, and a spec may hold an entry
// `selfie apply` will always refuse — a relative target, or one naming another
// user's home. Reporting that as plain success with exit 0 tells the user the
// file is handled when no deploy will ever touch it (selfie-63yd).
//
// The library guards this for the three commands that reach it, but this answer
// is given before the library is called at all, so it needs the rule itself.
#[test]
fn already_tracking_refuses_an_entry_apply_would_refuse() {
    let temp = setup_default_test_config();
    let home = temp.path().canonicalize().unwrap();

    let packages = home.join("packages");
    std::fs::create_dir_all(packages.join("bat")).unwrap();
    std::fs::write(packages.join("bat/config"), "--theme=ansi").unwrap();
    // A relative target: `expand_target_path` leaves it relative, so the same
    // string on the command line matches it, and `deploy_target` refuses it.
    std::fs::write(
        packages.join("bat.yaml"),
        format!(
            "name: bat\nenvironments:\n  {SELFIE_ENV}:\n    install: \"echo i\"\ndotfiles:\n  \
             - source: \"bat/config\"\n    target: \"relative/config\"\n"
        ),
    )
    .unwrap();

    let output = sandboxed_command(&temp)
        .env("HOME", &home)
        .args(["track", "relative/config"])
        .output()
        .unwrap();

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let both = format!("{stdout}{stderr}");

    assert_eq!(
        output.status.code(),
        Some(1),
        "an entry that can never deploy must not exit 0, got:\n{both}"
    );
    assert!(
        both.contains("not absolute"),
        "the reason is not given, got:\n{both}"
    );
    // Asserted as an absence: a run that printed both sentences would still tell
    // the user the file is handled.
    assert!(
        !both.contains("Already tracking"),
        "still reports the entry as tracked, got:\n{both}"
    );
}

// The refusal points at the file holding the bad entry. `TargetRejection`'s own
// suggestion is advice for someone choosing a target, and the user here has one
// written down already — what they need is the path to open.
#[test]
fn already_tracking_names_the_spec_to_edit() {
    let temp = setup_default_test_config();
    let home = temp.path().canonicalize().unwrap();

    let packages = home.join("packages");
    std::fs::create_dir_all(packages.join("bat")).unwrap();
    std::fs::write(packages.join("bat/config"), "--theme=ansi").unwrap();
    std::fs::write(
        packages.join("bat.yaml"),
        format!(
            "name: bat\nenvironments:\n  {SELFIE_ENV}:\n    install: \"echo i\"\ndotfiles:\n  \
             - source: \"bat/config\"\n    target: \"relative/config\"\n"
        ),
    )
    .unwrap();

    let output = sandboxed_command(&temp)
        .env("HOME", &home)
        .args(["track", "relative/config"])
        .output()
        .unwrap();

    let both = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    assert!(
        both.contains("bat.yaml"),
        "the spec to edit is not named, got:\n{both}"
    );
    assert!(
        !both.contains("Use '~/.config/file'"),
        "still offers advice for choosing a target, got:\n{both}"
    );
}

// A tracked target that is a symlink is reported by the two library track
// commands. This answer is given before the library is reached, so without its
// own check the command a user is most likely to run stays the silent one.
#[cfg(unix)]
#[test]
fn already_tracking_reports_a_symlinked_target() {
    let temp = setup_default_test_config();
    let home = temp.path().canonicalize().unwrap();

    let config = home.join(".config").join("bat");
    std::fs::create_dir_all(&config).unwrap();
    let destination = home.join("real-config");
    std::fs::write(&destination, "--theme=ansi").unwrap();
    std::os::unix::fs::symlink(&destination, config.join("config")).unwrap();

    let packages = home.join("packages");
    std::fs::create_dir_all(packages.join("bat")).unwrap();
    std::fs::write(packages.join("bat/config"), "--theme=ansi").unwrap();
    std::fs::write(
        packages.join("bat.yaml"),
        format!(
            "name: bat\nenvironments:\n  {SELFIE_ENV}:\n    install: \"echo i\"\ndotfiles:\n  \
             - source: \"bat/config\"\n    target: \"~/.config/bat/config\"\n"
        ),
    )
    .unwrap();

    let output = sandboxed_command(&temp)
        .env("HOME", &home)
        .args(["track", config.join("config").to_str().unwrap()])
        .output()
        .unwrap();

    let both = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    assert_eq!(
        output.status.code(),
        Some(0),
        "an idempotent track must not be refused, got:\n{both}"
    );
    assert!(
        both.contains("symlink"),
        "the symlinked target is not reported, got:\n{both}"
    );
    assert!(
        both.contains("Already tracking"),
        "the entry is tracked and must still be reported so, got:\n{both}"
    );
}

// The control for the test above. Without it the report could be unconditional and
// that test would still pass, which is the whole failure mode a symlink fixture
// cannot see on its own.
#[test]
fn already_tracking_says_nothing_about_a_plain_target() {
    let temp = setup_default_test_config();
    let home = temp.path().canonicalize().unwrap();

    let config = home.join(".config").join("bat");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::write(config.join("config"), "--theme=ansi").unwrap();

    let packages = home.join("packages");
    std::fs::create_dir_all(packages.join("bat")).unwrap();
    std::fs::write(packages.join("bat/config"), "--theme=ansi").unwrap();
    std::fs::write(
        packages.join("bat.yaml"),
        format!(
            "name: bat\nenvironments:\n  {SELFIE_ENV}:\n    install: \"echo i\"\ndotfiles:\n  \
             - source: \"bat/config\"\n    target: \"~/.config/bat/config\"\n"
        ),
    )
    .unwrap();

    let output = sandboxed_command(&temp)
        .env("HOME", &home)
        .args(["track", config.join("config").to_str().unwrap()])
        .output()
        .unwrap();

    let both = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    assert_eq!(output.status.code(), Some(0), "got:\n{both}");
    assert!(
        both.contains("Already tracking"),
        "the entry is tracked and must be reported so, got:\n{both}"
    );
    assert!(
        !both.contains("symlink"),
        "an ordinary target was reported as a symlink, got:\n{both}"
    );
}
