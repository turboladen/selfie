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

// Whether the packages directory distinguishes `Neovim.yml` from `neovim.yml`.
// The guard only has something to refuse when it does not, and CI runs Linux,
// where it does -- so without this the test below would assert nothing there and
// stay green with the guard deleted.
fn packages_dir_is_case_sensitive(temp: &tempfile::TempDir) -> bool {
    let probe = temp.path().join("packages").join("CaseProbe.yml");
    fs::write(&probe, "probe").unwrap();
    let sensitive = !temp.path().join("packages").join("caseprobe.yml").exists();
    fs::remove_file(&probe).unwrap();
    sensitive
}

// A file stored under a different capitalization is invisible to the name check,
// and on a case-insensitive file system the write resolves to it and truncates
// it. On a case-sensitive one the two are different files and the create
// legitimately succeeds. Both outcomes are asserted, so neither file system
// leaves the guard unexercised.
#[test]
fn spec_create_does_not_replace_a_file_stored_under_another_case() {
    let temp = setup_default_test_config();
    let case_sensitive = packages_dir_is_case_sensitive(&temp);
    let existing = temp.path().join("packages").join("Neovim.yml");
    let yaml = "name: Neovim\nenvironments:\n  test-env:\n    install: \"brew install neovim\"\n";
    fs::write(&existing, yaml).unwrap();

    let assertion = sandboxed_command(&temp)
        .args(["spec", "create", "neovim"])
        .assert();

    let created = temp.path().join("packages").join("neovim.yml");
    if case_sensitive {
        // Two genuinely different files, so the create takes nothing.
        assertion.success();
        assert!(
            created.exists(),
            "a create that collides with nothing must still write"
        );
    } else {
        // One file under two spellings, so the create has to be refused.
        //
        // Asserting the message as well as the status: almost every way this
        // could fail exits non-zero and leaves the file intact, including a run
        // that never reaches the guard, so a status check alone would pass on a
        // missing config or a panic.
        assertion
            .failure()
            .stderr(predicates::str::contains("already taken"));
    }

    assert_eq!(
        fs::read_to_string(&existing).unwrap(),
        yaml,
        "the existing file must not be replaced"
    );
}
