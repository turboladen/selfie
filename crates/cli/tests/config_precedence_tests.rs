//! These tests check which directory each command actually reads when a flag
//! and the configuration file disagree.
//!
//! The unit tests over `build_cli_config` prove that function applies its
//! overrides. They would all still pass if nothing downstream ever received the
//! result, so these drive the real binary instead and assert on what it printed
//! or wrote.
//!
//! Every test runs under `sandboxed_command`, which is also what makes them
//! safe: without it they would read the developer's own config and write their
//! own home directory.

pub mod common;

use std::{fs, path::Path};

use common::{SELFIE_ENV, sandboxed_command};
use predicates::prelude::*;
use tempfile::TempDir;

// A fixture with two of everything, so an applied override and an ignored one
// look different. One package directory named in the config file and one
// reachable only through `-p`; one dotfiles directory found by the sibling
// default and one reachable only through `--dotfiles-directory`.
fn fixture() -> TempDir {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();

    for dir in [
        ".config/selfie",
        "config-packages/sentinel",
        "flag-packages",
        "dotfiles",
        "flag-dotfiles",
        "flag-state",
        "config-dotfiles",
        "config-state",
    ] {
        fs::create_dir_all(root.join(dir)).unwrap();
    }

    fs::write(
        root.join(".config/selfie/config.yaml"),
        format!(
            "environment: {SELFIE_ENV}\npackage_directory: {}\n",
            root.join("config-packages").display()
        ),
    )
    .unwrap();

    write_package(
        root,
        "config-packages/from-config-pkg.yaml",
        "from-config-pkg",
    );
    write_package(root, "flag-packages/from-flag-pkg.yaml", "from-flag-pkg");

    // The `~` target is the one thing no path flag redirects, so the fixture
    // always carries one.
    fs::write(root.join("config-packages/sentinel/file.txt"), "sentinel\n").unwrap();
    write_dotfile_package(
        root,
        "config-packages/sentinel-pkg.yaml",
        "sentinel-pkg",
        "sentinel/file.txt",
        "~/sentinel-target",
    );

    fs::write(root.join("dotfiles/default-src.txt"), "default\n").unwrap();
    write_dotfile_package(
        root,
        "dotfiles/default-dotfile.yaml",
        "default-dotfile",
        "default-src.txt",
        "~/default-dotfile-target",
    );

    fs::write(root.join("flag-dotfiles/flagged-src.txt"), "flagged\n").unwrap();
    write_dotfile_package(
        root,
        "flag-dotfiles/flagged-dotfile.yaml",
        "flagged-dotfile",
        "flagged-src.txt",
        "~/flagged-dotfile-target",
    );

    fs::write(root.join("config-dotfiles/config-src.txt"), "from config\n").unwrap();
    write_dotfile_package(
        root,
        "config-dotfiles/config-dotfile.yaml",
        "config-dotfile",
        "config-src.txt",
        "~/config-dotfile-target",
    );

    temp
}

// The same fixture with both optional directories named in the config file.
//
// Without it, the flag tests for `--dotfiles-directory` and `--state-directory`
// only prove the flag beats a *derived default*: the file states no opinion on
// either field, so "the flag won" and "the file won" produce the same directory
// and neither test can tell them apart. This is the axis those two flags have to
// vary along.
fn fixture_naming_both_directories_in_the_config_file() -> TempDir {
    let temp = fixture();
    let root = temp.path();

    let config = root.join(".config/selfie/config.yaml");
    let mut yaml = fs::read_to_string(&config).unwrap();
    yaml.push_str(&format!(
        "dotfiles_directory: {}\nstate_directory: {}\n",
        root.join("config-dotfiles").display(),
        root.join("config-state").display()
    ));
    fs::write(&config, yaml).unwrap();

    temp
}

// `true` rather than anything observable: the sandbox does not sandbox
// execution, so whatever goes here runs for real on the machine under test.
fn package_yaml(name: &str) -> String {
    format!("name: {name}\nenvironments:\n  {SELFIE_ENV}:\n    install: \"true\"\n")
}

fn write_package(root: &Path, path: &str, name: &str) {
    fs::write(root.join(path), package_yaml(name)).unwrap();
}

