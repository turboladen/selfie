//! `selfie apply` against a package file whose unknown-key check cannot run.
//!
//! The check re-reads the file into a map of keys, and a file that parses as a
//! package can still fail that read. What apply does then depends on what is
//! left to deploy: a package with entries is applied and the run still succeeds,
//! while one with nothing to deploy is refused, because that is also what a
//! shadowed `dotfiles:` key looks like and this file could not be checked for
//! one.
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
fn apply_reports_an_unchecked_file_without_failing_the_run() {
    let temp = setup_default_test_config();
    write_package(&temp);

    let output = sandboxed_command(&temp)
        .args(["apply", "-y"])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);

    assert!(
        output.status.success(),
        "a check that did not run must not fail the run, got {:?}:\n{stderr}{stdout}",
        output.status.code()
    );

    // Content first: `read_link` below errors for a file that is not there at
    // all, so asserting it before this one blames the wrong thing for a package
    // that never deployed.
    let deployed = temp.path().join(".config/myapp/config.toml");
    assert_eq!(
        std::fs::read_to_string(&deployed).unwrap(),
        "REPO\n",
        "the package must still deploy"
    );
    assert!(
        deployed.read_link().is_err(),
        "the deployed file must be a regular file, not a link"
    );

    let text = format!("{stderr}{stdout}");
    assert!(
        text.contains("could not re-read") && text.contains("myapp"),
        "the run must say the check could not run, got:\n{text}"
    );
    // The refusal arm a few lines away says "Skipping package", and this package
    // was not skipped.
    assert!(
        !text.contains("Skipping"),
        "nothing was skipped, got:\n{text}"
    );
}

// The same file with no dotfiles behind it, which is where the exit status
// splits. Reporting this as a plain success is the count that means both
// "nothing to do" and "selfie declined".
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
