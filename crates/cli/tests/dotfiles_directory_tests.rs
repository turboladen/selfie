//! What selfie says when the standalone dotfiles directory is not there.
//!
//! The dotfile service reports a missing directory only when the user
//! configured `dotfiles_directory` explicitly, observed here through the CLI
//! across `dotfiles list`, `spec create` and `dotfiles track`.
//!
//! The silent case has its own test and is the load-bearing one: a service
//! that warned whenever the directory was absent would fire on every
//! invocation for everyone who keeps no standalone dotfiles, which is how a
//! diagnostic becomes noise people filter out.

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

// The control: `dotfiles_directory` defaults to a sibling of
// `package_directory`, so an absent default is the ordinary state of anyone
// who keeps no standalone dotfiles. A service that reported it here would
// complain on every invocation forever.
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

// The other control: when the directory is there, its packages are really
// read. A service that treated the directory as missing unconditionally
// would still satisfy every other test in this file, since they only check
// the missing case.
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

    // Which heading it lands under is the only observable proof that the spec
    // was recorded as coming from the dotfiles directory. The package directory
    // here is empty, so a spec attributed to it prints the wrong heading and
    // suppresses the right one -- and every other assertion in this file still
    // passes, because they only ask whether the name appears somewhere.
    assert!(
        stdout.contains("Dotfiles:"),
        "a standalone spec must be listed under its own heading, got:\n{stdout}"
    );
    assert!(
        !stdout.contains("Packages:"),
        "an empty package directory must not get a heading, got:\n{stdout}"
    );
}

// `spec create` is the one namespace check that proceeds to a write when the
// dotfiles directory does not exist — it writes into the *package* directory
// and has no refusal of its own. So it is where a skipped uniqueness check
// would let a colliding name through.
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

// `dotfiles track` copies the file *into* the dotfiles directory, so a
// missing one stops the run before any copy is attempted. Asserting the
// exact count is what would catch the refusal being reported twice, not
// only whether it is reported at all.
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

// Only an empty path is silent at the unset default. A plain file or a dangling
// symlink there cannot come from leaving the setting out, so `selfie track` names it
// before the prompt exactly as it would at a configured path.
#[test]
fn track_reports_an_occupied_unset_default_dotfiles_directory_before_prompting() {
    for (clause, a_plain_file) in [
        ("is a regular file", true),
        ("is a symlink to nothing", false),
    ] {
        let temp = config_without_dotfiles_dir();
        let dotfiles = temp.path().join("dotfiles");
        if a_plain_file {
            std::fs::write(&dotfiles, "not a directory\n").unwrap();
        } else {
            std::os::unix::fs::symlink(temp.path().join("moved-away"), &dotfiles).unwrap();
        }
        let untracked = temp.path().join("untracked.conf");
        std::fs::write(&untracked, "x").unwrap();

        let output = sandboxed_command(&temp)
            .args(["track", untracked.to_str().unwrap()])
            .output()
            .unwrap();
        let combined = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );

        // The refusal is what ends the run, so the warning appearing ahead of it is
        // the evidence it came before any prompt.
        let warning_at = combined
            .find(clause)
            .unwrap_or_else(|| panic!("`{clause}` must be named, got:\n{combined}"));
        let refusal_at = combined
            .find("needs a terminal")
            .unwrap_or_else(|| panic!("the run must end on the refusal, got:\n{combined}"));
        assert!(
            warning_at < refusal_at,
            "`{clause}` must be reported before the run ends, got:\n{combined}"
        );
        assert!(
            combined.contains("cannot be tracked until that is fixed"),
            "the consequence must be stated for `{clause}`, got:\n{combined}"
        );
    }
}

// The control for the test above: with nothing at the unset default, `selfie track`
// says nothing about the directory. Without it, a guard deleted outright would pass.
#[test]
fn track_is_silent_about_an_empty_unset_default_dotfiles_directory() {
    let temp = config_without_dotfiles_dir();
    let untracked = temp.path().join("untracked.conf");
    std::fs::write(&untracked, "x").unwrap();

    let output = sandboxed_command(&temp)
        .args(["track", untracked.to_str().unwrap()])
        .output()
        .unwrap();
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    assert!(
        combined.contains("needs a terminal"),
        "the run must reach the refusal, got:\n{combined}"
    );
    assert!(
        !combined.to_lowercase().contains("dotfiles directory"),
        "an empty unset default must not be reported, got:\n{combined}"
    );
}

