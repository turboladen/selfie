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

// A spec stored under a different capitalization answers to the folded name, so
// the create finds `Neovim.yml` and declines instead of writing a second file.
//
// No file system probe here, unlike the version this replaces: the refusal now
// comes from the name index rather than from asking the disk whether the path
// is taken, so it does not depend on how the disk compares names. The same
// outcome on Linux and on APFS is the point of the change.
#[test]
fn spec_create_does_not_replace_a_file_stored_under_another_case() {
    let temp = setup_default_test_config();
    let packages = temp.path().join("packages");
    let existing = packages.join("Neovim.yml");
    let yaml = "name: Neovim\nenvironments:\n  test-env:\n    install: \"brew install neovim\"\n";
    fs::write(&existing, yaml).unwrap();

    sandboxed_command(&temp)
        .args(["spec", "create", "neovim"])
        .assert()
        .stdout(predicates::str::contains("already exists"));

    // Listing the directory rather than testing `neovim.yml.exists()`, which is
    // true on a case-insensitive file system whether or not anything was
    // written, and would report a pass for the write it is meant to catch.
    let mut entries: Vec<String> = fs::read_dir(&packages)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    entries.sort();
    assert_eq!(
        entries,
        vec!["Neovim.yml".to_string()],
        "a name that already resolves must not gain a second file"
    );
    assert_eq!(
        fs::read_to_string(&existing).unwrap(),
        yaml,
        "the existing file must not be replaced"
    );
}
