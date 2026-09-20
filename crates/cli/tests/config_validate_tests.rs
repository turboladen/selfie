pub mod common;

use common::{sandboxed_command, setup_default_test_config, setup_test_config};

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
    // The loader canonicalizes the package directory and the home directory,
    // so both derived paths print in canonical form: `/private/var/...` where
    // the sandbox was minted as `/var/...` on macOS.
    let root = temp_dir.path().canonicalize().unwrap();
    let mut cmd = sandboxed_command(&temp_dir);
    cmd.args(["config", "validate"]);

    cmd.assert()
        .success()
        .stdout(predicates::str::contains(format!(
            "dotfiles_directory: {}",
            root.join("dotfiles").display()
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

    cmd.assert()
        .failure()
        .stderr(predicates::str::contains("environment"));
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
        .stderr(predicates::str::contains("relative and cannot be resolved"));
}

#[test]
fn test_validate_config_with_nonexistent_directory_shows_warning() {
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

    cmd.assert()
        .success()
        .stderr(predicates::str::contains("does not exist"));
}

// A flag must not change what this command reports, including the two `cli:`
// settings. Every other line already came from the reloaded file; these two came
// from the flag-merged config, so `--no-color` made a file saying
// `use_colors: true` read back as false.
#[test]
fn a_flag_does_not_change_the_cli_settings_this_reports() {
    let yaml = r#"
environment: "test-env"
package_directory: "/test/packages"
cli:
  use_colors: true
  verbose: true
"#;

    let temp_dir = setup_test_config(yaml);
    let mut cmd = sandboxed_command(&temp_dir);
    cmd.args(["--no-color", "config", "validate"]);

    // Asserted as the file's values, against the flag that contradicts one of
    // them. Asserting only that the command succeeds would pass either way.
    cmd.assert()
        .stdout(predicates::str::contains("use_colors: true"))
        .stdout(predicates::str::contains("verbose: true"));
}
