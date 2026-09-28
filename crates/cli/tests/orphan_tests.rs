//! What `selfie apply` and `selfie dotfiles drift` print for a target no entry
//! deploys to any more, and what they exit with.

pub mod common;

use common::{SELFIE_ENV, sandboxed_command, setup_default_test_config};

// A config with its own state directory, so the deploy state lands in the
// sandbox whatever `XDG_STATE_HOME` the suite inherits.
fn sandbox() -> tempfile::TempDir {
    let temp = setup_default_test_config();
    let config = temp.path().join(".config/selfie/config.yaml");
    let mut text = std::fs::read_to_string(&config).unwrap();
    text.push_str(&format!(
        "state_directory: {}\n",
        temp.path().join("state").display()
    ));
    std::fs::write(config, text).unwrap();
    temp
}

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

// Drift's summary is a warning, not a success, when it found an orphan.
#[test]
fn drift_warns_in_its_summary_and_exits_zero() {
    let temp = moved_target();
    // Deploys the new target, so the orphan is the only thing drift can find.
    let (code, stdout, stderr) = run(&temp, &["apply", "-y"]);
    assert_eq!(code, Some(0), "{stdout}{stderr}");

    let (code, stdout, stderr) = run(&temp, &["dotfiles", "drift"]);
    assert_eq!(code, Some(0), "{stdout}{stderr}");
    assert!(stderr.contains("Orphaned"), "{stderr}");
    assert!(
        stderr.contains("1 orphaned") && !stdout.contains("1 orphaned"),
        "the summary must be the warning, not the success line:\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        stderr.contains("0 drifted"),
        "nothing but the orphan may make the summary a warning:\n{stderr}"
    );
}
