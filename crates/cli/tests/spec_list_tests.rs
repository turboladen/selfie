pub mod common;

use std::fs;

use common::{sandboxed_command, setup_default_test_config};
use predicates::prelude::*;

// `spec list` prints its own `Invalid: {path}` label before the reason, so a
// reason that named the file as well would print it twice. Nothing about the
// layout stops that; what stops it is that a parse failure has no path in its
// wording, and this is the render site where that shows.
#[test]
fn an_invalid_spec_is_named_once_in_the_listing() {
    let temp_dir = setup_default_test_config();
    let packages_dir = temp_dir.path().join("packages");
    fs::create_dir_all(&packages_dir).unwrap();
    fs::write(
        packages_dir.join("broken.yml"),
        "name: broken\nenvironments: [oops\n",
    )
    .unwrap();

    let output = sandboxed_command(&temp_dir)
        .args(["spec", "list"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let stdout = String::from_utf8(output).expect("stdout must be UTF-8");

    assert_eq!(
        stdout.matches("broken.yml").count(),
        1,
        "the spec must be named exactly once, got: {stdout}"
    );
}

// A spec file names credential stores in its `command:` entries, and the listing
// is one of the places a parse failure is rendered for a person to read.
#[test]
fn an_invalid_spec_does_not_quote_its_own_contents() {
    let temp_dir = setup_default_test_config();
    let packages_dir = temp_dir.path().join("packages");
    fs::create_dir_all(&packages_dir).unwrap();
    fs::write(
        packages_dir.join("creds.yml"),
        "name: creds\ndotfiles:\n  - command: op read op://vault/private/token\n    \
         target: ~/.creds\nenvironments: {oops\n",
    )
    .unwrap();

    sandboxed_command(&temp_dir)
        .args(["spec", "list"])
        .assert()
        .success()
        .stdout(predicate::str::contains("creds.yml"))
        .stdout(predicate::str::contains("op://").not())
        .stdout(predicate::str::contains("vault").not());
}
