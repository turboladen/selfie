//! `selfie track` on a file some spec already covers.
//!
//! The command reports the target before it prompts for anything, so this path
//! is reachable without driving the interactive select.

pub mod common;

use common::{SELFIE_ENV, sandboxed_command, setup_default_test_config};

// Reported from the spec, not from the argument. The two differ exactly here:
// the spec holds `~/…` and the caller names the same file absolutely, which is
// what `selfie track <tab-completed path>` produces.
#[test]
fn already_tracking_names_the_target_the_spec_holds() {
    let temp = setup_default_test_config();

    // Canonicalized because the child's `~` expansion canonicalizes too, and on
    // macOS the temp dir is reached through a symlink (`/var` -> `/private/var`).
    let home = temp.path().canonicalize().unwrap();

    let config = home.join(".config").join("bat");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::write(config.join("config"), "--theme=ansi").unwrap();

    let packages = home.join("packages");
    std::fs::create_dir_all(packages.join("bat")).unwrap();
    std::fs::write(packages.join("bat/config"), "--theme=ansi").unwrap();
    std::fs::write(
        packages.join("bat.yaml"),
        format!(
            "name: bat\nenvironments:\n  {SELFIE_ENV}:\n    install: \"echo i\"\ndotfiles:\n  \
             - source: \"bat/config\"\n    target: \"~/.config/bat/config\"\n"
        ),
    )
    .unwrap();

    let absolute = config.join("config");
    let output = sandboxed_command(&temp)
        .env("HOME", &home)
        .args(["track", absolute.to_str().unwrap()])
        .output()
        .unwrap();

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("~/.config/bat/config"),
        "should report the spec's target, got:\n{stdout}"
    );
    assert!(
        !stdout.contains(absolute.to_str().unwrap()),
        "should not echo the caller's absolute path, got:\n{stdout}"
    );
}
