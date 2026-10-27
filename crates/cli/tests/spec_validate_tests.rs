pub mod common;

use std::fs;

use common::{CLEAN, FAILED, sandboxed_command, setup_default_test_config};
use predicates::prelude::*;

// A fixture value, never a real credential. High-entropy and deliberately not
// shaped like a path, a package name or an environment name: `assert_secret_free`
// scans a twelve-character window, so a path-shaped value would match ordinary
// output and pass for the wrong reason.
const SECRET: &str = "Xq7Rm2Kz9Wp4Ns6Tv8Bh3Gd5";

fn spec_with_a_secret_above_the_failure(temp_dir: &tempfile::TempDir) {
    let packages_dir = temp_dir.path().join("packages");
    fs::create_dir_all(&packages_dir).unwrap();
    fs::write(
        packages_dir.join("creds.yml"),
        format!(
            "name: creds\ndotfiles:\n  - command: op read op://vault/item/field\n    \
             vars:\n      token: {SECRET}\n    target: ~/.npmrc\nenvironments: {{oops\n"
        ),
    )
    .unwrap();
}

// The window the maintainer asked for: a line and a column alone means holding an
// offset in your head and opening an editor to find it.
#[test]
fn an_unparsable_spec_shows_the_lines_around_the_failure() {
    let temp_dir = setup_default_test_config();
    spec_with_a_secret_above_the_failure(&temp_dir);

    // A spec that will not parse is a failed run, not a quiet one. Without this
    // the assertions below would still hold if the exit code regressed to 0.
    let output = sandboxed_command(&temp_dir)
        .args(["spec", "validate", "creds"])
        .assert()
        .code(FAILED)
        .get_output()
        .stderr
        .clone();
    let stderr = String::from_utf8(output).expect("stderr must be UTF-8");

    // The failing line, its number, and a caret under the column.
    assert!(
        stderr.contains("environments: {oops"),
        "the window must quote the failing line, got: {stderr}"
    );
    assert!(
        stderr.contains('^'),
        "the window must point at it, got: {stderr}"
    );
    assert!(
        stderr.contains("creds.yml:7:15"),
        "the location must be named, got: {stderr}"
    );

    // The neighbors come with it, `vars:` value and all, and that is the agreed
    // behavior rather than a leak being tolerated: a terminal shows the reader
    // their own file, and the MCP server shows the same failure with none of it.
    // `event_collector.rs` holds the other half and asserts the opposite.
    //
    // A plain `contains`, because the value must be PRESENT here. Narrowing the
    // window, or dropping it, has to fail this rather than pass quietly.
    assert!(
        stderr.contains(SECRET),
        "the window must quote the lines either side, got: {stderr}"
    );
}

// The window is stderr, like the sentence above it.
//
// The stderr predicate is the control. On its own, "stdout does not hold the
// window" also passes a run that produced no window anywhere.
#[test]
fn the_window_goes_to_stderr_and_not_stdout() {
    let temp_dir = setup_default_test_config();
    spec_with_a_secret_above_the_failure(&temp_dir);

    sandboxed_command(&temp_dir)
        .args(["spec", "validate", "creds"])
        .assert()
        .stderr(predicate::str::contains("environments: {oops"))
        .stdout(predicate::str::contains("environments: {oops").not());
}

// One failure, one marker. The location line and the window are evidence for the
// sentence above them, and `print_error_context` exists to say that in the output:
// a second marker reads as a second problem.
#[test]
fn one_parse_failure_prints_one_marker() {
    let temp_dir = setup_default_test_config();
    spec_with_a_secret_above_the_failure(&temp_dir);

    let output = sandboxed_command(&temp_dir)
        .args(["spec", "validate", "creds"])
        .assert()
        .get_output()
        .stderr
        .clone();
    let stderr = String::from_utf8(output).expect("stderr must be UTF-8");

    assert_eq!(
        stderr.matches('✗').count(),
        1,
        "one failure must print one marker, got: {stderr}"
    );
}

// The listing side of the same file, unchanged: many files, one line each. A
// source window per row is what made this output unreadable.
#[test]
fn a_listing_does_not_show_a_window() {
    let temp_dir = setup_default_test_config();
    spec_with_a_secret_above_the_failure(&temp_dir);

    let output = sandboxed_command(&temp_dir)
        .args(["spec", "list"])
        .assert()
        .code(CLEAN)
        .get_output()
        .stdout
        .clone();
    let stdout = String::from_utf8(output).expect("stdout must be UTF-8");

    assert!(
        stdout.contains("creds.yml"),
        "the spec must be listed, got: {stdout}"
    );
    assert!(
        !stdout.contains("environments: {oops"),
        "a listing must not quote the file, got: {stdout}"
    );
}

