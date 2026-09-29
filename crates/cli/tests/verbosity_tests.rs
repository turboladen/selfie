pub mod common;

use std::fs;

use common::{SELFIE_ENV, sandboxed_command, setup_default_test_config};

// What each command shows at default verbosity and under `--verbose`, and on
// which stream. stdout carries the answer; stderr carries everything about the
// run: status lines, the operation header, a configured command's own output,
// and debug logs. Every test reads the two streams apart.

fn write_spec(dir: &std::path::Path, name: &str, environment_body: &str) {
    fs::write(
        dir.join("packages").join(format!("{name}.yaml")),
        format!("name: {name}\nenvironments:\n  {SELFIE_ENV}:\n{environment_body}"),
    )
    .unwrap();
}

// A package with one repository-file dotfile and inert commands.
fn write_bat(dir: &std::path::Path) {
    let packages = dir.join("packages");
    fs::create_dir_all(packages.join("bat")).unwrap();
    fs::write(packages.join("bat").join("config"), "cfg\n").unwrap();
    fs::write(
        packages.join("bat.yaml"),
        format!(
            "name: bat\n\
             dotfiles:\n\
             \x20 - source: bat/config\n\
             \x20   target: ~/.config/bat/config\n\
             environments:\n\
             \x20 {SELFIE_ENV}:\n\
             \x20   install: \"true\"\n\
             \x20   check: \"true\"\n\
             \x20   audit: \"echo brew\"\n"
        ),
    )
    .unwrap();
}

