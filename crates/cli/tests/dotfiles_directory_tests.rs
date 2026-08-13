//! What selfie says when the standalone dotfiles directory is not there.
//!
//! Every call site shares one helper, which reports the missing directory only
//! when the user configured that path.
//!
//! The silent case has its own test and is the load-bearing one: a helper that
//! warned whenever the directory was absent would fire on every invocation for
//! everyone who keeps no standalone dotfiles, which is how a diagnostic becomes
//! noise people filter out.

pub mod common;

use std::path::Path;

use common::{SELFIE_ENV, sandboxed_command};
use tempfile::TempDir;

const MISSING_DIR_WARNING: &str = "standalone dotfiles will not be read";
const MISSING_DIR_REFUSAL: &str = "Cannot track a standalone dotfile";
/// Either wording — for the controls, which assert nothing is said at all.
const MISSING_DIR_ANY: &str = "dotfiles directory does not exist";

/// A sandbox whose config names a `dotfiles_directory` explicitly.
fn config_with_explicit_dotfiles_dir(dotfiles_dir: &str) -> TempDir {
    let temp = tempfile::tempdir().unwrap();
    let config_dir = temp.path().join(".config").join("selfie");
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::create_dir_all(temp.path().join("packages")).unwrap();

    std::fs::write(
        config_dir.join("config.yaml"),
        format!(
            "environment: {SELFIE_ENV}\npackage_directory: {}\ndotfiles_directory: {}\n",
            temp.path().join("packages").display(),
            dotfiles_dir,
        ),
    )
    .unwrap();

    temp
}

/// A sandbox with no `dotfiles_directory` key, so the sibling default applies —
/// and nothing creates that sibling.
fn config_without_dotfiles_dir() -> TempDir {
    let temp = tempfile::tempdir().unwrap();
    let config_dir = temp.path().join(".config").join("selfie");
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::create_dir_all(temp.path().join("packages")).unwrap();

    std::fs::write(
        config_dir.join("config.yaml"),
        format!(
            "environment: {SELFIE_ENV}\npackage_directory: {}\n",
            temp.path().join("packages").display(),
        ),
    )
    .unwrap();

    temp
}

/// Whether a spec for `name` exists, under either extension selfie accepts.
/// `spec create` writes `.yml`; naming one extension would let the assertion
/// pass for the wrong reason if that ever changed.
fn spec_exists(package_dir: &Path, name: &str) -> bool {
    package_dir.join(format!("{name}.yml")).exists()
        || package_dir.join(format!("{name}.yaml")).exists()
}

fn write_standalone_dotfile(dotfiles_dir: &Path, name: &str) {
    std::fs::create_dir_all(dotfiles_dir).unwrap();
    std::fs::write(
        dotfiles_dir.join(format!("{name}.yaml")),
        format!(
            "name: {name}\nenvironments:\n  {SELFIE_ENV}:\n    install: \"echo i\"\ndotfiles:\n  \
             - source: \"{name}.conf\"\n    target: \"~/.config/{name}.conf\"\n"
        ),
    )
    .unwrap();
}

#[test]
fn an_explicitly_configured_missing_dotfiles_directory_is_reported() {
    let temp = config_with_explicit_dotfiles_dir("/nonexistent/selfie-dotfiles");

    let output = sandboxed_command(&temp)
        .args(["dotfiles", "list"])
        .output()
        .unwrap();
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    assert!(
        combined.contains(MISSING_DIR_WARNING),
        "a configured directory that is not there must be reported, got:\n{combined}"
    );
    // Naming the right directory is the assertion, not just naming one. Both the
    // package and dotfiles directories are in scope in that helper, and
    // reporting the package directory would read as plausible and be useless.
    assert!(
        combined.contains("/nonexistent/selfie-dotfiles"),
        "the message must name the dotfiles directory, got:\n{combined}"
    );
    assert!(
        !combined.contains(&temp.path().join("packages").display().to_string()),
        "the message named the package directory instead, got:\n{combined}"
    );
}

// The control, and the one that fails if the helper is ever "simplified" back to
// a bare `is_dir()` check. `dotfiles_directory` defaults to a sibling of
// `package_directory`, so this is the ordinary state of anyone who keeps no
// standalone dotfiles — a run that complained here would complain on every
// invocation forever.
#[test]
fn an_absent_default_dotfiles_directory_is_silent() {
    let temp = config_without_dotfiles_dir();

    let output = sandboxed_command(&temp)
        .args(["dotfiles", "list"])
        .output()
        .unwrap();
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    assert!(
        !combined.to_lowercase().contains(MISSING_DIR_ANY),
        "an unset dotfiles_directory whose default is absent must not be reported, got:\n{combined}"
    );
    assert!(
        output.status.success(),
        "the run must still succeed, got:\n{combined}"
    );
}

