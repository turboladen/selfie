//! These tests run selfie on a machine with no configuration file.
//!
//! Fresh-machine bootstrap, containers and CI all start without one, so every
//! command has to work when the flags carry everything the file would have.
//!
//! They drive the real binary, because the property is about how `main` handles
//! one specific load error. A unit test over the builder would pass whether or
//! not anything reached it.

use std::time::Duration;

pub mod common;

use common::sandboxed_command;
use tempfile::TempDir;

/// A sandbox with a package directory and deliberately **no** config file.
fn sandbox_without_config() -> TempDir {
    let temp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(temp.path().join(".config").join("selfie")).unwrap();
    std::fs::create_dir_all(temp.path().join("packages")).unwrap();
    temp
}

fn run(temp: &TempDir, args: &[&str]) -> (bool, String) {
    let output = sandboxed_command(temp)
        .timeout(Duration::from_secs(30))
        .args(args)
        .output()
        .unwrap();
    (
        output.status.success(),
        format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        ),
    )
}

#[test]
fn a_run_with_both_required_flags_needs_no_config_file() {
    let temp = sandbox_without_config();
    let packages = temp.path().join("packages");

    let (ok, combined) = run(
        &temp,
        &[
            "--environment",
            "test-env",
            "--package-directory",
            packages.to_str().unwrap(),
            "package",
            "list",
        ],
    );

    assert!(ok, "expected success, got:\n{combined}");
    assert!(
        !combined.contains("No configuration file found"),
        "the absent file must not be reported at all, got:\n{combined}"
    );
}

// Every missing setting, not just the first. A fresh-machine bootstrap that
// reports one flag per run is a guessing game.
#[test]
fn both_missing_required_settings_are_named() {
    let temp = sandbox_without_config();

    let (ok, combined) = run(&temp, &["package", "list"]);

    assert!(!ok, "expected a failure, got:\n{combined}");
    assert!(
        combined.contains("--environment"),
        "the missing environment flag must be named, got:\n{combined}"
    );
    assert!(
        combined.contains("--package-directory"),
        "the missing package-directory flag must be named, got:\n{combined}"
    );
}

// The control for the above: the message is computed from what is actually
// missing, not printed as a fixed block.
#[test]
fn only_the_missing_setting_is_named() {
    let temp = sandbox_without_config();

    let (ok, combined) = run(&temp, &["--environment", "test-env", "package", "list"]);

    assert!(!ok, "expected a failure, got:\n{combined}");
    assert!(
        combined.contains("--package-directory"),
        "the missing flag must be named, got:\n{combined}"
    );
    assert!(
        !combined.contains("--environment"),
        "a flag that was supplied must not be listed as missing, got:\n{combined}"
    );
}

// A fifo at the configuration path is not an absent file, and must not be
// quietly replaced by the flags — the flags would "work" while selfie ignored a
// configuration file that is really there.
#[cfg(unix)]
#[test]
fn a_fifo_config_is_still_fatal_when_every_flag_is_supplied() {
    let temp = sandbox_without_config();
    let packages = temp.path().join("packages");
    let status = std::process::Command::new("mkfifo")
        .arg(
            temp.path()
                .join(".config")
                .join("selfie")
                .join("config.yaml"),
        )
        .status()
        .unwrap();
    assert!(status.success(), "mkfifo failed to create the test fixture");

    let (ok, combined) = run(
        &temp,
        &[
            "--environment",
            "test-env",
            "--package-directory",
            packages.to_str().unwrap(),
            "package",
            "list",
        ],
    );

    assert!(!ok, "expected a refusal, got:\n{combined}");
    assert!(
        combined.contains("not a regular file"),
        "the refusal must survive the flags-only fallback, got:\n{combined}"
    );
}

// The same property for a file that is present and unparsable, and the case that
// pins the *variant* rather than the outcome. Widening this fallback to every
// load error would silently replace a config file the user is actively editing
// with the flags — and unlike a refusal, a parse failure has nothing else in the
// pipeline that would stop the run.
#[test]
fn a_malformed_config_is_still_fatal_when_every_flag_is_supplied() {
    let temp = sandbox_without_config();
    let packages = temp.path().join("packages");
    std::fs::write(
        temp.path()
            .join(".config")
            .join("selfie")
            .join("config.yaml"),
        "environment: [unclosed\n",
    )
    .unwrap();

    let (ok, combined) = run(
        &temp,
        &[
            "--environment",
            "test-env",
            "--package-directory",
            packages.to_str().unwrap(),
            "package",
            "list",
        ],
    );

    assert!(
        !ok,
        "a config file that cannot be parsed must not be replaced by the flags, got:\n{combined}"
    );
}