// A dotfiles directory a standalone entry cannot be written into is reported by
// `selfie track` before the prompt, not after the user has chosen a name.
//
// The run is not a TTY, so the interactive select cannot be driven: everything this
// test can read was printed before any prompt, which is what makes the ordering
// checkable rather than asserted.
#[test]
fn track_reports_a_dangling_symlink_dotfiles_directory_before_prompting() {
    let temp = tempfile::tempdir().unwrap();
    let config_dir = temp.path().join(".config").join("selfie");
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::create_dir_all(temp.path().join("packages")).unwrap();
    let dotfiles = temp.path().join("dotfiles");
    std::fs::write(
        config_dir.join("config.yaml"),
        format!(
            "environment: {SELFIE_ENV}\npackage_directory: {}\ndotfiles_directory: {}\n",
            temp.path().join("packages").display(),
            dotfiles.display(),
        ),
    )
    .unwrap();
    // A link whose destination was never created: the path is occupied and nothing
    // is behind it, so `mkdir -p` cannot fix it.
    std::os::unix::fs::symlink(temp.path().join("moved-away"), &dotfiles).unwrap();

    let untracked = temp.path().join("untracked.conf");
    std::fs::write(&untracked, "x").unwrap();

    let output = sandboxed_command(&temp)
        .args(["track", untracked.to_str().unwrap()])
        .output()
        .unwrap();

    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        combined.contains("is a symlink to nothing"),
        "the state must be named, got:\n{combined}"
    );
    assert!(
        combined.contains("cannot be tracked until that is fixed"),
        "the consequence must be stated before the prompt, got:\n{combined}"
    );
    // The ordering itself. The refusal is what ends the run, so the warning appearing
    // ahead of it in the stream is the evidence the user learns the directory is
    // unusable before being asked anything.
    let warning_at = combined.find("is a symlink to nothing").unwrap();
    let refusal_at = combined.find("needs a terminal").unwrap();
    assert!(
        warning_at < refusal_at,
        "the directory must be reported before the run ends, got:\n{combined}"
    );
    assert!(
        !combined.contains("mkdir -p"),
        "mkdir -p cannot create a path a link occupies, got:\n{combined}"
    );
}

// The control for the test above: a dotfiles directory that is simply not there is
// reported the same way and does get the `mkdir -p` remedy, because that command
// works. Without this pair, a change that dropped the remedy everywhere would pass.
#[test]
fn track_offers_mkdir_for_a_dotfiles_directory_that_is_merely_absent() {
    let temp = tempfile::tempdir().unwrap();
    let config_dir = temp.path().join(".config").join("selfie");
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::create_dir_all(temp.path().join("packages")).unwrap();
    let dotfiles = temp.path().join("dotfiles");
    std::fs::write(
        config_dir.join("config.yaml"),
        format!(
            "environment: {SELFIE_ENV}\npackage_directory: {}\ndotfiles_directory: {}\n",
            temp.path().join("packages").display(),
            dotfiles.display(),
        ),
    )
    .unwrap();

    let untracked = temp.path().join("untracked.conf");
    std::fs::write(&untracked, "x").unwrap();

    let output = sandboxed_command(&temp)
        .args(["track", untracked.to_str().unwrap()])
        .output()
        .unwrap();

    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(combined.contains("does not exist"), "got:\n{combined}");
    assert!(
        combined.contains(&format!("mkdir -p {}", dotfiles.display())),
        "an absent path is the one case the remedy works for, got:\n{combined}"
    );
}

// `selfie dotfiles track` against an unreadable dotfiles directory reports the
// directory, and does not tell the user their name is unusable. The name may be
// perfectly good; nothing could check it.
//
// Skipped for a user who can read a 0o000 directory, since the fixture cannot be
// built for root and a pass would mean nothing.
#[test]
fn dotfiles_track_blames_the_directory_not_the_name_when_it_cannot_be_read() {
    let temp = tempfile::tempdir().unwrap();
    let config_dir = temp.path().join(".config").join("selfie");
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::create_dir_all(temp.path().join("packages")).unwrap();
    let dotfiles = temp.path().join("dotfiles");
    std::fs::create_dir_all(&dotfiles).unwrap();
    std::fs::write(
        config_dir.join("config.yaml"),
        format!(
            "environment: {SELFIE_ENV}\npackage_directory: {}\ndotfiles_directory: {}\n",
            temp.path().join("packages").display(),
            dotfiles.display(),
        ),
    )
    .unwrap();

    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&dotfiles, std::fs::Permissions::from_mode(0o000)).unwrap();
    }
    if std::fs::read_dir(&dotfiles).is_ok() {
        eprintln!("SKIP dotfiles_track_blames_the_directory_not_the_name_when_it_cannot_be_read");
        return;
    }

    let tracked = temp.path().join("starship.toml");
    std::fs::write(&tracked, "format = \"$all\"").unwrap();

    let output = sandboxed_command(&temp)
        .args(["dotfiles", "track", "starship", tracked.to_str().unwrap()])
        .output()
        .unwrap();

    // Restored before the assertions, so a failure does not leave an unreadable
    // directory behind for the temp dir's own cleanup to trip over.
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&dotfiles, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!output.status.success(), "got:\n{combined}");
    assert!(
        combined.contains("cannot tell whether the name is already taken"),
        "the refusal must say the answer is unknown, got:\n{combined}"
    );
    assert!(
        !combined.contains("Cannot use name"),
        "the name is not what failed, got:\n{combined}"
    );
}