// The other control: when the directory is there, the repository is really
// built and really read. Without this, returning `None` unconditionally would
// satisfy every other test in this file.
#[test]
fn dotfiles_list_includes_standalone_dotfiles() {
    let temp = tempfile::tempdir().unwrap();
    let config_dir = temp.path().join(".config").join("selfie");
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::create_dir_all(temp.path().join("packages")).unwrap();
    let dotfiles_dir = temp.path().join("dotfiles");
    write_standalone_dotfile(&dotfiles_dir, "starship");

    std::fs::write(
        config_dir.join("config.yaml"),
        format!(
            "environment: {SELFIE_ENV}\npackage_directory: {}\ndotfiles_directory: {}\n",
            temp.path().join("packages").display(),
            dotfiles_dir.display(),
        ),
    )
    .unwrap();

    let output = sandboxed_command(&temp)
        .args(["dotfiles", "list"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);

    assert!(
        stdout.contains("starship"),
        "the standalone dotfile must be listed, got:\n{stdout}"
    );
    assert!(
        !stdout.to_lowercase().contains(MISSING_DIR_ANY),
        "an existing directory must not be reported as missing, got:\n{stdout}"
    );
}

// `spec create` is the one namespace check that proceeds to a write when the
// dotfiles repository is absent — it writes into the *package* directory and has
// no refusal of its own. So it is where the check being skipped would let a
// colliding name through.
#[test]
fn spec_create_refuses_a_name_that_collides_with_a_standalone_dotfile() {
    let temp = tempfile::tempdir().unwrap();
    let config_dir = temp.path().join(".config").join("selfie");
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::create_dir_all(temp.path().join("packages")).unwrap();
    let dotfiles_dir = temp.path().join("dotfiles");
    write_standalone_dotfile(&dotfiles_dir, "vim");

    std::fs::write(
        config_dir.join("config.yaml"),
        format!(
            "environment: {SELFIE_ENV}\npackage_directory: {}\ndotfiles_directory: {}\n",
            temp.path().join("packages").display(),
            dotfiles_dir.display(),
        ),
    )
    .unwrap();

    let output = sandboxed_command(&temp)
        .args(["spec", "create", "vim"])
        .output()
        .unwrap();
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    assert!(
        combined.contains("Name conflict"),
        "the collision with the standalone dotfile must be reported, got:\n{combined}"
    );
    assert!(
        !spec_exists(&temp.path().join("packages"), "vim"),
        "a colliding spec must not have been written"
    );
}

// The counterpart, and the decision it pins: with no dotfiles directory there
// are no standalone dotfiles, so the uniqueness check has nothing to miss and
// creation proceeds. A "the namespace could not be fully checked" notice here
// would fire on every `spec create` for everyone who keeps no standalone
// dotfiles.
#[test]
fn spec_create_succeeds_quietly_when_there_is_no_dotfiles_directory() {
    let temp = config_without_dotfiles_dir();

    let output = sandboxed_command(&temp)
        .args(["spec", "create", "vim"])
        .output()
        .unwrap();
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    assert!(
        output.status.success(),
        "creation must proceed, got:\n{combined}"
    );
    assert!(
        !combined.to_lowercase().contains(MISSING_DIR_ANY),
        "an absent default must not be reported, got:\n{combined}"
    );
    assert!(
        spec_exists(&temp.path().join("packages"), "vim"),
        "the spec must have been written"
    );
}

// `dotfiles track` copies the file *into* the dotfiles directory, so a missing
// one stops the run. Asserting the count rather than mere presence is what
// catches the other failure: the reading helper warning a line before the
// refusal says the same thing.
#[test]
fn dotfiles_track_refuses_once_when_the_dotfiles_directory_is_missing() {
    let temp = config_with_explicit_dotfiles_dir("/nonexistent/selfie-dotfiles");
    let file = temp.path().join("tracked.conf");
    std::fs::write(&file, "value = 1\n").unwrap();

    let output = sandboxed_command(&temp)
        .args(["dotfiles", "track", "tracked", file.to_str().unwrap()])
        .output()
        .unwrap();
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    assert_eq!(
        output.status.code(),
        Some(1),
        "tracking into a directory that is not there must fail, got:\n{combined}"
    );
    assert_eq!(
        combined.matches(MISSING_DIR_REFUSAL).count(),
        1,
        "the refusal must be reported exactly once, got:\n{combined}"
    );
}
