// What `selfie apply` does with a secret-bearing entry's target, asserted against
// the real binary: the exit code and the words a user reads are the contract.

pub mod common;

use std::os::unix::fs::PermissionsExt as _;

use common::{SELFIE_ENV, sandboxed_command, setup_default_test_config};

// `--yes` accepts conflicts, and never one over a target selfie could not read. A
// credential there may be the only copy, and nothing about it is recorded, so an
// overwrite would destroy it for good. It is refused before the provider command
// runs, since a provider can raise an authentication prompt for a deploy that could
// only be refused.
#[test]
fn apply_yes_refuses_a_secret_target_it_cannot_read_before_running_anything() {
    let temp = setup_default_test_config();
    let root = temp.path();
    let target = root.join("target").join("credentials");
    std::fs::create_dir_all(root.join("target")).unwrap();
    std::fs::write(
        root.join("packages/creds.yaml"),
        format!(
            "name: creds\nenvironments:\n  {SELFIE_ENV}:\n    install: \"true\"\ndotfiles:\n  \
             - command: \"echo token\"\n    target: \"{}\"\n",
            target.display()
        ),
    )
    .unwrap();
    std::fs::write(&target, "EXISTING").unwrap();
    std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o200)).unwrap();
    if std::fs::read(&target).is_ok() {
        eprintln!(
            "SKIP apply_yes_refuses_a_secret_target_it_cannot_read_before_running_anything: \
             running as root, mode bits ignored"
        );
        return;
    }

    let output = sandboxed_command(&temp)
        .args(["apply", "--yes"])
        .output()
        .unwrap();
    let mode = std::fs::metadata(&target).unwrap().permissions().mode() & 0o7777;
    std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o600)).unwrap();
    let all = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    assert_eq!(output.status.code(), Some(1), "output:\n{all}");
    assert!(
        all.contains("could not be read")
            && all.contains(&*target.to_string_lossy())
            && all.contains("No command was run"),
        "the refusal must say the target could not be read, name it, and say nothing \
         ran:\n{all}"
    );
    assert!(
        all.contains("0 conflict(s), 1 refused"),
        "a target selfie could not read is refused, not a conflict:\n{all}"
    );
    assert_eq!(mode, 0o200, "the target's mode must be untouched");
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "EXISTING");
}

// A secret-bearing conflict with no terminal to ask on is left alone, and the run
// says only a terminal can accept it: `--yes` does not.
#[test]
fn apply_without_a_terminal_says_only_a_terminal_accepts_a_secret_conflict() {
    let temp = setup_default_test_config();
    let target = temp.path().join("credentials");
    std::fs::write(
        temp.path().join("packages/creds.yaml"),
        format!(
            "name: creds\nenvironments:\n  {SELFIE_ENV}:\n    install: \"true\"\ndotfiles:\n  \
             - command: \"echo token\"\n    target: \"{}\"\n",
            target.display()
        ),
    )
    .unwrap();
    std::fs::write(&target, "EXISTING").unwrap();

    let out = common::run_sandboxed(&temp, &["apply"]);

    assert_eq!(out.code, Some(0), "{}{}", out.stdout, out.stderr);
    assert!(
        out.stderr.contains(
            "1 secret-bearing conflict was left as it is with no terminal to ask on. Only a \
             terminal can accept one; --yes does not."
        ),
        "{}",
        out.stderr
    );
    assert!(!out.stderr.contains("Pass --yes"), "{}", out.stderr);
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "EXISTING");
}
