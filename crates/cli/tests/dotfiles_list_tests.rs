//! `selfie dotfiles list` and the entries it cannot describe as a source.
//!
//! `DotfileEntry::content_source` returns a `Result`, and this is the consumer
//! furthest from the deploy path — the one most likely to acquire a
//! `let Ok(..) else { continue }` and silently drop a refused entry. An entry
//! present in the package file has to appear in the listing whatever is wrong
//! with it, or a user chasing a dotfile that never deploys is told it does not
//! exist.

pub mod common;

use common::{CLEAN, FAILED, FOUND, SELFIE_ENV, sandboxed_command, setup_default_test_config};

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
    write_readable_sibling(base);
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

    // An invalid entry inside a readable spec is shown as a row and stays Clean;
    // only a spec the listing could not read makes it Found.
    assert_eq!(output.status.code(), Some(CLEAN), "{stdout}");
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
    // The column holds a command or a rendered template for a secret-bearing
    // entry, not a source path, so its header must not say "Source".
    assert!(
        stdout.contains("Content"),
        "the table's third column must be headed 'Content', got:\n{stdout}"
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
    assert_eq!(
        output.status.code(),
        Some(FAILED),
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

// A package spec declaring no environment is a refused row with apply's
// reason, in place of its entries, and the listing exits 3 like `package list`.
#[test]
fn a_package_spec_declaring_no_environment_is_a_refused_row() {
    let spec = "name: noenv\ndotfiles:\n  - source: \"odd.conf\"\n    target: \"~/.odd.conf\"\n";
    assert_refusal_exits_found(
        &[("noenv.yaml", spec.to_string())],
        "noenv",
        "At least one environment must be defined",
    );
}

// A readable package beside a refused spec, so each test below can tell a
// listing that carried on from one that stopped.
fn write_readable_sibling(base: &tempfile::TempDir) {
    let packages = base.path().join("packages");
    std::fs::create_dir_all(&packages).unwrap();
    std::fs::write(
        packages.join("bat.yaml"),
        format!(
            "name: bat\nenvironments:\n  {SELFIE_ENV}:\n    install: \"echo i\"\ndotfiles:\n  \
             - source: \"bat/config\"\n    target: \"~/.config/bat/config\"\n"
        ),
    )
    .unwrap();
}

// Lists with `files` (name, content) beside a readable package, and asserts the
// listing exits 3, warns about `refused` with a message containing `reason`, lists
// the readable entry, and lists none of the refused spec's `~/.odd.conf`.
fn assert_refusal_exits_found(files: &[(&str, String)], refused: &str, reason: &str) -> String {
    let base = setup_default_test_config();
    write_readable_sibling(&base);
    for (name, content) in files {
        std::fs::write(base.path().join("packages").join(name), content).unwrap();
    }

    let output = sandboxed_command(&base)
        .args(["dotfiles", "list"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert_eq!(output.status.code(), Some(FOUND), "{stdout}{stderr}");
    assert!(
        stderr.contains(&format!("Cannot read the dotfiles in '{refused}'")),
        "{stderr}"
    );
    assert!(stderr.contains(reason), "{stderr}");
    // The closing line names what the exit code found.
    assert!(
        format!("{stdout}{stderr}").contains(&format!("could not answer for: {refused}")),
        "{stdout}{stderr}"
    );
    assert!(stdout.contains("~/.config/bat/config"), "{stdout}");
    assert!(!stdout.contains("~/.odd.conf"), "{stdout}");
    format!("{stdout}{stderr}")
}

fn spec_with(extra_top: &str, extra_env: &str) -> String {
    format!(
        "name: odd\n{extra_top}environments:\n  {SELFIE_ENV}:\n    install: \"echo i\"\n{extra_env}\
         dotfiles:\n  - source: \"odd.conf\"\n    target: \"~/.odd.conf\"\n"
    )
}

#[test]
fn an_unknown_top_level_key_exits_found() {
    assert_refusal_exits_found(
        &[("odd.yaml", spec_with("bogus: 1\n", ""))],
        "odd",
        "unknown field 'bogus'; expected one of: name, homepage",
    );
}

#[test]
fn an_unknown_environment_key_exits_found() {
    assert_refusal_exits_found(
        &[("odd.yaml", spec_with("", "    bogus: 1\n"))],
        "odd",
        "in environment 'test-env': unknown field 'bogus'",
    );
}

// One run names both levels, so the user fixes the file in one pass.
#[test]
fn unknown_keys_at_two_levels_are_named_together() {
    let output = assert_refusal_exits_found(
        &[("odd.yaml", spec_with("bogus: 1\n", "    bogus2: 1\n"))],
        "odd",
        "unknown field 'bogus'; expected one of: name, homepage",
    );
    assert!(
        output.contains(". In environment 'test-env': unknown field 'bogus2'"),
        "{output}"
    );
}

#[test]
fn an_unchecked_top_level_exits_found() {
    assert_refusal_exits_found(
        &[("odd.yaml", spec_with("extra:\n  ? [a, b]\n  : v\n", ""))],
        "odd",
        "its top-level keys could not be checked",
    );
}

#[test]
fn an_ambiguous_name_exits_found() {
    let spec = spec_with("", "");
    let stderr = assert_refusal_exits_found(
        &[("odd.yaml", spec.clone()), ("odd.yml", spec)],
        "odd",
        "Multiple packages found with name `odd`",
    );
    // Both files are named, one warning each.
    assert_eq!(
        stderr.matches("Cannot read the dotfiles in 'odd'").count(),
        2,
        "{stderr}"
    );
}

// A spec that does not parse is as unread as a refused one.
#[test]
fn a_spec_that_does_not_parse_exits_found() {
    let base = setup_default_test_config();
    write_readable_sibling(&base);
    std::fs::write(
        base.path().join("packages/broken.yaml"),
        "name: [unterminated\n",
    )
    .unwrap();

    let output = sandboxed_command(&base)
        .args(["dotfiles", "list"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert_eq!(output.status.code(), Some(FOUND), "{stdout}{stderr}");
    assert!(stderr.contains("broken"), "{stderr}");
    assert!(
        format!("{stdout}{stderr}").contains("could not answer for: broken"),
        "{stdout}{stderr}"
    );
    assert!(stdout.contains("~/.config/bat/config"), "{stdout}");
}

// The control: nothing refused, so the listing is clean.
#[test]
fn a_listing_that_refused_nothing_exits_clean() {
    let base = setup_default_test_config();
    write_readable_sibling(&base);

    let output = sandboxed_command(&base)
        .args(["dotfiles", "list"])
        .assert()
        .code(CLEAN)
        .get_output()
        .clone();
    let both = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    // The row is there, so the absence below is not a listing that printed nothing.
    assert!(both.contains("~/.config/bat/config"), "{both}");
    assert!(!both.contains("Listed dotfiles across"), "{both}");
}

// Two install-only files claiming one name are refused rows too, though neither
// declares a dotfile.
#[test]
fn an_install_only_ambiguous_pair_exits_found() {
    let install_only = format!("name: nv\nenvironments:\n  {SELFIE_ENV}:\n    install: \"true\"\n");
    let stderr = assert_refusal_exits_found(
        &[("nv.yml", install_only.clone()), ("nv.yaml", install_only)],
        "nv",
        "Multiple packages found with name `nv`",
    );
    assert_eq!(
        stderr.matches("could not answer for: nv (refused)").count(),
        1,
        "{stderr}"
    );
}
