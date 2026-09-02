//! `selfie spec edit` opens the user's file without rewriting it first.
//!
//! These drive the real binary with `EDITOR=true`, an editor that exits 0 having
//! changed nothing, so what the file looks like afterwards is entirely down to
//! what selfie did to it before and after the editor ran.

pub mod common;

use common::{sandboxed_command, setup_default_test_config};
use predicates::prelude::PredicateBooleanExt;
use std::fs;

// Everything a serde round trip destroys, in one file: an anchor and its alias,
// comments at three depths, a blank line, and key ordering that does not match
// the struct's field order.
const HAND_WRITTEN: &str = r#"# myapp - the package I maintain by hand
name: myapp

_target: &shared ~/.config/myapp/config.toml

environments:
  # the machine I actually use
  test-env:
    install: "true"   # inert on purpose
    check: "true"

dotfiles:
  - source: config.toml
    target: *shared
"#;

fn write_package(temp: &tempfile::TempDir, name: &str, yaml: &str) -> std::path::PathBuf {
    let path = temp.path().join("packages").join(format!("{name}.yml"));
    fs::write(&path, yaml).unwrap();
    path
}

// The whole point of the unit: opening a file must not change it.
#[test]
fn spec_edit_leaves_an_existing_file_byte_identical() {
    let temp = setup_default_test_config();
    let path = write_package(&temp, "myapp", HAND_WRITTEN);

    sandboxed_command(&temp)
        .env("EDITOR", "true")
        .args(["spec", "edit", "myapp"])
        .assert()
        .success();

    // Byte-for-byte, not "parses the same". The anchor, the comments, the blank
    // line and the key order are exactly what a rewrite would have taken.
    assert_eq!(
        fs::read_to_string(&path).unwrap(),
        HAND_WRITTEN,
        "spec edit rewrote the file it was asked to open"
    );
}

// The file a typo guard rejects is the file the user is trying to fix, so
// refusing to open it is the one outcome that leaves them stuck.
#[test]
fn spec_edit_opens_a_file_carrying_a_key_selfie_would_refuse_to_write() {
    let temp = setup_default_test_config();
    let yaml = r#"name: myapp
_dotfiles:
  - source: config.toml
    target: ~/.config/myapp/config.toml
environments:
  test-env:
    install: "true"
"#;
    let path = write_package(&temp, "myapp", yaml);

    sandboxed_command(&temp)
        .env("EDITOR", "true")
        .args(["spec", "edit", "myapp"])
        .assert()
        .success();

    assert_eq!(fs::read_to_string(&path).unwrap(), yaml);
}

// `spec edit` is what a user reaches for when a file is broken, so treating a
// parse failure as "does not exist" is worst here: selfie offered to create,
// and on `y` wrote a template over the file they were trying to repair.
//
// The write itself sits behind a `dialoguer` confirm that needs a TTY, so this
// asserts the half a test can reach — that selfie no longer claims the file is
// absent — plus the file being untouched.
#[test]
fn spec_edit_does_not_call_an_unparsable_file_missing() {
    let temp = setup_default_test_config();
    let yaml = "{{{\n";
    let path = write_package(&temp, "myapp", yaml);

    sandboxed_command(&temp)
        .env("EDITOR", "true")
        .args(["spec", "edit", "myapp"])
        .assert()
        .failure()
        .stdout(predicates::str::contains("does not exist").not());

    assert_eq!(
        fs::read_to_string(&path).unwrap(),
        yaml,
        "the file must not be touched"
    );
}

// The control. A name with no file behind it must still be offered as a new
// package, or the guard above has broken `spec edit` for its other purpose.
#[test]
fn spec_edit_still_offers_to_create_a_package_with_no_file() {
    let temp = setup_default_test_config();

    sandboxed_command(&temp)
        .env("EDITOR", "true")
        .args(["spec", "edit", "brandnew"])
        .assert()
        .stdout(predicates::str::contains("does not exist"));
}

// The name folds, so `spec edit neovim` finds `Neovim.yml` and opens it rather
// than offering to create a second spec. This is the outcome the folding is
// for: that file used to be neither reachable by name nor safely replaceable,
// so the only thing `spec edit` could do with it was refuse.
//
// Uniform across file systems, so no probe: what changed is what a name means,
// not what the disk does with two spellings of one path.
#[test]
fn spec_edit_opens_a_spec_stored_under_another_case() {
    let temp = setup_default_test_config();
    let packages = temp.path().join("packages");
    let existing = packages.join("Neovim.yml");
    let yaml = "name: Neovim\nenvironments:\n  test-env:\n    install: \"brew install neovim\"\n";
    fs::write(&existing, yaml).unwrap();

    sandboxed_command(&temp)
        .env("EDITOR", "true")
        .args(["spec", "edit", "neovim"])
        .assert()
        .success()
        .stdout(predicates::str::contains("Opening existing package"));

    let mut entries: Vec<String> = fs::read_dir(&packages)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    entries.sort();
    assert_eq!(
        entries,
        vec!["Neovim.yml".to_string()],
        "opening a spec must not create a second one beside it"
    );
    assert_eq!(
        fs::read_to_string(&existing).unwrap(),
        yaml,
        "the existing file must not be replaced"
    );
}
