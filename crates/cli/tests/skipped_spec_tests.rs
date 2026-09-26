pub mod common;

use std::fs;

use common::{sandboxed_command, setup_default_test_config};

// What every command that enumerates specs prints when it skips one it could not
// parse. Four commands share one sentence, and these are the only assertions on
// its bytes.
//
// They are indifferent to who renders it -- the library's shared helper, or the
// CLI adapter that calls it -- and that is the point: they fail if the bytes
// change, whoever produces them.
fn sandbox_with_one_unparsable_spec() -> tempfile::TempDir {
    let temp_dir = setup_default_test_config();
    let packages_dir = temp_dir.path().join("packages");
    fs::create_dir_all(&packages_dir).unwrap();
    fs::write(
        packages_dir.join("good.yml"),
        "name: good\nenvironments:\n  test-env:\n    install: \"true\"\n",
    )
    .unwrap();
    fs::write(
        packages_dir.join("creds.yml"),
        "name: creds\nenvironments: {oops\n",
    )
    .unwrap();
    temp_dir
}

fn stderr_of(temp_dir: &tempfile::TempDir, args: &[&str]) -> String {
    let output = sandboxed_command(temp_dir)
        .args(args)
        .assert()
        .get_output()
        .stderr
        .clone();
    String::from_utf8(output).expect("stderr must be UTF-8")
}

// The sentence itself, on the four commands that emit it.
#[test]
fn every_enumerating_command_names_the_skipped_spec_the_same_way() {
    let temp_dir = sandbox_with_one_unparsable_spec();
    // The tail of the sentence, not the absolute path: a temp dir resolves through
    // /private on macOS, and the prefix is asserted separately below.
    let expected = "creds.yml: YAML parsing error: unclosed bracket '{' at line 2, column 15";

    for args in [
        &["spec", "validate", "--all"][..],
        &["package", "audit", "--all"][..],
        &["apply", "--dry-run"][..],
        &["dotfiles", "drift"][..],
    ] {
        let stderr = stderr_of(&temp_dir, args);
        assert!(
            stderr.contains("Skipping package file "),
            "{args:?} must name what it skipped, got: {stderr}"
        );
        assert!(
            stderr.contains(expected),
            "{args:?} must print the shared reason, got: {stderr}"
        );
    }
}

// The file is named once across the whole line: the sentence prefixes the path,
// and the reason must not name it again.
//
// Asserted here, against real output, because the claim is about what a reader
// sees. The Debug of a typed event resembles nothing a user reads, so the same
// assertion made against an event stream would pass while the terminal printed
// the file twice.
#[test]
fn a_skipped_spec_is_named_once_on_the_line_that_reports_it() {
    let temp_dir = sandbox_with_one_unparsable_spec();

    for args in [
        &["spec", "validate", "--all"][..],
        &["package", "audit", "--all"][..],
        &["apply", "--dry-run"][..],
        &["dotfiles", "drift"][..],
    ] {
        let stderr = stderr_of(&temp_dir, args);
        let line = stderr
            .lines()
            .find(|line| line.contains("Skipping package file"))
            .unwrap_or_else(|| panic!("{args:?} printed no skip line, got: {stderr}"));

        assert_eq!(
            line.matches("creds.yml").count(),
            1,
            "{args:?}: the file must be named once, got: {line}"
        );
    }
}

// A spec that parses is still processed. Without this, a regression that skipped
// everything would satisfy both tests above.
//
// Counted rather than searched for: `good` is a substring of the skip line that
// names it, so any assertion phrased as "says good, or does not skip good" holds
// for every possible output and can never fail.
#[test]
fn the_readable_spec_is_still_processed() {
    let temp_dir = sandbox_with_one_unparsable_spec();
    let stderr = stderr_of(&temp_dir, &["spec", "validate", "--all"]);

    let skipped: Vec<&str> = stderr
        .lines()
        .filter(|line| line.contains("Skipping package file"))
        .collect();

    assert_eq!(
        skipped.len(),
        1,
        "exactly one spec must be skipped, got: {stderr}"
    );
    assert!(
        skipped[0].contains("creds.yml"),
        "the skipped spec must be the unparsable one, got: {}",
        skipped[0]
    );
}

