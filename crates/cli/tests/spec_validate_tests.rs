pub mod common;

use std::fs;

use common::{sandboxed_command, setup_default_test_config};
use predicates::prelude::*;

// A fixture value, never a real credential. High-entropy and deliberately not
// shaped like a path, a package name or an environment name: `assert_secret_free`
// scans a twelve-character window, so a path-shaped value would match ordinary
// output and pass for the wrong reason.
const SECRET: &str = "Xq7Rm2Kz9Wp4Ns6Tv8Bh3Gd5";

fn spec_with_a_secret_above_the_failure(temp_dir: &tempfile::TempDir) {
    let packages_dir = temp_dir.path().join("packages");
    fs::create_dir_all(&packages_dir).unwrap();
    fs::write(
        packages_dir.join("creds.yml"),
        format!(
            "name: creds\ndotfiles:\n  - command: op read op://vault/item/field\n    \
             vars:\n      token: {SECRET}\n    target: ~/.npmrc\nenvironments: {{oops\n"
        ),
    )
    .unwrap();
}

// The window the maintainer asked for: a line and a column alone means holding an
// offset in your head and opening an editor to find it.
#[test]
fn an_unparsable_spec_shows_the_lines_around_the_failure() {
    let temp_dir = setup_default_test_config();
    spec_with_a_secret_above_the_failure(&temp_dir);

    // A spec that will not parse is a failed run, not a quiet one. Without this
    // the assertions below would still hold if the exit code regressed to 0.
    let output = sandboxed_command(&temp_dir)
        .args(["spec", "validate", "creds"])
        .assert()
        .failure()
        .get_output()
        .stderr
        .clone();
    let stderr = String::from_utf8(output).expect("stderr must be UTF-8");

    // The failing line, its number, and a caret under the column.
    assert!(
        stderr.contains("environments: {oops"),
        "the window must quote the failing line, got: {stderr}"
    );
    assert!(
        stderr.contains('^'),
        "the window must point at it, got: {stderr}"
    );
    assert!(
        stderr.contains("creds.yml:7:15"),
        "the location must be named, got: {stderr}"
    );

    // The neighbors come with it, `vars:` value and all, and that is the agreed
    // behavior rather than a leak being tolerated: a terminal shows the reader
    // their own file, and the MCP server shows the same failure with none of it.
    // `event_collector.rs` holds the other half and asserts the opposite.
    //
    // A plain `contains`, because the value must be PRESENT here. Narrowing the
    // window, or dropping it, has to fail this rather than pass quietly.
    assert!(
        stderr.contains(SECRET),
        "the window must quote the lines either side, got: {stderr}"
    );
}

// The window is stderr, like the sentence above it.
//
// The stderr predicate is the control. On its own, "stdout does not hold the
// window" also passes a run that produced no window anywhere.
#[test]
fn the_window_goes_to_stderr_and_not_stdout() {
    let temp_dir = setup_default_test_config();
    spec_with_a_secret_above_the_failure(&temp_dir);

    sandboxed_command(&temp_dir)
        .args(["spec", "validate", "creds"])
        .assert()
        .stderr(predicate::str::contains("environments: {oops"))
        .stdout(predicate::str::contains("environments: {oops").not());
}

// One failure, one marker. The location line and the window are evidence for the
// sentence above them, and `print_error_context` exists to say that in the output:
// a second marker reads as a second problem.
#[test]
fn one_parse_failure_prints_one_marker() {
    let temp_dir = setup_default_test_config();
    spec_with_a_secret_above_the_failure(&temp_dir);

    let output = sandboxed_command(&temp_dir)
        .args(["spec", "validate", "creds"])
        .assert()
        .get_output()
        .stderr
        .clone();
    let stderr = String::from_utf8(output).expect("stderr must be UTF-8");

    assert_eq!(
        stderr.matches('✗').count(),
        1,
        "one failure must print one marker, got: {stderr}"
    );
}

// The listing side of the same file, unchanged: many files, one line each. A
// source window per row is what made this output unreadable.
#[test]
fn a_listing_does_not_show_a_window() {
    let temp_dir = setup_default_test_config();
    spec_with_a_secret_above_the_failure(&temp_dir);

    let output = sandboxed_command(&temp_dir)
        .args(["spec", "list"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let stdout = String::from_utf8(output).expect("stdout must be UTF-8");

    assert!(
        stdout.contains("creds.yml"),
        "the spec must be listed, got: {stdout}"
    );
    assert!(
        !stdout.contains("environments: {oops"),
        "a listing must not quote the file, got: {stdout}"
    );
}
