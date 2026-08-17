//! `selfie dotfiles list` and the entries it cannot describe as a source.
//!
//! `DotfileEntry::content_source` returns a `Result`, and this is the consumer
//! furthest from the deploy path — the one most likely to acquire a
//! `let Ok(..) else { continue }` and silently drop a refused entry. An entry
//! present in the package file has to appear in the listing whatever is wrong
//! with it, or a user chasing a dotfile that never deploys is told it does not
//! exist.

pub mod common;

use common::{SELFIE_ENV, sandboxed_command, setup_default_test_config};

// Write a package whose single dotfile is refused, and one that is not.
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

    let output = sandboxed_command(&temp)
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
    // Naming the var is not enough on its own: a listing that treated the entry
    // as a perfectly good template would render `creds/credentials.tpl (vars:
    // not-a-name)`, which also contains the name. This is what tells the two
    // apart, and it is checked this way rather than on the refusal wording so a
    // narrow terminal wrapping the cell cannot make it flap.
    assert!(
        !stdout.contains("(vars:"),
        "an entry that cannot deploy must not be listed as a working template, got:\n{stdout}"
    );
    // The control: the listing really did run and really did render entries, so
    // the assertions above cannot pass by finding text in an error message.
    assert!(
        stdout.contains("bat/config"),
        "the ordinary entry must list normally, got:\n{stdout}"
    );
}

// There is deliberately no test here asserting that listing runs no command.
// `selfie dotfiles list` has no `CommandRunner` wired into it at all, so such a
// test cannot fail for the reason its name would promise, and the only proxy
// available from outside the process — grepping stdout for a shell error — passes
// on any machine where `op` happens to be installed. A test that cannot observe
// the invariant it names is the failure mode records.
// The real guard is `a_var_name_that_cannot_be_substituted_runs_no_command` in
// `crates/selfie/tests/dotfile_service_tests.rs`, which asserts `call_count() == 0`
// against an injected runner and has a positive control proving that runner
// records calls on the same path.
