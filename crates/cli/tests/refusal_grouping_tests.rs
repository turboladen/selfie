pub mod common;

use std::fs;

use common::{
    RunOutput, SELFIE_ENV, package_repo_with_remote, run_sandboxed, setup_default_test_config,
};

// Packages refused whole for one reason print as one warning naming them all,
// so the finding the user ran the command for is not buried under N copies of
// one sentence.

const GROUPED: &str = "Skipping 3 packages (a, b, c): unknown field 'version'";

fn run(temp_dir: &tempfile::TempDir, args: &[&str]) -> RunOutput {
    let mut all = vec!["--no-color"];
    all.extend_from_slice(args);
    run_sandboxed(temp_dir, &all)
}

// Three packages carry the same unknown top-level key, and one deployed dotfile
// has since drifted.
fn sandbox() -> tempfile::TempDir {
    let temp_dir = setup_default_test_config();
    let packages = temp_dir.path().join("packages");
    for name in ["a", "b", "c"] {
        fs::write(
            packages.join(format!("{name}.yaml")),
            format!(
                "name: {name}\nversion: 1\nenvironments:\n  {SELFIE_ENV}:\n    install: \"true\"\n    audit: \"echo x\"\n"
            ),
        )
        .unwrap();
    }
    fs::create_dir_all(packages.join("bat")).unwrap();
    fs::write(packages.join("bat").join("config"), "cfg\n").unwrap();
    fs::write(
        packages.join("bat.yaml"),
        format!(
            "name: bat\ndotfiles:\n  - source: bat/config\n    target: ~/.batrc\nenvironments:\n  {SELFIE_ENV}:\n    install: \"true\"\n"
        ),
    )
    .unwrap();
    let deployed = run(&temp_dir, &["apply", "--yes"]);
    assert!(
        temp_dir.path().join(".batrc").exists(),
        "{}",
        deployed.stderr
    );
    fs::write(temp_dir.path().join(".batrc"), "edited\n").unwrap();
    temp_dir
}

fn assert_grouped(out: &RunOutput) {
    assert_eq!(
        out.stderr.lines().filter(|l| l.contains(GROUPED)).count(),
        1,
        "{}",
        out.stderr
    );
    assert!(!out.stderr.contains("Skipping package '"), "{}", out.stderr);
}

#[test]
fn drift_groups_its_refusals_and_keeps_its_finding() {
    let temp_dir = sandbox();

    let out = run(&temp_dir, &["dotfiles", "drift"]);

    assert_eq!(out.code, Some(1), "{}", out.stderr);
    assert_grouped(&out);
    assert!(out.stdout.contains("Drift in"), "{}", out.stdout);
}

#[test]
fn apply_dry_run_groups_its_refusals() {
    let temp_dir = sandbox();

    let out = run(&temp_dir, &["apply", "--dry-run"]);

    assert_eq!(out.code, Some(1), "{}", out.stderr);
    assert_grouped(&out);
}

#[test]
fn audit_all_groups_its_refusals() {
    let temp_dir = sandbox();

    let out = run(&temp_dir, &["package", "audit", "--all"]);

    assert_grouped(&out);
}

// Under `--verbose` each refused package keeps its own line.
#[test]
fn verbose_names_each_refused_package() {
    let temp_dir = sandbox();

    let out = run(&temp_dir, &["-v", "dotfiles", "drift"]);

    for name in ["a", "b", "c"] {
        assert!(
            out.stderr.contains(&format!("Skipping package '{name}'")),
            "{}",
            out.stderr
        );
    }
    assert!(!out.stderr.contains(GROUPED), "{}", out.stderr);
}

// `sync status` points at "the warnings above", so the grouped warning must
// already be above it.
#[test]
fn sync_status_prints_the_group_before_pointing_at_it() {
    let temp_dir = sandbox();
    package_repo_with_remote(&temp_dir);

    let out = run(&temp_dir, &["sync", "status"]);

    assert_grouped(&out);
    let lines: Vec<&str> = out.stderr.lines().collect();
    let grouped = lines.iter().position(|l| l.contains(GROUPED)).unwrap();
    let pointer = lines
        .iter()
        .position(|l| l.contains("refusal(s) left dotfiles unchecked"))
        .unwrap_or_else(|| panic!("{}", out.stderr));
    assert!(grouped < pointer, "{}", out.stderr);
}
