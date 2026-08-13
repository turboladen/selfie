//! What selfie says about the configuration file itself — the file it cannot
//! read, and the keys in it that it ignored.
//!
//! These drive the real binary. The unit tests over the loader prove it returns
//! the right value; only a real process proves the CLI reports it, and only a
//! real fifo proves the guard runs before a read that would otherwise never
//! return.

use std::time::Duration;

pub mod common;

use common::sandboxed_command;

/// A fifo at the configuration path, which every command reads before doing
/// anything else.
#[cfg(unix)]
fn config_dir_with_fifo() -> tempfile::TempDir {
    let temp_dir = tempfile::tempdir().unwrap();
    let config_dir = temp_dir.path().join(".config").join("selfie");
    std::fs::create_dir_all(&config_dir).unwrap();

    // Shelling out rather than taking a `nix` dev-dependency for one call in one
    // test. `mkfifo` is POSIX and this test is unix-only anyway.
    let status = std::process::Command::new("mkfifo")
        .arg(config_dir.join("config.yaml"))
        .status()
        .unwrap();
    assert!(status.success(), "mkfifo failed to create the test fixture");

    temp_dir
}

// The deadline is what turns a regression into a failure rather than a wedged
// suite: without the guard this command never returns, and a test that simply
// waited would hang CI instead of reporting.
//
// Asserting on the wording as well as the status matters here. A timeout kill
// also exits non-zero, so "exited non-zero" alone would pass on the very hang
// this exists to catch.
#[cfg(unix)]
#[test]
fn a_fifo_config_file_does_not_hang_the_cli() {
    let temp_dir = config_dir_with_fifo();

    let output = sandboxed_command(&temp_dir)
        .timeout(Duration::from_secs(10))
        .args(["package", "list"])
        .output()
        .unwrap();

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !output.status.success(),
        "expected a refusal, got success. stderr: {stderr}"
    );
    assert!(
        stderr.contains("not a regular file"),
        "stderr did not name the problem: {stderr}"
    );
    assert!(
        stderr.contains("named pipe (fifo)"),
        "stderr did not name the kind: {stderr}"
    );
}

// The refusal must not read as "there is no configuration file". Skipping
// irregular files during discovery would produce that, and would still exit
// non-zero — so the negative assertion is the one carrying the weight.
#[cfg(unix)]
#[test]
fn a_fifo_config_file_is_not_reported_as_a_missing_one() {
    let temp_dir = config_dir_with_fifo();

    let output = sandboxed_command(&temp_dir)
        .timeout(Duration::from_secs(10))
        .args(["package", "list"])
        .output()
        .unwrap();

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("No configuration file found"),
        "a fifo was reported as an absent file: {stderr}"
    );
}

// A config file symlinked into a dotfiles repository that is not checked out
// yet. `path_exists` follows, so this reads as "no configuration file" — and the
// user's configuration is right there, being ignored.
#[cfg(unix)]
#[test]
fn a_dangling_config_symlink_is_not_reported_as_absent() {
    let temp_dir = tempfile::tempdir().unwrap();
    let config_dir = temp_dir.path().join(".config").join("selfie");
    std::fs::create_dir_all(&config_dir).unwrap();
    std::os::unix::fs::symlink(
        temp_dir.path().join("not-checked-out").join("config.yaml"),
        config_dir.join("config.yaml"),
    )
    .unwrap();

    let output = sandboxed_command(&temp_dir)
        .timeout(Duration::from_secs(30))
        .args(["package", "list"])
        .output()
        .unwrap();
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    assert!(
        !output.status.success(),
        "expected a refusal, got:\n{combined}"
    );
    assert!(
        combined.contains("does not resolve"),
        "the unresolvable link must be named, got:\n{combined}"
    );
    assert!(
        !combined.contains("No configuration file found"),
        "a link that is present must not be reported as absent, got:\n{combined}"
    );
}

// The control: a symlinked config that *does* resolve is the supported, common
// setup — pointing selfie at a file kept in a dotfiles repository. A guard that
// refused every symlink would break it.
#[cfg(unix)]
#[test]
fn a_config_symlink_that_resolves_is_read_normally() {
    let temp_dir = tempfile::tempdir().unwrap();
    let config_dir = temp_dir.path().join(".config").join("selfie");
    let repo = temp_dir.path().join("dotfiles-repo");
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::create_dir_all(&repo).unwrap();
    std::fs::create_dir_all(temp_dir.path().join("packages")).unwrap();
    std::fs::write(
        repo.join("config.yaml"),
        format!(
            "environment: {}\npackage_directory: {}\n",
            common::SELFIE_ENV,
            temp_dir.path().join("packages").display()
        ),
    )
    .unwrap();
    std::os::unix::fs::symlink(repo.join("config.yaml"), config_dir.join("config.yaml")).unwrap();

    let output = sandboxed_command(&temp_dir)
        .timeout(Duration::from_secs(30))
        .args(["package", "list"])
        .output()
        .unwrap();
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    assert!(
        output.status.success(),
        "expected success, got:\n{combined}"
    );
    assert!(
        !combined.contains("does not resolve"),
        "a link that resolves must not be refused, got:\n{combined}"
    );
}

// The control. Same command, same sandbox, an ordinary config file — so a guard
// that refused every configuration file would fail here rather than looking like
// a pass everywhere else.
#[test]
fn an_ordinary_config_file_is_read_normally() {
    let temp_dir = common::setup_default_test_config();

    let output = sandboxed_command(&temp_dir)
        .timeout(Duration::from_secs(30))
        .args(["package", "list"])
        .output()
        .unwrap();

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("not a regular file"),
        "a regular file was refused: {stderr}"
    );
    assert!(
        output.status.success(),
        "expected success on a valid config. stderr: {stderr}"
    );
}
