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

// `spec validate --all` in the sandbox: its exit code, stdout and stderr.
fn validate_all(temp_dir: &tempfile::TempDir) -> (Option<i32>, String, String) {
    let output = sandboxed_command(temp_dir)
        .args(["spec", "validate", "--all"])
        .output()
        .unwrap();
    (
        output.status.code(),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

// The last line of `out` that is not blank.
fn last_line(out: &str) -> &str {
    out.lines()
        .rfind(|line| !line.trim().is_empty())
        .unwrap_or_default()
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

    let (code, stdout, _) = validate_all(&temp_dir);

    assert_eq!(code, Some(FAILED), "{stdout}");
    // Asserted on the row and on the entry apart, so neither copy can stand in
    // for the other.
    assert_eq!(
        row(&stdout, "creds"),
        ["✗", "creds", "unparsable", "line 2, column 15"],
        "{stdout}"
    );
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

    let (code, stdout, stderr) = validate_all(&temp_dir);

    assert_eq!(code, Some(common::FOUND), "{stdout}{stderr}");
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

// The cells of the results table's row for `spec`. The results table is the one
// with four columns; no issues table has that few.
fn row<'a>(stdout: &'a str, spec: &str) -> Vec<&'a str> {
    let rows = rows(stdout);
    let found: Vec<Vec<&str>> = rows.into_iter().filter(|cells| cells[1] == spec).collect();
    assert_eq!(found.len(), 1, "one row for {spec}: {stdout}");
    found.into_iter().next().unwrap()
}

// Every row of the results table, header excluded.
fn rows(stdout: &str) -> Vec<Vec<&str>> {
    stdout
        .lines()
        .map(|line| {
            line.trim_matches('│')
                .split('┆')
                .map(str::trim)
                .collect::<Vec<_>>()
        })
        .filter(|cells| cells.len() == 4 && cells[1] != "Spec")
        .collect()
}

// The labels of the issues section's entries, in order.
fn entry_labels(stdout: &str) -> Vec<&str> {
    stdout
        .lines()
        .filter_map(|line| {
            line.strip_prefix("  ✗ ")
                .or_else(|| line.strip_prefix("  ℹ "))
        })
        .collect()
}

// One row per spec, whatever became of it, and an issues entry only for the specs
// with a problem. Each count in the summary is a number of rows on screen, and the
// summary is the last thing printed.
#[test]
fn validate_all_shows_a_row_per_spec_and_details_for_problems_only() {
    let temp_dir = sandbox_with_an_unparsable_spec();
    let packages = temp_dir.path().join("packages");
    let env = common::SELFIE_ENV;
    for (name, body) in [
        (
            "warn",
            format!("environments:\n  {env}:\n    install: \"echo `true`\"\n"),
        ),
        ("noenv", "description: x\n".to_string()),
        (
            "note",
            format!(
                "dotfiles:\n  - command: \"echo value\"\n    target: \"~/.noterc\"\n\
                 environments:\n  {env}:\n    install: \"true\"\n"
            ),
        ),
    ] {
        fs::write(
            packages.join(format!("{name}.yml")),
            format!("name: {name}\n{body}"),
        )
        .unwrap();
    }
    let ambiguous = format!("name: nv\nenvironments:\n  {env}:\n    install: \"true\"\n");
    fs::write(packages.join("nv.yml"), &ambiguous).unwrap();
    fs::write(packages.join("nv.yaml"), &ambiguous).unwrap();

    let (code, stdout, stderr) = validate_all(&temp_dir);

    assert_eq!(code, Some(FAILED), "{stdout}{stderr}");
    let statuses: Vec<(&str, &str)> = rows(&stdout)
        .into_iter()
        .map(|cells| (cells[1], cells[2]))
        .collect();
    assert_eq!(
        statuses,
        [
            ("good", "valid"),
            ("noenv", "errors"),
            ("note", "valid"),
            ("warn", "warnings"),
            ("creds", "unparsable"),
            ("nv", "ambiguous"),
        ],
        "{stdout}"
    );
    assert_eq!(row(&stdout, "note")[3], "1 notice(s)", "{stdout}");

    // A valid spec has no issues entry, notices or not: `note` is named once, in
    // its row.
    assert_eq!(
        stdout.lines().filter(|line| line.contains("note")).count(),
        1,
        "{stdout}"
    );
    let labels = entry_labels(&stdout);
    assert_eq!(labels.len(), 4, "{stdout}");
    assert_eq!(labels[..2], ["noenv", "warn"], "{stdout}");
    assert!(labels[2].ends_with("packages/creds.yml"), "{stdout}");
    assert_eq!(labels[3], "nv", "{stdout}");

    // The ambiguous name is named in its row and its entry, and in no warning.
    assert!(!stderr.contains("'nv'"), "{stderr}");

    assert_eq!(
        last_line(&stdout),
        format!(
            "✗ Spec validation completed in environment '{env}': 4 package(s) validated, \
             1 with errors, 1 with warnings, 1 unparsable, 1 ambiguous or unlistable"
        ),
        "{stdout}"
    );
}

// A clean run is the table and the summary, with no issues section.
#[test]
fn validate_all_over_clean_specs_prints_no_issues_section() {
    let temp_dir = sandbox_with_an_unparsable_spec();
    fs::remove_file(temp_dir.path().join("packages/creds.yml")).unwrap();

    let (code, stdout, _) = validate_all(&temp_dir);

    assert_eq!(code, Some(CLEAN), "{stdout}");
    assert_eq!(row(&stdout, "good")[2], "valid", "{stdout}");
    assert!(entry_labels(&stdout).is_empty(), "{stdout}");
    assert!(!stdout.contains("Validation"), "{stdout}");
    assert!(
        last_line(&stdout).contains("1 package(s) validated successfully"),
        "{stdout}"
    );
}

// A dotfiles directory selfie could not list is named once, as its row and entry
// in the results, and in no warning above them.
#[cfg(unix)]
#[test]
fn validate_all_names_an_unlistable_directory_only_in_its_results() {
    use std::os::unix::fs::PermissionsExt;

    let temp_dir = sandbox_with_an_unparsable_spec();
    fs::remove_file(temp_dir.path().join("packages/creds.yml")).unwrap();
    // The sibling of `packages`, which is where an unset `dotfiles_directory`
    // resolves to.
    let dotfiles = temp_dir.path().join("dotfiles");
    fs::create_dir_all(&dotfiles).unwrap();
    fs::set_permissions(&dotfiles, fs::Permissions::from_mode(0o000)).unwrap();
    // Root ignores the mode bits, so check the precondition actually holds.
    if fs::read_dir(&dotfiles).is_ok() {
        fs::set_permissions(&dotfiles, fs::Permissions::from_mode(0o755)).unwrap();
        eprintln!("SKIP validate_all_names_an_unlistable_directory_only_in_its_results");
        return;
    }

    let (code, stdout, stderr) = validate_all(&temp_dir);
    // Before any assertion, so a failure cannot leave the temp dir unremovable.
    fs::set_permissions(&dotfiles, fs::Permissions::from_mode(0o755)).unwrap();

    assert_eq!(code, Some(FAILED), "{stdout}{stderr}");
    let shown = dotfiles.display().to_string();
    assert_eq!(row(&stdout, &shown)[2], "unlistable", "{stdout}");
    assert!(
        stdout.contains("Failed to load standalone dotfiles"),
        "{stdout}"
    );
    assert!(
        !stderr.contains("Failed to load standalone dotfiles"),
        "{stderr}"
    );
}
