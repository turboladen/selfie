pub mod common;

use std::fs;
use std::path::Path;

use common::{CLEAN, FOUND, SELFIE_ENV, sandboxed_command, setup_default_test_config};
use console::measure_text_width;

// `apply` and `dotfiles drift` print a table, one row per entry, its columns
// fixed before the first row. Columns are measured from the start of the line
// as a terminal shows it.

// `HOME` is the canonical temporary directory: selfie expands targets against
// the canonical home, and a `HOME` spelled through a symlink (macOS's `/var`)
// would leave every target unshortened and past the column cap, so the test
// would measure overflow instead of alignment.
fn run(temp_dir: &tempfile::TempDir, args: &[&str]) -> (Option<i32>, String, String) {
    let home = temp_dir.path().canonicalize().unwrap();
    let output = sandboxed_command(temp_dir)
        .env("HOME", &home)
        .args(args)
        .output()
        .unwrap();
    (
        output.status.code(),
        String::from_utf8(output.stdout).expect("stdout must be UTF-8"),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

// A package `name` whose entries deploy each repository file in `files`, which
// holds its own name, to `~/.<file>` with `/` read as `-`. `extra` is appended
// to the entry list.
fn package(root: &Path, name: &str, files: &[&str], extra: &str) {
    let packages = root.join("packages");
    let mut entries = String::new();
    for file in files {
        let path = packages.join(name).join(file);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, format!("{file}\n")).unwrap();
        let target = file.replace('/', "-");
        entries.push_str(&format!(
            "  - source: \"{name}/{file}\"\n    target: \"~/.{target}\"\n"
        ));
    }
    fs::write(
        packages.join(format!("{name}.yaml")),
        format!(
            "name: {name}\ndotfiles:\n{entries}{extra}environments:\n  {SELFIE_ENV}:\n    \
             install: \"true\"\n"
        ),
    )
    .unwrap();
}

// The rows of a table: the lines holding an arrow.
fn rows(stdout: &str) -> Vec<&str> {
    stdout.lines().filter(|line| line.contains(" → ")).collect()
}

// The terminal column where `needle` starts on each row.
fn columns(stdout: &str, needle: &str) -> Vec<usize> {
    rows(stdout)
        .into_iter()
        .map(|line| {
            let at = line
                .find(needle)
                .unwrap_or_else(|| panic!("{needle:?} not in {line:?}"));
            measure_text_width(&line[..at])
        })
        .collect()
}

fn all_equal(columns: &[usize]) -> bool {
    columns.windows(2).all(|pair| pair[0] == pair[1])
}

// Sources of different widths, one of them CJK (more bytes and fewer
// characters than columns), a template, and a provider entry whose command is
// inert and never run by a dry run.
#[test]
fn a_dry_runs_rows_line_up_by_display_width() {
    let temp_dir = setup_default_test_config();
    package(
        temp_dir.path(),
        "app",
        &["a", "日本/conf", "longer/name"],
        "  - source: \"app/a\"\n    target: \"~/.tmpl\"\n    vars:\n      email: echo me\n  \
         - command: echo token\n    target: \"~/.token\"\n",
    );

    let (code, stdout, stderr) = run(&temp_dir, &["apply", "--dry-run"]);

    assert_eq!(code, Some(CLEAN), "{stdout}{stderr}");
    assert_eq!(rows(&stdout).len(), 5, "{stdout}");
    assert!(all_equal(&columns(&stdout, " → ")), "{stdout}");
    assert!(all_equal(&columns(&stdout, "dry run")), "{stdout}");
}

// A deployed row lines up with a skipped one, and the line naming the copy it
// kept starts under its source.
#[test]
fn a_deployed_row_lines_up_with_a_skipped_one() {
    let temp_dir = setup_default_test_config();
    let root = temp_dir.path();
    package(root, "app", &["a", "much/longer-name"], "");
    let (code, stdout, stderr) = run(&temp_dir, &["apply"]);
    assert_eq!(code, Some(CLEAN), "{stdout}{stderr}");
    // Content selfie did not write, so `--yes` overwrites it and keeps a copy.
    fs::write(root.join(".much-longer-name"), "mine\n").unwrap();
    fs::write(root.join("packages/app/much/longer-name"), "changed\n").unwrap();

    let (code, stdout, stderr) = run(&temp_dir, &["apply", "--yes"]);

    assert_eq!(code, Some(CLEAN), "{stdout}{stderr}");
    let rows = rows(&stdout);
    assert_eq!(rows.len(), 2, "{stdout}");
    assert!(
        rows[0].starts_with("skipped ") && rows[1].starts_with("deployed "),
        "{stdout}"
    );
    assert!(all_equal(&columns(&stdout, " → ")), "{stdout}");
    let source_column = columns(&stdout, "app/")[0];
    let copy = stdout
        .lines()
        .find(|line| line.contains("previous content copied to"))
        .unwrap_or_else(|| panic!("no backup line:\n{stdout}"));
    assert_eq!(
        copy.len() - copy.trim_start().len(),
        source_column,
        "{stdout}"
    );
}

// A drifted row names its source and lines up with an unverified one. The
// provider entry's command is never run by drift.
#[test]
fn a_drifted_row_lines_up_with_an_unverified_one() {
    let temp_dir = setup_default_test_config();
    let root = temp_dir.path();
    package(
        root,
        "app",
        &["conf"],
        "  - command: echo token\n    target: \"~/.a-token\"\n",
    );
    let (code, stdout, stderr) = run(&temp_dir, &["apply"]);
    assert_eq!(code, Some(CLEAN), "{stdout}{stderr}");
    fs::write(root.join(".conf"), "edited\n").unwrap();

    let (code, stdout, stderr) = run(&temp_dir, &["dotfiles", "drift"]);

    assert_eq!(code, Some(FOUND), "{stdout}{stderr}");
    let rows = rows(&stdout);
    assert_eq!(rows.len(), 2, "{stdout}");
    assert!(
        rows[0].starts_with("drifted ") && rows[0].contains("app/conf "),
        "{stdout}"
    );
    assert!(rows[1].starts_with("unverified "), "{stdout}");
    assert!(all_equal(&columns(&stdout, " → ")), "{stdout}");
    let detail = |row: &str, needle: &str| measure_text_width(&row[..row.find(needle).unwrap()]);
    assert_eq!(
        detail(rows[0], "target changed"),
        detail(rows[1], "provider-sourced"),
        "{stdout}"
    );
}
