//! These tests run selfie's dynamic shell completion against the package names
//! in the configured package directory.
//!
//! They drive the real binary, through the protocol a shell uses on each TAB
//! press, because what is under test is which configuration file the completer
//! finds and how it reads it.

use std::path::Path;
use std::process::Output;
use std::time::Duration;

pub mod common;

use common::{CLEAN, config_dir_with_fifo, sandboxed_command};

// Every fixture name starts with this, and every test completes it. A word that
// does not start with `-` gets no flag candidates, so the output holds package
// names and nothing else.
const PREFIX: &str = "pkg-";

// Writes `config.yaml` into `config_dir`, creating the directory.
fn write_config(config_dir: &Path, yaml: &str) {
    std::fs::create_dir_all(config_dir).unwrap();
    std::fs::write(config_dir.join("config.yaml"), yaml).unwrap();
}

// Creates `dir` holding an empty `<name>.yaml` for each of `names`.
fn write_packages(dir: &Path, names: &[&str]) {
    std::fs::create_dir_all(dir).unwrap();
    for name in names {
        std::fs::write(dir.join(format!("{name}.yaml")), "").unwrap();
    }
}

fn config_naming(package_directory: &Path) -> String {
    format!(
        "environment: test-env\npackage_directory: {}\n",
        package_directory.display()
    )
}

// A `config.yaml` in `config_dir` naming `package_directory`, which holds a
// package for each of `names`.
fn fixture(config_dir: &Path, package_directory: &Path, names: &[&str]) {
    write_config(config_dir, &config_naming(package_directory));
    write_packages(package_directory, names);
}

// Asks `cmd` to complete `word` as the package name of `spec edit`, the way fish
// does.
fn complete(cmd: assert_cmd::Command, word: &str) -> Output {
    complete_after(cmd, &["spec", "edit"], word)
}

// Asks `cmd` to complete `word` as the next argument of `subcommand`, the way
// fish does. The timeout turns a completer that blocks into a failed test.
fn complete_after(mut cmd: assert_cmd::Command, subcommand: &[&str], word: &str) -> Output {
    cmd.timeout(Duration::from_secs(30))
        .env("COMPLETE", "fish")
        .args(["--", "selfie"])
        .args(subcommand)
        .arg(word)
        .output()
        .unwrap()
}

// The candidates `output` offered, sorted, after asserting that completion
// printed nothing to stderr and exited cleanly.
fn candidates(output: &Output) -> Vec<String> {
    assert_eq!(String::from_utf8_lossy(&output.stderr), "");
    assert_eq!(output.status.code(), Some(CLEAN));
    let mut names: Vec<String> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(String::from)
        .collect();
    names.sort();
    names
}

// The two routes name different configurations, so only the one completion
// reads can account for the name it offers.
#[test]
fn completion_reads_the_config_selfie_config_dir_names() {
    let temp = tempfile::tempdir().unwrap();
    let chosen = temp.path().join("elsewhere");
    let xdg = temp.path().join("xdg");
    fixture(&chosen, &temp.path().join("pkgs-a"), &["pkg-alpha"]);
    fixture(
        &xdg.join("selfie"),
        &temp.path().join("pkgs-b"),
        &["pkg-bravo"],
    );

    let mut cmd = sandboxed_command(&temp);
    cmd.env("SELFIE_CONFIG_DIR", &chosen)
        .env("XDG_CONFIG_HOME", &xdg);

    assert_eq!(candidates(&complete(cmd, PREFIX)), ["pkg-alpha"]);
}

// `XDG_CONFIG_HOME` sits outside `$HOME/.config`, so a completer that skipped it
// for the `HOME` fallback would find nothing.
#[test]
fn completion_falls_back_to_xdg_config_home() {
    let temp = tempfile::tempdir().unwrap();
    let xdg = temp.path().join("xdg");
    fixture(
        &xdg.join("selfie"),
        &temp.path().join("pkgs-b"),
        &["pkg-bravo"],
    );

    let mut cmd = sandboxed_command(&temp);
    cmd.env_remove("SELFIE_CONFIG_DIR")
        .env("XDG_CONFIG_HOME", &xdg);

    assert_eq!(candidates(&complete(cmd, PREFIX)), ["pkg-bravo"]);
}

// The fixture is the layout `docs/configuration.md` prescribes, with a `~` in
// `package_directory`. Its `cli:` is a mapping and its `command_timeout` a
// number, so a reader that accepted only strings would refuse the whole file
// and every TAB press would come back empty.
#[test]
fn completion_expands_a_tilde_in_the_documented_layout() {
    let temp = tempfile::tempdir().unwrap();
    write_config(
        &temp.path().join(".config/selfie"),
        "environment: macos\npackage_directory: ~/pkgs\n\
         command_timeout: 60\ncli:\n  verbose: true\n  use_colors: false\n",
    );
    write_packages(&temp.path().join("pkgs"), &["pkg-alpha", "pkg-bravo"]);

    assert_eq!(
        candidates(&complete(sandboxed_command(&temp), PREFIX)),
        ["pkg-alpha", "pkg-bravo"]
    );
}