struct Run {
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

fn run(temp_dir: &tempfile::TempDir, args: &[&str]) -> Run {
    let output = sandboxed_command(temp_dir).args(args).output().unwrap();
    Run {
        code: output.status.code(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    }
}

fn count(text: &str, needle: &str) -> usize {
    text.lines().filter(|line| line.contains(needle)).count()
}

const SPEC_INFO_HEADER: &str = "Spec info package 'bat' in environment 'test-env'";

#[test]
fn spec_info_shows_only_its_table_by_default() {
    let temp_dir = setup_default_test_config();
    write_bat(temp_dir.path());

    let out = run(&temp_dir, &["spec", "info", "bat"]);

    assert_eq!(out.code, Some(0), "{}", out.stderr);
    assert_eq!(count(&out.stdout, "*test-env"), 1, "{}", out.stdout);
    for text in [&out.stdout, &out.stderr] {
        assert_eq!(count(text, SPEC_INFO_HEADER), 0, "{text}");
        assert_eq!(count(text, "Getting info for bat"), 0, "{text}");
        assert_eq!(count(text, "Loading package definition"), 0, "{text}");
    }
}

#[test]
fn spec_info_shows_its_header_and_steps_on_stderr_when_verbose() {
    let temp_dir = setup_default_test_config();
    write_bat(temp_dir.path());

    let out = run(&temp_dir, &["-v", "spec", "info", "bat"]);

    // Tracing's own lines on stderr repeat these words, so each is matched on
    // the CLI's line, which tracing never prints bare.
    assert!(
        out.stderr.lines().any(|l| l.ends_with(SPEC_INFO_HEADER)),
        "{}",
        out.stderr
    );
    assert!(
        out.stderr
            .lines()
            .any(|l| l.trim() == "Getting info for bat..."),
        "{}",
        out.stderr
    );
    assert!(
        out.stderr
            .lines()
            .any(|l| l.trim().starts_with("Loading package definition")),
        "{}",
        out.stderr
    );
    for needle in [
        SPEC_INFO_HEADER,
        "Getting info",
        "Loading package definition",
    ] {
        assert_eq!(count(&out.stdout, needle), 0, "{}", out.stdout);
    }
}

#[test]
fn spec_list_hides_its_loading_line_unless_verbose() {
    let temp_dir = setup_default_test_config();
    write_bat(temp_dir.path());

    let quiet = run(&temp_dir, &["spec", "list"]);
    assert_eq!(count(&quiet.stdout, "Loading specs"), 0, "{}", quiet.stdout);
    assert_eq!(count(&quiet.stderr, "Loading specs"), 0, "{}", quiet.stderr);

    let verbose = run(&temp_dir, &["-v", "spec", "list"]);
    assert!(
        verbose
            .stderr
            .lines()
            .any(|l| l.trim() == "Loading specs..."),
        "{}",
        verbose.stderr
    );
    assert_eq!(
        count(&verbose.stdout, "Loading specs"),
        0,
        "{}",
        verbose.stdout
    );
}

// A command that runs a configured command shows one waiting line on stderr,
// from the library's step, and nothing the CLI inferred on its own.
#[test]
fn a_command_that_waits_says_so_once_on_stderr() {
    let temp_dir = setup_default_test_config();
    write_bat(temp_dir.path());

    for (args, waiting) in [
        (
            &["package", "status", "bat"][..],
            "Running the check command for bat",
        ),
        (
            &["package", "check", "bat"][..],
            "Running the check command for bat",
        ),
        (
            &["package", "audit", "bat"][..],
            "Running the audit command for bat",
        ),
    ] {
        let out = run(&temp_dir, args);

        assert_eq!(count(&out.stderr, waiting), 1, "{args:?}: {}", out.stderr);
        assert_eq!(count(&out.stdout, waiting), 0, "{args:?}: {}", out.stdout);
        for inferred in ["Checking status of", "Checking bat...", "Auditing bat"] {
            assert_eq!(
                count(&out.stdout, inferred) + count(&out.stderr, inferred),
                0,
                "{args:?}: {}{}",
                out.stdout,
                out.stderr
            );
        }
    }
}

#[test]
fn an_audit_of_every_package_names_each_one() {
    let temp_dir = setup_default_test_config();
    write_bat(temp_dir.path());
    write_spec(
        temp_dir.path(),
        "fd",
        "    install: \"true\"\n    audit: \"echo brew\"\n",
    );

    let out = run(&temp_dir, &["package", "audit", "--all"]);

    assert_eq!(
        count(&out.stderr, "Running the audit command for bat"),
        1,
        "{}",
        out.stderr
    );
    assert_eq!(
        count(&out.stderr, "Running the audit command for fd"),
        1,
        "{}",
        out.stderr
    );
    assert_eq!(
        count(&out.stderr, "Auditing all packages"),
        0,
        "{}",
        out.stderr
    );
}

#[test]
fn apply_prints_no_header_by_default() {
    let temp_dir = setup_default_test_config();
    write_bat(temp_dir.path());

    let out = run(&temp_dir, &["apply", "--dry-run"]);

    assert_eq!(out.code, Some(0), "{}", out.stderr);
    for text in [&out.stdout, &out.stderr] {
        assert_eq!(count(text, "Dotfile apply in environment"), 0, "{text}");
    }
}

// An install command's own output is not the answer: at default verbosity it
// is not shown, and under `--verbose` it is prefixed on stderr. It never reaches
// stdout.
#[test]
fn an_install_command_output_is_shown_only_when_verbose_on_stderr() {
    let temp_dir = setup_default_test_config();
    write_spec(
        temp_dir.path(),
        "echoer",
        "    install: \"echo MARKER-FROM-INSTALL\"\n",
    );

    let quiet = run(&temp_dir, &["package", "install", "echoer"]);
    assert_eq!(quiet.code, Some(0), "{}", quiet.stderr);
    assert_eq!(
        count(&quiet.stderr, "MARKER-FROM-INSTALL"),
        0,
        "{}",
        quiet.stderr
    );
    assert_eq!(
        count(&quiet.stdout, "MARKER-FROM-INSTALL"),
        0,
        "{}",
        quiet.stdout
    );
    assert_eq!(
        count(&quiet.stderr, "Running the install command for echoer"),
        1,
        "{}",
        quiet.stderr
    );

    let verbose = run(&temp_dir, &["-v", "package", "install", "echoer"]);
    assert!(
        verbose
            .stderr
            .lines()
            .any(|l| l.contains("│ MARKER-FROM-INSTALL")),
        "{}",
        verbose.stderr
    );
    assert_eq!(
        count(&verbose.stdout, "MARKER-FROM-INSTALL"),
        0,
        "{}",
        verbose.stdout
    );
}

// A failing install command's stderr is shown with the error, at default
// verbosity: while it ran, its output was hidden.
#[test]
fn a_failing_install_shows_its_stderr_by_default() {
    let temp_dir = setup_default_test_config();
    write_spec(
        temp_dir.path(),
        "broken",
        "    install: \"echo FAILURE-REASON >&2; exit 3\"\n",
    );

    let out = run(&temp_dir, &["package", "install", "broken"]);

    assert_eq!(out.code, Some(1), "{}", out.stderr);
    // Its own line, apart from the error that quotes the command.
    assert_eq!(
        out.stderr
            .lines()
            .filter(|l| l.trim() == "FAILURE-REASON")
            .count(),
        1,
        "{}",
        out.stderr
    );
    assert_eq!(count(&out.stdout, "FAILURE-REASON"), 0, "{}", out.stdout);
}

// `verbose: true` under `cli:` in the config file turns on what the flag does:
// the header, and debug logs, both on stderr.
#[test]
fn the_config_file_setting_is_as_verbose_as_the_flag() {
    let temp_dir = setup_default_test_config();
    write_bat(temp_dir.path());
    let config_path = temp_dir.path().join(".config/selfie/config.yaml");
    let mut config = fs::read_to_string(&config_path).unwrap();
    config.push_str("cli:\n  verbose: true\n");
    fs::write(&config_path, config).unwrap();

    let out = run(&temp_dir, &["spec", "info", "bat"]);

    assert_eq!(out.code, Some(0), "{}", out.stderr);
    assert!(
        out.stderr.lines().any(|l| l.ends_with(SPEC_INFO_HEADER)),
        "{}",
        out.stderr
    );
    assert_eq!(count(&out.stderr, "Final config"), 1, "{}", out.stderr);
    assert_eq!(count(&out.stdout, "Final config"), 0, "{}", out.stdout);
}

// Control for the test above: with neither the setting nor the flag, no debug
// log is written.
#[test]
fn without_the_setting_or_the_flag_there_are_no_debug_logs() {
    let temp_dir = setup_default_test_config();
    write_bat(temp_dir.path());

    let out = run(&temp_dir, &["spec", "info", "bat"]);

    assert_eq!(out.code, Some(0), "{}", out.stderr);
    assert_eq!(count(&out.stderr, "Final config"), 0, "{}", out.stderr);
}

// A config that fails to load cannot turn verbosity on, so the flag decides,
// and the debug logs start before the failure is reported.
#[test]
fn a_verbose_run_logs_even_when_the_config_fails_to_load() {
    let temp_dir = setup_default_test_config();
    fs::write(
        temp_dir.path().join(".config/selfie/config.yaml"),
        "environment: [unterminated\n",
    )
    .unwrap();

    for (args, logs) in [(&["-v", "spec", "list"][..], 1), (&["spec", "list"][..], 0)] {
        let out = run(&temp_dir, args);

        assert_ne!(out.code, Some(0), "{args:?}: {}", out.stderr);
        assert_eq!(
            count(&out.stderr, "CLI arguments"),
            logs,
            "{args:?}: {}",
            out.stderr
        );
    }
}
