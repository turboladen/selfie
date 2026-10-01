//! What `selfie apply` and `selfie dotfiles drift` print for a target no entry
//! deploys to any more, and what they exit with.

pub mod common;

use common::{SELFIE_ENV, sandboxed_command, setup_test_config_with_state_directory as sandbox};

fn write_package(temp: &tempfile::TempDir, target: &str) {
    let packages = temp.path().join("packages");
    std::fs::create_dir_all(packages.join("myapp")).unwrap();
    std::fs::write(packages.join("myapp/rc"), "REPO\n").unwrap();
    std::fs::write(
        packages.join("myapp.yaml"),
        format!(
            "name: myapp\ndotfiles:\n  - source: myapp/rc\n    target: \"{target}\"\nenvironments:\n  {SELFIE_ENV}:\n    install: \"true\"\n"
        ),
    )
    .unwrap();
}

fn run(temp: &tempfile::TempDir, args: &[&str]) -> (Option<i32>, String, String) {
    let output = sandboxed_command(temp).args(args).output().unwrap();
    (
        output.status.code(),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

// Deploy to `~/.old`, then move the entry to `~/.new`, leaving the old file.
fn moved_target() -> tempfile::TempDir {
    let temp = sandbox();
    write_package(&temp, "~/.old");
    let (code, stdout, stderr) = run(&temp, &["apply", "-y"]);
    assert_eq!(code, Some(0), "{stdout}{stderr}");
    assert!(temp.path().join(".old").exists());
    assert!(temp.path().join("state/deploy-state.yml").exists());
    write_package(&temp, "~/.new");
    temp
}

#[test]
fn apply_names_the_orphan_and_exits_zero() {
    let temp = moved_target();

    let (code, stdout, stderr) = run(&temp, &["apply", "-y"]);
    assert_eq!(
        code,
        Some(0),
        "an orphan is not a failure:\n{stdout}{stderr}"
    );
    assert_eq!(
        stderr.matches("Orphaned").count(),
        1,
        "one warning for the one orphan:\n{stderr}"
    );
    assert!(
        stderr.contains("deployed from myapp/rc by package 'myapp'"),
        "{stderr}"
    );
    assert!(stderr.contains("selfie leaves it in place"), "{stderr}");
    assert!(
        format!("{stdout}{stderr}").contains("1 orphaned"),
        "the summary counts it:\n{stdout}{stderr}"
    );
    assert_eq!(
        std::fs::read_to_string(temp.path().join(".old")).unwrap(),
        "REPO\n",
        "the old file is left as it was"
    );
}

// Drift's summary is marked as a finding, not a success, when it found an
// orphan, and an orphan is a finding drift was asked to look for. Both are
// drift's answer, so both are on stdout.
#[test]
fn drift_warns_in_its_summary_and_exits_three() {
    let temp = moved_target();
    // Deploys the new target, so the orphan is the only thing drift can find.
    let (code, stdout, stderr) = run(&temp, &["apply", "-y"]);
    assert_eq!(code, Some(0), "{stdout}{stderr}");

    let (code, stdout, stderr) = run(&temp, &["dotfiles", "drift"]);
    assert_eq!(code, Some(3), "{stdout}{stderr}");
    assert!(
        stdout.contains("Orphaned") && !stderr.contains("Orphaned"),
        "{stdout}{stderr}"
    );
    let summary = stdout
        .lines()
        .find(|line| line.contains("1 orphaned"))
        .unwrap_or_else(|| panic!("the summary counts the orphan:\n{stdout}"));
    assert!(
        summary.starts_with('⚠') && !stderr.contains("1 orphaned"),
        "the summary must be marked as a finding, on stdout:\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        summary.contains("0 drifted"),
        "nothing but the orphan may make the summary a finding:\n{summary}"
    );
}

// An orphan's source is shown relative to the directory its record names, under
// that directory's heading.
#[test]
fn drift_shows_an_orphan_relative_to_its_directory() {
    let temp = moved_target();

    let (code, stdout, stderr) = run(&temp, &["dotfiles", "drift"]);
    assert_eq!(code, Some(3), "{stdout}{stderr}");
    let lines: Vec<&str> = stdout.lines().collect();
    let heading = lines
        .iter()
        .position(|line| line.contains("Packages: "))
        .unwrap_or_else(|| panic!("the base directory is named:\n{stdout}"));
    let orphan = lines
        .iter()
        .position(|line| line.contains("deployed from myapp/rc by package 'myapp'"))
        .unwrap_or_else(|| panic!("the orphan's source is relative:\n{stdout}"));
    assert!(heading < orphan, "{stdout}");
}

// A record that names no base keeps its spelling, with no heading claiming a
// directory for it.
#[test]
fn an_orphan_whose_record_names_no_base_shows_its_spelling() {
    let temp = moved_target();
    let state = temp.path().join("state/deploy-state.yml");
    let old = temp.path().join(".old");
    // A record without the `base` field.
    std::fs::write(
        &state,
        format!(
            "deployed:\n  {}:\n    source: myapp/rc\n    checksum: abc\n    \
             deployed_at: \"2026-09-01T00:00:00+00:00\"\n    package: myapp\n",
            old.display()
        ),
    )
    .unwrap();

    let (code, stdout, stderr) = run(&temp, &["dotfiles", "drift"]);
    assert_eq!(code, Some(3), "{stdout}{stderr}");
    assert!(
        stdout
            .contains("deployed from myapp/rc (as recorded, directory unknown) by package 'myapp'"),
        "{stdout}"
    );
    assert!(!stdout.contains("Packages: "), "{stdout}");
}

// Apply warns about an orphan on stderr, so its heading is on stderr too: a
// reader of that stream alone can resolve the path.
#[test]
fn apply_heads_an_orphan_on_its_own_stream() {
    let temp = moved_target();

    let (code, stdout, stderr) = run(&temp, &["apply", "-y"]);
    assert_eq!(code, Some(0), "{stdout}{stderr}");
    let lines: Vec<&str> = stderr.lines().collect();
    let heading = lines.iter().position(|l| l.contains("Packages: "));
    let orphan = lines
        .iter()
        .position(|l| l.contains("deployed from myapp/rc by package 'myapp'"));
    assert!(
        matches!((heading, orphan), (Some(h), Some(o)) if h < o),
        "stderr:\n{stderr}"
    );
}