// A command can take its environment from `--environment`, so a file without one
// still names the package directory every command uses.
#[test]
fn completion_does_not_need_an_environment() {
    let temp = tempfile::tempdir().unwrap();
    write_config(
        &temp.path().join(".config/selfie"),
        &format!(
            "package_directory: {}\n",
            temp.path().join("pkgs").display()
        ),
    );
    write_packages(&temp.path().join("pkgs"), &["pkg-alpha"]);

    assert_eq!(
        candidates(&complete(sandboxed_command(&temp), PREFIX)),
        ["pkg-alpha"]
    );
}

// A control: with nothing to read, completion offers nothing and says nothing.
#[test]
fn completion_offers_nothing_without_a_config() {
    let temp = tempfile::tempdir().unwrap();

    assert!(candidates(&complete(sandboxed_command(&temp), PREFIX)).is_empty());
}

// Every command refuses to choose between the two files, so completion does not
// choose either.
#[test]
fn completion_offers_nothing_when_both_config_files_exist() {
    let temp = tempfile::tempdir().unwrap();
    let config_dir = temp.path().join(".config/selfie");
    fixture(&config_dir, &temp.path().join("pkgs-a"), &["pkg-alpha"]);
    std::fs::write(
        config_dir.join("config.yml"),
        config_naming(&temp.path().join("pkgs-b")),
    )
    .unwrap();
    write_packages(&temp.path().join("pkgs-b"), &["pkg-bravo"]);

    assert!(candidates(&complete(sandboxed_command(&temp), PREFIX)).is_empty());
}

// Every command fails on this file. Completion offers nothing, and prints nothing
// into the user's shell either.
#[test]
fn completion_offers_nothing_for_a_config_that_does_not_load() {
    let temp = tempfile::tempdir().unwrap();
    write_config(
        &temp.path().join(".config/selfie"),
        &format!(
            "{}command_timeout: soon\n",
            config_naming(&temp.path().join("pkgs"))
        ),
    );
    write_packages(&temp.path().join("pkgs"), &["pkg-alpha"]);

    assert!(candidates(&complete(sandboxed_command(&temp), PREFIX)).is_empty());
}

#[test]
fn completion_offers_only_names_with_the_typed_prefix() {
    let temp = tempfile::tempdir().unwrap();
    fixture(
        &temp.path().join(".config/selfie"),
        &temp.path().join("pkgs"),
        &["pkg-alpha", "pkg-bravo"],
    );

    assert_eq!(
        candidates(&complete(sandboxed_command(&temp), "pkg-a")),
        ["pkg-alpha"]
    );
}

// A package directory that is a file cannot be listed. Completion offers
// nothing, and prints nothing into the user's shell.
#[test]
fn completion_offers_nothing_when_the_package_directory_is_a_file() {
    let temp = tempfile::tempdir().unwrap();
    let not_a_directory = temp.path().join("pkgs");
    std::fs::write(&not_a_directory, "").unwrap();
    write_config(
        &temp.path().join(".config/selfie"),
        &config_naming(&not_a_directory),
    );

    assert!(candidates(&complete(sandboxed_command(&temp), PREFIX)).is_empty());
}

// Every command folds a package name to lower case. The typed word is folded
// the same way, so `pkg-Neo` still finds the spec `pkg-Neovim.yaml`.
#[test]
fn completion_matches_the_typed_word_ignoring_case() {
    let temp = tempfile::tempdir().unwrap();
    fixture(
        &temp.path().join(".config/selfie"),
        &temp.path().join("pkgs"),
        &["pkg-Neovim"],
    );

    assert_eq!(
        candidates(&complete(sandboxed_command(&temp), "pkg-Neo")),
        ["pkg-neovim"]
    );
}

// Every package-name argument registers the same completer; this one stands in
// for those other than `spec edit`.
#[test]
fn completion_offers_names_for_package_install() {
    let temp = tempfile::tempdir().unwrap();
    fixture(
        &temp.path().join(".config/selfie"),
        &temp.path().join("pkgs"),
        &["pkg-alpha"],
    );

    assert_eq!(
        candidates(&complete_after(
            sandboxed_command(&temp),
            &["package", "install"],
            PREFIX
        )),
        ["pkg-alpha"]
    );
}

// Opening a fifo blocks until a writer arrives, which would hang the user's
// shell on every TAB press.
#[cfg(unix)]
#[test]
fn completion_does_not_hang_on_a_fifo_config() {
    let temp = config_dir_with_fifo();

    assert!(candidates(&complete(sandboxed_command(&temp), PREFIX)).is_empty());
}
