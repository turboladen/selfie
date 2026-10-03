// A command that needs to ask and has no terminal to ask on ends as a usage
// error (exit 2), names what to run instead, and changes nothing.
//
// The test process gives `selfie` no terminal on stderr, which is what the
// prompt door checks, so every run here is the no-terminal case.

pub mod common;

use std::fs;

use common::{SELFIE_ENV, package_repo_with_remote, run_sandboxed, setup_default_test_config};

const USAGE: Option<i32> = Some(2);

fn write_spec(temp: &tempfile::TempDir, name: &str) -> std::path::PathBuf {
    let path = temp.path().join("packages").join(format!("{name}.yml"));
    fs::write(
        &path,
        format!("name: {name}\nenvironments:\n  {SELFIE_ENV}:\n    install: \"true\"\n"),
    )
    .unwrap();
    path
}

fn package_files(temp: &tempfile::TempDir) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(temp.path().join("packages"))
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

// Refused before the name check or any prompt text, so the run says only why.
#[test]
fn spec_create_interactive_without_a_terminal_is_a_usage_error() {
    let temp = setup_default_test_config();

    let out = run_sandboxed(&temp, &["spec", "create", "fresh", "--interactive"]);

    assert_eq!(out.code, USAGE, "{}{}", out.stdout, out.stderr);
    assert!(
        out.stderr
            .contains("spec create --interactive needs a terminal"),
        "{}",
        out.stderr
    );
    assert!(
        out.stderr.contains("Leave off --interactive"),
        "{}",
        out.stderr
    );
    assert_eq!(out.stdout, "");
    assert!(
        package_files(&temp).is_empty(),
        "{:?}",
        package_files(&temp)
    );
}

#[test]
fn spec_edit_of_a_new_name_without_a_terminal_is_a_usage_error() {
    let temp = setup_default_test_config();

    let out = common::sandboxed_std_command(&temp)
        .env("EDITOR", "true")
        .args(["spec", "edit", "fresh"])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);

    assert_eq!(out.status.code(), USAGE, "{stderr}");
    assert!(stderr.contains("needs a terminal"), "{stderr}");
    assert!(stderr.contains("selfie spec create fresh"), "{stderr}");
    assert!(
        package_files(&temp).is_empty(),
        "{:?}",
        package_files(&temp)
    );
}

#[test]
fn spec_remove_without_a_terminal_is_a_usage_error_and_keeps_the_file() {
    let temp = setup_default_test_config();
    let path = write_spec(&temp, "tool");

    let out = run_sandboxed(&temp, &["spec", "remove", "tool"]);

    assert_eq!(out.code, USAGE, "{}{}", out.stdout, out.stderr);
    assert!(out.stderr.contains("Pass --yes"), "{}", out.stderr);
    assert!(path.exists(), "the spec must not be removed unasked");
}

// The commit-message prompt is the only one sync push has; --yes skips it.
#[test]
fn sync_push_without_a_terminal_is_a_usage_error_and_commits_nothing() {
    let temp = setup_default_test_config();
    write_spec(&temp, "bat");
    package_repo_with_remote(&temp);
    write_spec(&temp, "fd");
    let head = || {
        std::process::Command::new("git")
            .arg("-C")
            .arg(temp.path().join("packages"))
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap()
            .stdout
    };
    let before = head();

    let out = run_sandboxed(&temp, &["sync", "push"]);

    assert_eq!(out.code, USAGE, "{}{}", out.stdout, out.stderr);
    assert!(out.stderr.contains("Pass --yes"), "{}", out.stderr);
    // The preview exists to be confirmed, so a run that cannot ask prints none.
    assert_eq!(out.stdout, "");
    assert_eq!(head(), before, "nothing may be committed unasked");
}
