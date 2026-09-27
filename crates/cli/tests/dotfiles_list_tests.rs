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

// Whether the user keeps standalone dotfiles is settled before the listing runs:
// the repository is built only when the directory is there. Once it is, a
// listing selfie could not perform must not come back as a successful listing
// that happens to be missing everything in that directory.
#[test]
fn an_unreadable_dotfiles_directory_fails_the_listing() {
    use std::os::unix::fs::PermissionsExt as _;

    let temp = setup_default_test_config();
    write_packages(&temp);

    // The sibling of `packages`, which is where an unset `dotfiles_directory`
    // resolves to.
    let dotfiles = temp.path().join("dotfiles");
    std::fs::create_dir_all(&dotfiles).unwrap();
    std::fs::set_permissions(&dotfiles, std::fs::Permissions::from_mode(0o000)).unwrap();

    // Root ignores the mode bits, so check the precondition actually holds
    // rather than inferring it from the user id.
    if std::fs::read_dir(&dotfiles).is_ok() {
        std::fs::set_permissions(&dotfiles, std::fs::Permissions::from_mode(0o755)).unwrap();
        eprintln!("SKIP an_unreadable_dotfiles_directory_fails_the_listing: still readable");
        return;
    }

    let output = sandboxed_command(&temp)
        .args(["dotfiles", "list"])
        .output()
        .unwrap();

    // Before any assertion, so a failure cannot leave the temp dir unremovable.
    std::fs::set_permissions(&dotfiles, std::fs::Permissions::from_mode(0o755)).unwrap();

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !output.status.success(),
        "listing reported success; stderr was: {stderr}"
    );
    // Both halves. The cause on its own leaves the user to work out what it
    // cost them, and the consequence on its own does not say which directory to
    // go and fix.
    assert!(
        stderr.contains("Failed to load standalone dotfiles"),
        "must name what it could not read; stderr was: {stderr}"
    );
    assert!(
        stderr.contains("would be missing entries"),
        "must say the listing is incomplete; stderr was: {stderr}"
    );
}

// A refused entry's cell gives the key's message and not the anchor advice, which
// is too long for a table; apply and drift carry the advice.
#[test]
fn a_shadowing_key_s_cell_gives_the_message_without_the_advice() {
    let base = setup_default_test_config();
    let packages = base.path().join("packages");
    std::fs::create_dir_all(&packages).unwrap();
    std::fs::write(
        packages.join("anchor.yaml"),
        format!(
            "name: anchor\nenvironments:\n  {SELFIE_ENV}:\n    install: \"echo i\"\ndotfiles:\n  \
             - source: \"a.conf\"\n    target: \"~/.a.conf\"\n    _target: \"x\"\n"
        ),
    )
    .unwrap();

    let output = sandboxed_command(&base)
        .args(["dotfiles", "list"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);

    assert!(
        stdout.contains("'_target' cannot be told apart"),
        "{stdout}"
    );
    assert!(!stdout.contains("Anchors are legal"), "{stdout}");
}