// The optional directories keep their fallbacks, so two flags really are enough.
// `state_directory` falls back under HOME and `dotfiles_directory` to a sibling
// of the package directory — the same values a two-key config file produces.
#[test]
fn the_optional_directories_keep_their_defaults() {
    let temp = sandbox_without_config();
    let packages = temp.path().join("packages");
    std::fs::create_dir_all(temp.path().join("dotfiles")).unwrap();
    std::fs::write(
        temp.path().join("dotfiles").join("starship.yaml"),
        "name: starship\nenvironments:\n  test-env:\n    install: \"echo i\"\ndotfiles:\n  - source: \"starship.conf\"\n    target: \"~/.config/starship.conf\"\n",
    )
    .unwrap();

    let (ok, combined) = run(
        &temp,
        &[
            "--environment",
            "test-env",
            "--package-directory",
            packages.to_str().unwrap(),
            "dotfiles",
            "list",
        ],
    );

    assert!(ok, "expected success, got:\n{combined}");
    // The dotfiles directory is found through the sibling default, because no
    // `--dotfiles-directory` flag is given.
    assert!(
        combined.contains("starship"),
        "the sibling dotfiles directory must still be found, got:\n{combined}"
    );
}

// A flag value is used exactly as typed — `~` is expanded for the same setting
// in the configuration file, but not here. This pins the divergence rather than
// hiding it: an equivalence test using absolute paths would pass while this
// difference went unrecorded.
#[test]
fn a_tilde_in_a_flag_is_not_expanded() {
    let temp = sandbox_without_config();

    let (ok, combined) = run(
        &temp,
        &[
            "--environment",
            "test-env",
            "--package-directory",
            "~/packages",
            "package",
            "list",
        ],
    );

    assert!(!ok, "a literal ~ cannot resolve, got:\n{combined}");
    assert!(
        combined.contains("~/packages"),
        "the unexpanded value must appear in the error, got:\n{combined}"
    );
}

// `--environment ''` parses to `Some("")`, which passes a bare presence check
// and builds a configuration whose environment matches no package. With no file
// there is nothing downstream to validate it, so the run would fail much later
// somewhere less informative.
#[test]
fn an_empty_required_flag_counts_as_missing() {
    let temp = sandbox_without_config();
    let packages = temp.path().join("packages");

    let (ok, combined) = run(
        &temp,
        &[
            "--environment",
            "",
            "--package-directory",
            packages.to_str().unwrap(),
            "package",
            "list",
        ],
    );

    assert!(
        !ok,
        "an empty environment must not be accepted, got:\n{combined}"
    );
    assert!(
        combined.contains("--environment"),
        "it must be named as missing, got:\n{combined}"
    );
}

// A configuration file that is present as a link going nowhere, on a run that
// supplies every flag.
//
// When the file is absent the flags stand in for it. When it is present but
// unresolvable they must not, because the user would be told nothing while
// selfie silently ignored the configuration they have.
#[cfg(unix)]
#[test]
fn a_dangling_config_symlink_is_still_fatal_when_every_flag_is_supplied() {
    let temp = sandbox_without_config();
    let packages = temp.path().join("packages");
    std::os::unix::fs::symlink(
        temp.path().join("not-checked-out").join("config.yaml"),
        temp.path()
            .join(".config")
            .join("selfie")
            .join("config.yaml"),
    )
    .unwrap();

    let (ok, combined) = run(
        &temp,
        &[
            "--environment",
            "test-env",
            "--package-directory",
            packages.to_str().unwrap(),
            "package",
            "list",
        ],
    );

    assert!(
        !ok,
        "a link that is present must not be replaced by the flags, got:\n{combined}"
    );
    assert!(
        combined.contains("does not resolve"),
        "the refusal must survive the flags-only fallback, got:\n{combined}"
    );
}
