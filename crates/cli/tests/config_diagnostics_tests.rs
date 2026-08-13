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

/// A sandbox whose config file is exactly `body`, with the two required
/// settings already in place.
fn config_with(body: &str) -> tempfile::TempDir {
    let temp = tempfile::tempdir().unwrap();
    let config_dir = temp.path().join(".config").join("selfie");
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::create_dir_all(temp.path().join("packages")).unwrap();

    std::fs::write(
        config_dir.join("config.yaml"),
        format!(
            "environment: {}\npackage_directory: {}\n{body}",
            common::SELFIE_ENV,
            temp.path().join("packages").display(),
        ),
    )
    .unwrap();

    temp
}

fn run(temp: &tempfile::TempDir, args: &[&str]) -> String {
    let output = sandboxed_command(temp)
        .timeout(Duration::from_secs(30))
        .args(args)
        .output()
        .unwrap();
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

// The key that started this: renamed, silently dropped, and the dotfiles setup
// it configured went on looking correct.
#[test]
fn a_stale_key_is_reported_before_the_command_runs() {
    let temp = config_with("configs_directory: /somewhere/else\n");

    let combined = run(&temp, &["package", "list"]);

    assert!(
        combined.contains("configs_directory"),
        "the ignored key must be named, got:\n{combined}"
    );
    // Naming the replacement is the whole reason the rename table exists — a
    // bare "unknown key" would satisfy the assertion above and help nobody.
    assert!(
        combined.contains("dotfiles_directory"),
        "the message must name the replacement, got:\n{combined}"
    );
}

// Diagnostics belong on stderr, all of it. `print_suggestion` writes to stdout,
// so splitting a notice across the two put `✨ Suggestion: …` into the file on
// every `selfie package list > packages.txt` until the key was removed.
#[test]
fn a_diagnostic_never_lands_in_redirected_output() {
    let temp = config_with("configs_directory: /somewhere/else\n");

    let output = sandboxed_command(&temp)
        .timeout(Duration::from_secs(30))
        .args(["package", "list"])
        .output()
        .unwrap();

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        stderr.contains("configs_directory"),
        "the diagnostic must be on stderr, got stderr:\n{stderr}"
    );
    assert!(
        !stdout.contains("configs_directory"),
        "nothing about it may reach stdout, got stdout:\n{stdout}"
    );
    assert!(
        !stdout.contains("Suggestion"),
        "the suggestion must not reach stdout either, got stdout:\n{stdout}"
    );
}

// `config validate` reports these itself, as table rows. Printing them again
// from `main` showed every ignored key twice, in two formats, in the one command
// whose whole job is reporting them.
#[test]
fn config_validate_reports_an_ignored_key_exactly_once() {
    let temp = config_with("configs_directory: /somewhere/else\n");

    let combined = run(&temp, &["config", "validate"]);

    // Counting the message rather than the key: the key legitimately appears
    // three times in one table row — as the field, inside the message, and
    // inside the suggestion — so counting it cannot tell one report from two.
    assert_eq!(
        combined.matches("was renamed to").count(),
        1,
        "the key must be reported once, got:\n{combined}"
    );
}

// An ignored key must not hide the settings someone ran this command to see.
#[test]
fn config_validate_still_shows_the_settings_when_a_key_is_ignored() {
    let temp = config_with("configs_directory: /somewhere/else\n");

    let combined = run(&temp, &["config", "validate"]);

    assert!(
        combined.contains("environment:"),
        "the settings summary must survive a warning, got:\n{combined}"
    );
    assert!(
        combined.contains("package_directory:"),
        "the settings summary must survive a warning, got:\n{combined}"
    );
}

#[test]
fn config_validate_reports_an_ignored_key() {
    let temp = config_with("configs_directory: /somewhere/else\n");

    let output = sandboxed_command(&temp)
        .timeout(Duration::from_secs(30))
        .args(["config", "validate"])
        .output()
        .unwrap();
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    assert!(
        combined.contains("configs_directory"),
        "validate must report the ignored key, got:\n{combined}"
    );
    assert!(
        !combined.contains("Configuration is valid."),
        "a file with an ignored key must not be called valid, got:\n{combined}"
    );
    // A warning, not an error: the configuration is still usable.
    assert!(
        output.status.success(),
        "an ignored key must not fail the command, got:\n{combined}"
    );
}

// The control for both of the above.
#[test]
fn a_clean_config_validates_without_diagnostics() {
    let temp = config_with("");

    let combined = run(&temp, &["config", "validate"]);

    assert!(
        combined.contains("Configuration is valid."),
        "a clean file must validate, got:\n{combined}"
    );
    assert!(
        !combined.contains("Selfie ignored it"),
        "a clean file must produce no diagnostics, got:\n{combined}"
    );
}

#[test]
fn an_unknown_key_inside_the_cli_section_is_reported() {
    let temp = config_with("cli:\n  verbos: true\n");

    let combined = run(&temp, &["package", "list"]);

    assert!(
        combined.contains("cli.verbos"),
        "a misspelled key inside cli: must be reported, got:\n{combined}"
    );
}

// `cli: true` is filtered out by the library as another frontend's section and
// fails to parse on the CLI side, so only the CLI can report it.
#[test]
fn a_scalar_cli_section_is_reported() {
    let temp = config_with("cli: true\n");

    let combined = run(&temp, &["package", "list"]);

    assert!(
        combined.contains("`cli:` section could not be read"),
        "a scalar where the cli: mapping belongs must be reported, got:\n{combined}"
    );
}

// And its counterpart: an empty section is a legitimate thing to write, so
// reporting it would be a fresh false positive introduced by the fix above.
#[test]
fn an_empty_cli_section_is_not_reported() {
    let temp = config_with("cli:\n");

    let combined = run(&temp, &["package", "list"]);

    assert!(
        !combined.contains("`cli:` section could not be read"),
        "an empty cli: section must not be reported, got:\n{combined}"
    );
    assert!(
        !combined.contains("Selfie ignored it"),
        "an empty cli: section must not be reported, got:\n{combined}"
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

// The command whose whole job is reporting what selfie ignored has to report the
// `cli:` half too. `main` suppresses every notice for this command on the
// grounds that it re-renders them — so anything it does not re-render is
// suppressed and never seen, and the file gets called valid while `package list`
// warns about it.
#[test]
fn config_validate_reports_an_unknown_key_inside_the_cli_section() {
    let temp = config_with("cli:\n  verbos: true\n");

    let combined = run(&temp, &["config", "validate"]);

    assert!(
        combined.contains("cli.verbos"),
        "the key under cli: must be reported here too, got:\n{combined}"
    );
    assert!(
        !combined.contains("Configuration is valid."),
        "a file with an ignored key must not be called valid, got:\n{combined}"
    );
}

// Same for a section of the wrong shape, which is invisible to the library.
#[test]
fn config_validate_reports_a_scalar_cli_section() {
    let temp = config_with("cli: true\n");

    let combined = run(&temp, &["config", "validate"]);

    assert!(
        combined.contains("`cli:` section could not be read"),
        "a scalar cli: section must be reported here too, got:\n{combined}"
    );
}

// The control: with nothing wrong in either half it still says so, and says it
// once.
#[test]
fn config_validate_calls_a_clean_cli_section_valid() {
    let temp = config_with("cli:\n  verbose: true\n");

    let combined = run(&temp, &["config", "validate"]);

    assert_eq!(
        combined.matches("Configuration is valid.").count(),
        1,
        "a clean file must validate exactly once, got:\n{combined}"
    );
}
