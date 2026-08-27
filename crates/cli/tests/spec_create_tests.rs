//! `selfie spec create` against a real file system.
//!
//! The unit tests mock `path_is_occupied`, so they cannot see the real
//! implementation. These drive the binary, which is what makes a guard that
//! refuses every create visible.

pub mod common;

use common::{sandboxed_command, setup_default_test_config};
use std::fs;

// The control the unit tests cannot provide: a genuinely new name must still be
// created. A `path_is_occupied` stuck at `true` passes every mocked test and
// fails only here.
#[test]
fn spec_create_writes_a_package_that_does_not_exist() {
    let temp = setup_default_test_config();
    let path = temp.path().join("packages").join("brandnew.yml");
    assert!(!path.exists());

    sandboxed_command(&temp)
        .args(["spec", "create", "brandnew"])
        .assert()
        .success();

    assert!(
        path.exists(),
        "a genuinely new package must still be created"
    );
}

// A file stored under a different capitalization is invisible to the name
// check, and on a case-insensitive file system the write resolves to it and
// truncates it. On a case-sensitive one the two are different files and the
// create legitimately succeeds -- so this asserts the file is intact either way
// rather than asserting which branch was taken.
#[test]
fn spec_create_does_not_replace_a_file_stored_under_another_case() {
    let temp = setup_default_test_config();
    let existing = temp.path().join("packages").join("Neovim.yml");
    let yaml = "name: Neovim\nenvironments:\n  test-env:\n    install: \"brew install neovim\"\n";
    fs::write(&existing, yaml).unwrap();

    // The exit status is deliberately not asserted: it differs by file system,
    // and which branch ran is not the property under test. That the existing
    // file is intact is true on both.
    let _ = sandboxed_command(&temp)
        .args(["spec", "create", "neovim"])
        .assert();

    assert_eq!(
        fs::read_to_string(&existing).unwrap(),
        yaml,
        "the existing file must not be replaced"
    );
}
