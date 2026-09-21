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
        // The status as well as the message: without a terminal the menu that
        // follows cannot be answered, so the run cancels and exits 0. A check
        // on the message alone would also pass on a run that printed it and
        // then failed for an unrelated reason.
        .success()
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

// A name whose Unicode normalization differs from the stored file's.
//
// Identity folds case and nothing else, so a spec stored in NFC is invisible to a
// lookup for the same name in NFD, while APFS resolves the NFD path onto that file.
// Name resolution says the name is free, the file system says the path is taken,
// and only the second is right. Without the guard the write replaces a regular file
// by rename and destroys a spec the user wrote.
//
// The load-bearing branch runs only on a normalization-insensitive volume, so CI on
// ubuntu takes the other one. Both assert; a skip would observe nothing.
#[test]
fn spec_create_does_not_replace_a_file_stored_under_another_normalization() {
    const NFC: &str = "na\u{ef}ve";
    const NFD: &str = "nai\u{308}ve";

    let temp = setup_default_test_config();
    let packages = temp.path().join("packages");
    let existing = packages.join(format!("{NFC}.yml"));
    let yaml = "name: naive\nenvironments:\n  test-env:\n    install: \"true\"\n";
    fs::write(&existing, yaml).unwrap();

    // Ask this file system rather than the platform name: a case-sensitive APFS
    // volume and a case-insensitive one both fold normalization, and ext4 folds
    // neither.
    let folds_normalization = packages.join(format!("{NFD}.yml")).exists();

    let assertion = sandboxed_command(&temp)
        .args(["spec", "create", NFD])
        .assert();

    if folds_normalization {
        // Distinct from the already-exists path, which cancels at a menu and exits
        // 0. Here no package answers to the name at all, so the run fails. The
        // output is asserted as well as the status, because nearly every failure
        // mode exits non-zero, including never reaching the guard.
        //
        // Deliberately not asserting the sentence about capitalization: that blames
        // the one cause the name fold excludes, and is being corrected separately.
        assertion
            .failure()
            .stderr(predicates::str::contains("is already taken"))
            .stderr(predicates::str::contains(
                "though no package answers to that name",
            ));

        let mut entries: Vec<String> = fs::read_dir(&packages)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        entries.sort();
        assert_eq!(
            entries.len(),
            1,
            "the create must not add a second file: {entries:?}"
        );
        assert_eq!(
            fs::read_to_string(&existing).unwrap(),
            yaml,
            "the existing spec must survive byte for byte"
        );
    } else {
        // Two distinct paths, so nothing was at risk and the create is ordinary.
        // Asserted rather than skipped: this half proves the guard does not refuse
        // a name that merely looks similar.
        assertion
            .success()
            .stdout(predicates::str::contains("created"));
        assert!(
            packages.join(format!("{NFD}.yml")).exists(),
            "on a normalization-sensitive file system the new spec is its own file"
        );
        assert_eq!(
            fs::read_to_string(&existing).unwrap(),
            yaml,
            "the existing spec must survive byte for byte"
        );
    }
}