// The exit status and the terminal text are what the library tests cannot see.
// A handler that claimed the failure event would print it and exit 0.
fn apply_output(temp_dir: &tempfile::TempDir, name: &str) -> (Option<i32>, String) {
    let output = sandboxed_command(temp_dir)
        .args(["apply", name])
        .output()
        .unwrap();
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stderr),
        String::from_utf8_lossy(&output.stdout)
    );
    (output.status.code(), text)
}

// Naming the unparsable spec must not read as "no such package": the file is
// there, and the skip line above the failure says why it could not be used.
#[test]
fn apply_naming_an_unparsable_spec_fails_and_says_it_could_not_be_loaded() {
    let temp_dir = sandbox_with_one_unparsable_spec();

    let (code, text) = apply_output(&temp_dir, "creds");

    assert_eq!(code, Some(1), "output was: {text}");
    assert!(
        text.contains("Package 'creds' could not be loaded"),
        "output was: {text}"
    );
    assert!(!text.contains("No package named"), "output was: {text}");
}

#[test]
fn apply_naming_no_package_fails() {
    let temp_dir = sandbox_with_one_unparsable_spec();

    let (code, text) = apply_output(&temp_dir, "nope");

    assert_eq!(code, Some(1), "output was: {text}");
    assert!(
        text.contains("No package named 'nope' was found"),
        "output was: {text}"
    );
}

fn drift_output(temp_dir: &tempfile::TempDir) -> (Option<i32>, String, String) {
    let output = sandboxed_command(temp_dir)
        .args(["dotfiles", "drift"])
        .output()
        .unwrap();
    (
        output.status.code(),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

// `dotfiles drift` printed its clean check directly under the warning naming a
// spec it could not load, so a run that examined none of that spec's dotfiles
// read as all clear. The clean line and the incomplete one carry the same
// sentence and differ only by stream and marker -- success to stdout with a
// check mark, warning to stderr -- so both are asserted.
#[test]
fn drift_does_not_claim_a_clean_check_over_a_spec_it_could_not_load() {
    let temp_dir = sandbox_with_one_unparsable_spec();

    let (code, stdout, stderr) = drift_output(&temp_dir);

    assert!(
        stderr.contains("creds.yml"),
        "must name the spec it could not load: {stderr}"
    );
    assert!(
        !stdout.contains("✓ Dotfile drift check"),
        "must not claim a clean check over an unloaded spec: {stdout}"
    );
    assert!(
        stderr.contains("⚠ Dotfile drift check"),
        "must report the check as incomplete: {stderr}"
    );
    assert!(
        stderr.contains("1 not loaded"),
        "must count the spec it could not load: {stderr}"
    );
    // An unloaded spec is a file the user has to fix rather than something
    // selfie refused, so the command still exits 0, as `sync status` does.
    assert_eq!(code, Some(0), "output was: {stdout}{stderr}");
}

// Control for the assertion above: with every spec loadable the clean check is
// still printed, to stdout, with nothing reported as unloaded. Without this, a
// build that never prints the line at all would satisfy the negative.
#[test]
fn drift_reports_a_clean_check_when_every_spec_loads() {
    let temp_dir = setup_default_test_config();
    let packages_dir = temp_dir.path().join("packages");
    fs::create_dir_all(&packages_dir).unwrap();
    fs::write(
        packages_dir.join("good.yml"),
        "name: good\nenvironments:\n  test-env:\n    install: \"true\"\n",
    )
    .unwrap();

    let (code, stdout, stderr) = drift_output(&temp_dir);

    assert!(
        stdout.contains("✓ Dotfile drift check"),
        "a check with every spec loaded is clean: {stdout}"
    );
    assert!(
        stdout.contains("0 not loaded"),
        "nothing was skipped, and the line says so: {stdout}"
    );
    assert_eq!(code, Some(0), "output was: {stdout}{stderr}");
}

// A named apply says nothing about specs it was not asked for, even one it could
// not load.
#[test]
fn a_named_apply_does_not_report_another_spec_it_could_not_load() {
    let temp_dir = sandbox_with_one_unparsable_spec();

    let output = sandboxed_command(&temp_dir)
        .args(["apply", "good"])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(!stderr.contains("creds.yml"), "{stderr}");
    assert_eq!(output.status.code(), Some(0), "{stderr}");
}