// The category column words a category for a person, never the enum's name.
#[test]
fn an_issue_category_reads_as_words() {
    let temp_dir = setup_default_test_config();
    let packages_dir = temp_dir.path().join("packages");
    fs::create_dir_all(&packages_dir).unwrap();
    fs::write(
        packages_dir.join("blank.yml"),
        format!(
            "name: blank\nenvironments:\n  {}:\n    install: \"\"\n",
            common::SELFIE_ENV
        ),
    )
    .unwrap();

    let output = sandboxed_command(&temp_dir)
        .args(["spec", "validate", "blank"])
        .output()
        .unwrap();

    let both = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        both.lines()
            .filter(|line| line.contains("required field"))
            .count(),
        1,
        "{both}"
    );
}

// The success card is for a clean spec only. A spec with warnings or errors
// shows its table and no card.
#[test]
fn only_a_clean_spec_shows_the_success_card() {
    let temp_dir = setup_default_test_config();
    let packages_dir = temp_dir.path().join("packages");
    fs::create_dir_all(&packages_dir).unwrap();
    let env = common::SELFIE_ENV;
    for (name, body) in [
        ("clean", "    install: \"true\"\n"),
        ("blank", "    install: \"\"\n"),
    ] {
        fs::write(
            packages_dir.join(format!("{name}.yml")),
            format!("name: {name}\nenvironments:\n  {env}:\n{body}"),
        )
        .unwrap();
    }
    let cards = |name: &str| {
        let output = sandboxed_command(&temp_dir)
            .args(["spec", "validate", name])
            .output()
            .unwrap();
        format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
        .lines()
        .filter(|line| line.contains("Validation Results"))
        .count()
    };

    assert_eq!(cards("clean"), 1);
    assert_eq!(cards("blank"), 0);
}

// The table printed under the line naming `label` in an issues section: the lines
// after it that draw a table, up to the next line that does not.
fn group_under<'a>(stdout: &'a str, label: &str) -> Vec<&'a str> {
    let mut lines = stdout.lines();
    let found = lines.by_ref().find(|line| {
        let line = line.trim_start();
        (line.starts_with('✗') || line.starts_with('ℹ')) && line.ends_with(label)
    });
    assert!(found.is_some(), "no group is labeled {label}: {stdout}");
    lines
        .take_while(|line| line.starts_with(['╭', '│', '╞', '├', '╰']))
        .collect()
}

fn sandbox_with_an_unparsable_spec() -> tempfile::TempDir {
    let temp_dir = setup_default_test_config();
    let packages_dir = temp_dir.path().join("packages");
    fs::write(
        packages_dir.join("good.yml"),
        format!(
            "name: good\nenvironments:\n  {}:\n    install: \"true\"\n",
            common::SELFIE_ENV
        ),
    )
    .unwrap();
    fs::write(
        packages_dir.join("creds.yml"),
        "name: creds\nenvironments: {oops\n",
    )
    .unwrap();
    temp_dir
}

// The results say where a spec failed to parse, in the entry labeled with its
// path, so the summary's unparsable count points at something on screen.
#[test]
fn validate_all_says_where_an_unparsable_spec_failed() {
    let temp_dir = sandbox_with_an_unparsable_spec();

    let output = sandboxed_command(&temp_dir)
        .args(["spec", "validate", "--all"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);

    assert_eq!(output.status.code(), Some(FAILED), "{stdout}");
    let group = group_under(&stdout, "packages/creds.yml");
    assert_eq!(
        group
            .iter()
            .filter(|line| line.contains("unclosed bracket") && line.contains("line 2, column 15"))
            .count(),
        1,
        "{stdout}"
    );
    assert!(stdout.contains("1 unparsable"), "{stdout}");
}

// A dotfiles spec that does not parse, under a name a package spec claims, is no
// result of the run: its skip line is its one mention, and it is still printed.
#[test]
fn validate_all_still_warns_about_a_shadowed_unparsable_spec() {
    let temp_dir = sandbox_with_an_unparsable_spec();
    fs::remove_file(temp_dir.path().join("packages/creds.yml")).unwrap();
    let dotfiles = temp_dir.path().join("dotfiles");
    fs::create_dir_all(&dotfiles).unwrap();
    let config = temp_dir.path().join(".config/selfie/config.yaml");
    let mut text = fs::read_to_string(&config).unwrap();
    text.push_str(&format!("dotfiles_directory: {}\n", dotfiles.display()));
    fs::write(&config, text).unwrap();
    fs::write(dotfiles.join("good.yaml"), "name: good\nenvironments: [\n").unwrap();

    let output = sandboxed_command(&temp_dir)
        .args(["spec", "validate", "--all"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert_eq!(
        stderr
            .lines()
            .filter(|line| line.contains("Skipping package file") && line.contains("good.yaml"))
            .count(),
        1,
        "{stderr}"
    );
    assert!(!stdout.contains("good.yaml"), "{stdout}");
}
