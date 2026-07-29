//! `selfie dotfiles list` and the entries it cannot describe as a source.
//!
//! `DotfileEntry::content_source` returns a `Result`, and this is the consumer
//! furthest from the deploy path — the one most likely to acquire a
//! `let Ok(..) else { continue }` and silently drop a refused entry. An entry
//! present in the package file has to appear in the listing whatever is wrong
//! with it, or a user chasing a dotfile that never deploys is told it does not
//! exist.

pub mod common;

use common::{SELFIE_ENV, get_command_with_test_config, setup_default_test_config};

/// Write a package whose single dotfile is refused, and one that is not.
fn write_packages(base: &tempfile::TempDir) {
    let packages = base.path().join("packages");
    std::fs::create_dir_all(packages.join("creds")).unwrap();
    std::fs::write(
        packages.join("creds/credentials.tpl"),
        "api_key: {{ api_key }}\n",
    )
    .unwrap();

    // A var name the renderer can never substitute.
    std::fs::write(
        packages.join("creds.yaml"),
        format!(
            "name: creds\nenvironments:\n  {SELFIE_ENV}:\n    install: \"echo i\"\ndotfiles:\n  \
             - source: \"creds/credentials.tpl\"\n    target: \"~/.gem/credentials\"\n    \
             vars:\n      \"not-a-name\": \"op read x\"\n"
        ),
    )
    .unwrap();

    // A perfectly ordinary entry, so the listing is not empty for another reason.
    std::fs::write(
        packages.join("bat.yaml"),
        format!(
            "name: bat\nenvironments:\n  {SELFIE_ENV}:\n    install: \"echo i\"\ndotfiles:\n  \
             - source: \"bat/config\"\n    target: \"~/.config/bat/config\"\n"
        ),
    )
    .unwrap();
}

#[test]
fn a_refused_entry_is_listed_with_the_reason_it_was_refused() {
    let temp = setup_default_test_config();
    write_packages(&temp);

    let output = get_command_with_test_config(&temp)
        .args(["dotfiles", "list"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);

    assert!(
        stdout.contains("~/.gem/credentials"),
        "the refused entry must still be listed, got:\n{stdout}"
    );
    assert!(
        stdout.contains("not-a-name"),
        "and it must say why it cannot deploy, got:\n{stdout}"
    );
    // The control: the listing really did run and really did render entries, so
    // the assertions above cannot pass by finding text in an error message.
    assert!(
        stdout.contains("bat/config"),
        "the ordinary entry must list normally, got:\n{stdout}"
    );
}

#[test]
fn listing_a_refused_entry_runs_none_of_its_commands() {
    // Describing an entry must never execute anything: `op read x` reaches a
    // secret store and can raise a biometric prompt. The command is deliberately
    // one that would fail loudly if it ran in this environment.
    let temp = setup_default_test_config();
    write_packages(&temp);

    let output = get_command_with_test_config(&temp)
        .args(["dotfiles", "list"])
        .output()
        .unwrap();
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    assert!(
        !combined.contains("command not found") && !combined.contains("op:"),
        "listing must not execute the binding command, got:\n{combined}"
    );
}