fn write_dotfile_package(root: &Path, path: &str, name: &str, source: &str, target: &str) {
    fs::write(
        root.join(path),
        format!(
            "{}dotfiles:\n  - source: {source}\n    target: {target}\n",
            package_yaml(name)
        ),
    )
    .unwrap();
}

#[test]
fn the_package_directory_flag_beats_the_config_file() {
    let temp = fixture();

    sandboxed_command(&temp)
        .arg("-p")
        .arg(temp.path().join("flag-packages"))
        .args(["package", "list"])
        .assert()
        .success()
        .stdout(predicate::str::contains("from-flag-pkg"))
        .stdout(predicate::str::contains("from-config-pkg").not());
}

// The flag is global, so clap accepts it on either side of the subcommand. A
// user who puts it on the right and gets the config file's directory has no way
// to tell that from the flag being ignored altogether.
#[test]
fn the_package_directory_flag_works_after_the_subcommand() {
    let temp = fixture();

    sandboxed_command(&temp)
        .args(["package", "list", "-p"])
        .arg(temp.path().join("flag-packages"))
        .assert()
        .success()
        .stdout(predicate::str::contains("from-flag-pkg"))
        .stdout(predicate::str::contains("from-config-pkg").not());
}

// The control. Without it the two tests above could pass on a build that reads
// the flag directory unconditionally and never consults the config file at all.
#[test]
fn without_a_flag_the_config_files_package_directory_decides() {
    let temp = fixture();

    sandboxed_command(&temp)
        .args(["package", "list"])
        .assert()
        .success()
        .stdout(predicate::str::contains("from-config-pkg"))
        .stdout(predicate::str::contains("from-flag-pkg").not());
}

#[test]
fn the_dotfiles_directory_flag_beats_the_sibling_default() {
    let temp = fixture();

    sandboxed_command(&temp)
        .arg("--dotfiles-directory")
        .arg(temp.path().join("flag-dotfiles"))
        .args(["dotfiles", "list"])
        .assert()
        .success()
        .stdout(predicate::str::contains("flagged-src.txt"))
        .stdout(predicate::str::contains("default-src.txt").not());
}

// Asserts on names, not on emptiness: a dotfiles directory that does not exist
// is dropped without any message, so an empty listing cannot be told apart from
// one the flag redirected correctly.
#[test]
fn without_a_flag_the_sibling_dotfiles_directory_decides() {
    let temp = fixture();

    sandboxed_command(&temp)
        .args(["dotfiles", "list"])
        .assert()
        .success()
        .stdout(predicate::str::contains("default-src.txt"))
        .stdout(predicate::str::contains("flagged-src.txt").not());
}

// The pair above only pits the flag against a derived default. This pits it
// against a directory the file names outright, which is the case the flag exists
// for and the one a wrong-way merge would break.
#[test]
fn the_dotfiles_directory_flag_beats_the_config_file() {
    let temp = fixture_naming_both_directories_in_the_config_file();

    sandboxed_command(&temp)
        .arg("--dotfiles-directory")
        .arg(temp.path().join("flag-dotfiles"))
        .args(["dotfiles", "list"])
        .assert()
        .success()
        .stdout(predicate::str::contains("flagged-src.txt"))
        .stdout(predicate::str::contains("config-src.txt").not());
}

// Its control, and the proof that the file's own value is reachable — without
// it, the test above could pass on a build that ignores the file entirely.
// Asserting the sibling default is absent too pins the second half of the order.
#[test]
fn without_a_flag_the_config_files_dotfiles_directory_decides() {
    let temp = fixture_naming_both_directories_in_the_config_file();

    sandboxed_command(&temp)
        .args(["dotfiles", "list"])
        .assert()
        .success()
        .stdout(predicate::str::contains("config-src.txt"))
        .stdout(predicate::str::contains("flagged-src.txt").not())
        .stdout(predicate::str::contains("default-src.txt").not());
}

// The control the four state tests below share. Each of them is about *where*
// the file landed, and `apply` writes one whether or not it deployed anything —
// a run with no entries leaves `deployed: {}` behind. Asserting existence alone
// therefore stays green on a fixture that stopped loading packages at all: a
// renamed environment, an unparsable spec, a moved source. Naming the entry is
// what makes the four prove precedence rather than the mere reachability of a
// write.
fn assert_state_records_the_fixture(state_file: &Path) {
    let state = fs::read_to_string(state_file).unwrap_or_else(|e| {
        panic!(
            "deploy state should be readable at {}: {e}",
            state_file.display()
        )
    });
    assert!(
        state.contains("sentinel/file.txt"),
        "deploy state at {} should record the fixture's dotfile rather than be empty; got:\n{state}",
        state_file.display()
    );
}

