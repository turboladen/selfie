pub mod common;

use common::{sandboxed_command, setup_default_test_config, setup_test_config};
use predicates::prelude::*;

#[test]
fn test_validate_valid_config() {
    // Valid config using default test config (creates real directories)
    let temp_dir = setup_default_test_config();
    let mut cmd = sandboxed_command(&temp_dir);
    cmd.args(["config", "validate"]);

    cmd.assert()
        .success()
        .stdout(predicates::str::contains("Configuration is valid"));
}

// Both optional directories are reported, as the values in effect: a wrong one
// is then visible here rather than only when a command behaves oddly. The
// default config names neither, so what is printed is the derived default for
// each, and a report of only what the file says would print nothing.
#[test]
fn config_validate_reports_the_dotfiles_and_state_directories() {
    let temp_dir = setup_default_test_config();
    // The package directory prints as written, so the dotfiles default beside
    // it does too. The home directory is canonicalized when `~` is resolved, so
    // the state default prints as `/private/var/...` where the sandbox was minted
    // as `/var/...` on macOS.
    let written = temp_dir.path();
    let root = temp_dir.path().canonicalize().unwrap();
    let mut cmd = sandboxed_command(&temp_dir);
    cmd.args(["config", "validate"]);

    cmd.assert()
        .success()
        .stdout(predicates::str::contains(format!(
            "dotfiles_directory: {}",
            written.join("dotfiles").display()
        )))
        .stdout(predicates::str::contains(format!(
            "state_directory: {}",
            root.join(".local/state/selfie").display()
        )));
}

#[test]
fn test_validate_invalid_config() {
    // Invalid config with missing required fields
    let yaml = r#"
# Missing environment
package_directory: "/test/packages"
"#;

    let temp_dir = setup_test_config(yaml);
    let mut cmd = sandboxed_command(&temp_dir);
    cmd.args(["config", "validate"]);

    // Validate's own row, not the refusal every other command gives. The rows
    // and the verdict are this command's answer, so they are on stdout.
    cmd.assert()
        .failure()
        .stdout(predicates::str::contains("Validation failed."))
        .stdout(predicates::str::contains(
            "The `environment` setting is missing",
        ))
        .stderr(predicates::str::contains("Validation failed.").not());
}

// A flag does not fill the gap for this command, which reports the file. Nor
// does the gap stop it: the other commands refuse such a file before they run.
#[test]
fn a_missing_environment_is_reported_as_a_row() {
    let packages = tempfile::tempdir().unwrap();
    let yaml = format!("package_directory: \"{}\"\n", packages.path().display());

    let temp_dir = setup_test_config(&yaml);
    let mut cmd = sandboxed_command(&temp_dir);
    cmd.args(["--environment", "flag-env", "config", "validate"]);

    cmd.assert()
        .stdout(predicates::str::contains("Validation failed."))
        .stdout(predicates::str::contains(
            "The `environment` setting is missing",
        ))
        .stderr(predicates::str::contains("does not set every required setting").not());
}

#[test]
fn test_validate_config_with_invalid_path() {
    // Config with invalid package directory (not absolute)
    let yaml = r#"
environment: "test-env"
package_directory: "relative/path"
"#;

    let temp_dir = setup_test_config(yaml);
    let mut cmd = sandboxed_command(&temp_dir);
    cmd.args(["config", "validate"]);

    cmd.assert()
        .failure()
        .stdout(predicates::str::contains("relative and cannot be resolved"));
}

// Every command that reads the package directory fails without it, so
// validate reports its absence as an error.
#[test]
fn test_validate_config_with_nonexistent_directory_shows_error() {
    // Use a guaranteed-nonexistent path under a fresh temp dir
    let pkg_tmp = tempfile::tempdir().unwrap();
    let nonexistent = pkg_tmp.path().join("does-not-exist");
    let yaml = format!(
        "environment: \"test-env\"\npackage_directory: \"{}\"",
        nonexistent.display()
    );

    let temp_dir = setup_test_config(&yaml);
    let mut cmd = sandboxed_command(&temp_dir);
    cmd.args(["config", "validate"]);

    // An error is a failure, which exits 1.
    cmd.assert()
        .code(1)
        .stdout(predicates::str::contains("Validation failed."))
        .stdout(predicates::str::contains("does not exist"));
}

// A flag must not change what this command reports, including the two `cli:`
// settings. Every other line already came from the reloaded file; these two came
// from the flag-merged config, so `--no-color` made a file saying
// `use_colors: true` read back as false.
#[test]
fn a_flag_does_not_change_the_cli_settings_this_reports() {
    let packages = tempfile::tempdir().unwrap();
    let yaml = format!(
        "environment: \"test-env\"\npackage_directory: \"{}\"\ncli:\n  use_colors: true\n  verbose: true\n",
        packages.path().display()
    );

    let temp_dir = setup_test_config(&yaml);
    let mut cmd = sandboxed_command(&temp_dir);
    cmd.args(["--no-color", "config", "validate"]);

    // Asserted as the file's values, against the flag that contradicts one of
    // them. Asserting only that the command succeeds would pass either way.
    cmd.assert()
        .stdout(predicates::str::contains("use_colors: true"))
        .stdout(predicates::str::contains("verbose: true"));
}

// A fresh machine: the named state directory is not there yet, and selfie
// creates it on the first write. That needs nothing done, so it is a note and
// the file still validates.
#[test]
fn a_state_directory_not_there_yet_is_a_note_not_a_warning() {
    let packages = tempfile::tempdir().unwrap();
    let state = packages.path().join("state-not-there-yet");
    let yaml = format!(
        "environment: \"test-env\"\npackage_directory: \"{}\"\nstate_directory: \"{}\"\n",
        packages.path().display(),
        state.display()
    );

    let temp_dir = setup_test_config(&yaml);
    let mut cmd = sandboxed_command(&temp_dir);
    cmd.args(["config", "validate"]);

    cmd.assert()
        .success()
        .stdout(predicates::str::contains("is not there yet"))
        .stdout(predicates::str::contains("Configuration is valid."))
        .stderr(predicates::str::contains("Validation failed.").not());
}
