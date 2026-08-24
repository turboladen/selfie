//! `selfie spec edit` opens the user's file without rewriting it first.
//!
//! These drive the real binary with `EDITOR=true`, an editor that exits 0 having
//! changed nothing, so what the file looks like afterwards is entirely down to
//! what selfie did to it before and after the editor ran.

pub mod common;

use common::{sandboxed_command, setup_default_test_config};
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
