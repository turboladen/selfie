//! `selfie apply` against a package file whose unknown-key check cannot run.
//!
//! The check re-reads the file into a map of keys, and a file that parses as a
//! package can still fail that read. Apply refuses such a package whether or
//! not it has entries, because the key that could not be ruled out is what
//! decides whether those entries are the right ones.
//!
//! The exit status is the half the library tests cannot see, and the wording is
//! the half no control-flow assertion can.

pub mod common;

use common::{SELFIE_ENV, sandboxed_command, setup_default_test_config};

// The construct that fails the re-read while the package itself still parses:
// `serde_json::Value` has no key for a mapping keyed by a sequence.
//
// Written once, because every test here depends on it failing that read, and two
// copies drifting apart would leave one test silently exercising nothing.
const UNREADABLE_TOP_LEVEL: &str = "extra:\n  ? [a, b]\n  : v\n";

// A package that parses, deploys one dotfile, and cannot be read back into the
// key map.
fn write_package(base: &tempfile::TempDir) {
    let packages = base.path().join("packages");
    std::fs::create_dir_all(packages.join("myapp")).unwrap();
    std::fs::write(packages.join("myapp/config.toml"), "REPO\n").unwrap();

    std::fs::write(
        packages.join("myapp.yaml"),
        format!(
            r#"name: myapp
{UNREADABLE_TOP_LEVEL}dotfiles:
  - source: "myapp/config.toml"
    target: "~/.config/myapp/config.toml"
environments:
  {SELFIE_ENV}:
    install: "echo i"
"#
        ),
    )
    .unwrap();
}

#[test]
fn apply_fails_the_run_for_an_unchecked_file_that_has_dotfiles() {
    let temp = setup_default_test_config();
    write_package(&temp);

    let output = sandboxed_command(&temp)
        .args(["apply", "-y"])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let text = format!("{stderr}{stdout}");

    assert_eq!(
        output.status.code(),
        Some(1),
        "a refusal must reach the exit status, got:\n{text}"
    );

    let deployed = temp.path().join(".config/myapp/config.toml");
    assert!(
        !deployed.exists(),
        "a refused package must deploy nothing, but '{}' contains {:?}",
        deployed.display(),
        std::fs::read_to_string(&deployed).ok()
    );

    assert!(
        text.contains("Skipping package 'myapp'") && text.contains("cannot be ruled out"),
        "the run must name the package and what could not be ruled out, got:\n{text}"
    );
}

// The same file with no dotfiles behind it. Reporting this as a plain success
// is the count that means both "nothing to do" and "selfie declined", so the
// exit status has to say declined here as much as it does above.
#[test]
fn apply_fails_the_run_when_an_unchecked_file_deploys_nothing() {
    let temp = setup_default_test_config();
    std::fs::write(
        temp.path().join("packages/myapp.yaml"),
        format!(
            r#"name: myapp
{UNREADABLE_TOP_LEVEL}environments:
  {SELFIE_ENV}:
    install: "echo i"
"#
        ),
    )
    .unwrap();

    let output = sandboxed_command(&temp)
        .args(["apply", "-y"])
        .output()
        .unwrap();
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stderr),
        String::from_utf8_lossy(&output.stdout)
    );

    assert_eq!(
        output.status.code(),
        Some(1),
        "a refusal must reach the exit status, got:\n{text}"
    );
    assert!(
        text.contains("Skipping package 'myapp'") && text.contains("could not be checked"),
        "the refusal must name the package and why, got:\n{text}"
    );
}

// `dotfiles drift` asks the same question apply does, and a refusal has to reach
// the exit status there too.
//
// This is the half no library test can see. `process_events` skips its default
// handler for any event a custom handler claims, and that default handler is the
// only thing that writes the exit code -- so drift's own renderer, which claims
// every successful completion, would print the refusal and exit 0 while the MCP
// server reported the same run as refused. A `refused_count` assertion in the
// library passes either way.
#[test]
fn drift_fails_the_run_for_an_unchecked_file() {
    let temp = setup_default_test_config();
    write_package(&temp);

    let output = sandboxed_command(&temp)
        .args(["dotfiles", "drift"])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let text = format!("{stderr}{stdout}");

    assert_eq!(
        output.status.code(),
        Some(1),
        "a package drift could not check must reach the exit status, got:\n{text}"
    );

    assert!(
        text.contains("Skipping package 'myapp'"),
        "drift must name the package it could not check, got:\n{text}"
    );

    // The summary is the line a reader believes. Reporting a clean check beside
    // a non-zero exit is the contradiction this whole change exists to remove.
    assert!(
        text.contains("1 refused"),
        "the summary must count what it skipped, got:\n{text}"
    );
}
