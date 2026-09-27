pub mod common;

use std::fs;

use common::{SELFIE_ENV, sandboxed_command, setup_default_test_config};

// `spec info` names each entry's content source, and warns that apply runs
// commands for them. Nothing here runs a command: the fixture's commands are
// listed, never executed, and `echo` keeps them inert if that ever changes.
#[test]
fn spec_info_lists_dotfile_sources_and_warns_that_apply_runs_commands() {
    let temp_dir = setup_default_test_config();
    fs::write(
        temp_dir.path().join("packages").join("sourced.yml"),
        format!(
            "name: sourced\n\
             dotfiles:\n\
             \x20 - source: a.tpl\n\
             \x20   target: ~/.a\n\
             \x20   vars:\n\
             \x20     x: echo x\n\
             \x20     y: echo y\n\
             environments:\n\
             \x20 {SELFIE_ENV}:\n\
             \x20   install: \"true\"\n\
             \x20 other:\n\
             \x20   install: \"true\"\n\
             \x20   dotfiles:\n\
             \x20     - command: echo other\n\
             \x20       target: ~/.c\n"
        ),
    )
    .unwrap();

    let output = sandboxed_command(&temp_dir)
        .args(["spec", "info", "sourced"])
        .output()
        .unwrap();

    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.status.code(), Some(0), "{text}");
    assert!(text.contains("a.tpl (vars: x, y)"), "{text}");
    assert!(text.contains("command: echo other"), "{text}");
    assert!(
        text.contains(&format!(
            "selfie apply runs 2 commands in environment '{SELFIE_ENV}'"
        )),
        "{text}"
    );
}

// A spec with only repository files runs nothing, and must not say it does.
#[test]
fn spec_info_says_nothing_about_commands_when_apply_runs_none() {
    let temp_dir = setup_default_test_config();
    fs::write(
        temp_dir.path().join("packages").join("plain.yml"),
        format!(
            "name: plain\n\
             dotfiles:\n\
             \x20 - source: a.conf\n\
             \x20   target: ~/.a\n\
             environments:\n\
             \x20 {SELFIE_ENV}:\n\
             \x20   install: \"true\"\n"
        ),
    )
    .unwrap();

    let output = sandboxed_command(&temp_dir)
        .args(["spec", "info", "plain"])
        .output()
        .unwrap();

    let text = String::from_utf8_lossy(&output.stdout).to_string()
        + &String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(0), "{text}");
    assert!(text.contains("a.conf"), "{text}");
    assert!(!text.contains("selfie apply runs"), "{text}");
}

// A key in an environment this run does not use leaves the spec described in
// full, with one line saying apply would refuse it there.
#[test]
fn spec_info_names_a_refusal_in_another_environment() {
    let temp_dir = setup_default_test_config();
    fs::write(
        temp_dir.path().join("packages").join("partial.yml"),
        format!(
            "name: partial\n\
             environments:\n\
             \x20 {SELFIE_ENV}:\n\
             \x20   install: \"true\"\n\
             \x20 work:\n\
             \x20   install: \"true\"\n\
             \x20   audt: x\n\
             \x20   dotfiles:\n\
             \x20     - source: w.conf\n\
             \x20       target: ~/.w\n"
        ),
    )
    .unwrap();

    let output = sandboxed_command(&temp_dir)
        .args(["spec", "info", "partial"])
        .output()
        .unwrap();

    let text = String::from_utf8_lossy(&output.stdout).to_string()
        + &String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(0), "{text}");
    assert!(text.contains("Environments"), "{text}");
    assert!(
        text.contains("selfie apply would refuse this spec in another environment"),
        "{text}"
    );
    assert!(text.contains("audt"), "{text}");
    assert!(text.contains("work (refused)"), "{text}");
}
