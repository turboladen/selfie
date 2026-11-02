//! What `apply` says when `stop_on_error` stops it.

pub mod common;

use common::{SELFIE_ENV, run_sandboxed};

// The config for `temp`, with `packages` as the package directory and
// `stop_on_error` on.
fn write_config(temp: &tempfile::TempDir, packages: &std::path::Path) {
    std::fs::write(
        temp.path().join(".config/selfie/config.yaml"),
        format!(
            "environment: {SELFIE_ENV}\npackage_directory: {}\nstop_on_error: true\n",
            packages.display()
        ),
    )
    .unwrap();
}

// An entry whose target a directory occupies is refused, and the run stops on
// it. The failure names the target as a path: U+200B and a line feed in it
// show escaped, on the failure's one line. The entry's own refusal, printed
// above it, is not this test's concern.
#[test]
fn a_stop_on_a_refused_entry_names_its_target_as_a_path() {
    let temp = common::setup_test_config("");
    let packages = temp.path().join("packages");
    std::fs::create_dir_all(packages.join("tool")).unwrap();
    std::fs::write(packages.join("tool/rc"), "x").unwrap();
    std::fs::write(
        packages.join("tool.yml"),
        format!(
            "name: tool\nenvironments:\n  {SELFIE_ENV}:\n    install: \"true\"\ndotfiles:\n  \
             - source: tool/rc\n    target: \"~/t\\u200Bx\\ny\"\n"
        ),
    )
    .unwrap();
    std::fs::create_dir_all(temp.path().join("t\u{200B}x\ny")).unwrap();
    write_config(&temp, &packages);

    let output = run_sandboxed(&temp, &["--no-color", "apply"]);
    let both = format!("{}{}", output.stdout, output.stderr);

    let failure = both
        .lines()
        .find(|line| line.contains("Stopped after failing to apply dotfile"))
        .unwrap_or_else(|| panic!("no stop: {both}"));
    assert!(
        failure.contains(r"dotfile '~/t\u{200b}x\ny' (stop_on_error is enabled)"),
        "{failure}"
    );
    assert!(!failure.contains('\u{200B}'), "a raw U+200B: {failure:?}");
}

// A spec that cannot be loaded stops the run before anything is applied, and
// the failure names the spec as a path.
#[test]
fn a_stop_on_an_unloadable_spec_names_it_as_a_path() {
    let temp = common::setup_test_config("");
    let packages = temp.path().join("p\u{200B}kgs");
    std::fs::create_dir_all(&packages).unwrap();
    std::fs::write(packages.join("bad.yml"), "environments: {oops\n").unwrap();
    write_config(&temp, &packages);

    let output = run_sandboxed(&temp, &["--no-color", "apply"]);
    let both = format!("{}{}", output.stdout, output.stderr);

    let failure = both
        .lines()
        .find(|line| line.contains("Stopped before applying anything: spec"))
        .unwrap_or_else(|| panic!("no stop: {both}"));
    assert!(
        failure.contains(r"p\u{200b}kgs/bad.yml' could not be loaded"),
        "{failure}"
    );
    assert!(!failure.contains('\u{200B}'), "a raw U+200B: {failure:?}");
}