#[test]
fn the_state_directory_flag_decides_where_deploy_state_lands() {
    let temp = fixture();

    sandboxed_command(&temp)
        .arg("--state-directory")
        .arg(temp.path().join("flag-state"))
        .args(["apply", "-y"])
        .assert()
        .success();

    assert_state_records_the_fixture(&temp.path().join("flag-state/deploy-state.yml"));
    // Paired with the assertion above: on its own, an absent fallback file is
    // also what a run that never reached the state write would leave behind.
    assert!(
        !temp.path().join(".local/state/selfie").exists(),
        "the flag should have kept deploy state out of the home fallback"
    );
}

// The other half of the pair, and the proof that the fallback is reachable at
// all. It also demonstrates why `HOME` has to be part of the sandbox: without
// the flag, deploy state is written under the home directory.
#[test]
fn without_a_flag_deploy_state_lands_under_home() {
    let temp = fixture();

    sandboxed_command(&temp)
        .args(["apply", "-y"])
        .assert()
        .success();

    assert_state_records_the_fixture(&temp.path().join(".local/state/selfie/deploy-state.yml"));
    // The negative half its three siblings each carry: without it the test also
    // passes on a build that writes the state file everywhere it can name.
    assert!(
        !temp.path().join("flag-state/deploy-state.yml").exists(),
        "the fallback should be the only place deploy state was written"
    );
}

// Same axis as the dotfiles pair: the two tests above only pit the flag against
// the home fallback, which a build that ignored `state_directory` in the file
// would also satisfy.
#[test]
fn the_state_directory_flag_beats_the_config_file() {
    let temp = fixture_naming_both_directories_in_the_config_file();

    sandboxed_command(&temp)
        .arg("--state-directory")
        .arg(temp.path().join("flag-state"))
        .args(["apply", "-y"])
        .assert()
        .success();

    assert_state_records_the_fixture(&temp.path().join("flag-state/deploy-state.yml"));
    assert!(
        !temp.path().join("config-state/deploy-state.yml").exists(),
        "the flag should have kept deploy state out of the file's directory"
    );
}

// Its control. Also pins the rest of the order for this field: the file's value
// beats the home fallback.
#[test]
fn without_a_flag_the_config_files_state_directory_decides() {
    let temp = fixture_naming_both_directories_in_the_config_file();

    sandboxed_command(&temp)
        .args(["apply", "-y"])
        .assert()
        .success();

    assert_state_records_the_fixture(&temp.path().join("config-state/deploy-state.yml"));
    assert!(
        !temp.path().join(".local/state/selfie").exists(),
        "the file's directory should have kept deploy state out of the home fallback"
    );
}

// No path flag redirects a `~` dotfile target; only `HOME` does. This is the
// property the whole sandbox rests on, so it gets its own test rather than
// being assumed by the others.
#[test]
fn a_tilde_target_is_written_under_the_sandboxed_home() {
    let temp = fixture();

    sandboxed_command(&temp)
        .arg("--state-directory")
        .arg(temp.path().join("flag-state"))
        .args(["apply", "-y"])
        .assert()
        .success();

    let target = temp.path().join("sentinel-target");
    assert!(
        target.exists(),
        "a ~ target should resolve against the sandboxed HOME"
    );
    assert_eq!(fs::read_to_string(target).unwrap(), "sentinel\n");
}

// Deliberate, and easy to mistake for a precedence bug: `config validate`
// reloads the file so that a flag cannot mask a problem in what is on disk.
// Anyone using it to confirm their flags took effect is reading the wrong
// answer, which is what this pins.
#[test]
fn config_validate_reports_the_file_not_the_flag() {
    let temp = fixture();

    sandboxed_command(&temp)
        .arg("-p")
        .arg(temp.path().join("flag-packages"))
        .args(["config", "validate"])
        .assert()
        .success()
        .stdout(predicate::str::contains("config-packages"))
        .stdout(predicate::str::contains("flag-packages").not());
}
