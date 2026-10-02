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

// The name typed on the command line reaches the library's spec-name rule, so
// the CLI cannot write a spec every later command refuses to load. The
// interactive prompts build their package the same way and hand it to the same
// service call, which is where the rule is applied.
#[test]
fn spec_create_refuses_a_name_the_loader_would_refuse() {
    let temp = setup_default_test_config();
    let packages = temp.path().join("packages");

    sandboxed_command(&temp)
        .args(["spec", "create", "my tool"])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "Refusing to create 'my tool': it is not a valid spec name",
        ));

    assert!(
        !packages.join("my tool.yml").exists(),
        "a refused name must write nothing"
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
        // follows cannot be answered, so the run declines and exits 1, since it
        // wrote nothing. A check on the message alone would also pass on a run
        // that printed it and then failed for an unrelated reason.
        .code(1)
        .stderr(predicates::str::contains("'neovim' is already a package"));

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
// lookup for the same name in NFD, while APFS resolves the NFD path onto that file:
// a create that reached the write would replace a spec the user wrote. The
// decomposed name carries a combining mark, which the spec-name rule does not
// admit, so the create is refused before any lookup, on every file system alike.
// The occupied-path guard behind it keeps its own unit test.
#[test]
fn spec_create_does_not_replace_a_file_stored_under_another_normalization() {
    const NFC: &str = "na\u{ef}ve";
    const NFD: &str = "nai\u{308}ve";

    let temp = setup_default_test_config();
    let packages = temp.path().join("packages");
    let existing = packages.join(format!("{NFC}.yml"));
    let yaml = "name: naive\nenvironments:\n  test-env:\n    install: \"true\"\n";
    fs::write(&existing, yaml).unwrap();

    sandboxed_command(&temp)
        .args(["spec", "create", NFD])
        .assert()
        .failure()
        .stderr(predicates::str::contains("not a valid spec name"));

    let entries: Vec<String> = fs::read_dir(&packages)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
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
}

// A name a package spec already has is reported as a package, even when that
// spec cannot be loaded: where the name was found decides the sentence.
#[test]
fn spec_create_over_an_unloadable_package_reports_a_package() {
    let temp_dir = setup_default_test_config();
    fs::write(
        temp_dir.path().join("packages").join("vim.yaml"),
        "name: [unterminated\n",
    )
    .unwrap();

    let output = sandboxed_command(&temp_dir)
        .args(["--no-color", "spec", "create", "vim"])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(stderr.contains("'vim' is already a package"), "{stderr}");
    assert!(stderr.contains("could not be loaded"), "{stderr}");
    assert!(!stderr.contains("dotfile spec"), "{stderr}");
}
