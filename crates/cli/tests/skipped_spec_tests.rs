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
