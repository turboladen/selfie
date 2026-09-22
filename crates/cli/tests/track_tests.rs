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

// The pre-check reports what a spec already tracks, and a spec may hold an entry
// `selfie apply` will always refuse — a relative target, or one naming another
// user's home. Reporting that as plain success with exit 0 tells the user the
// file is handled when no deploy will ever touch it (selfie-63yd).
//
// The library guards this for the three commands that reach it, but this answer
// is given before the library is called at all, so it needs the rule itself.
#[test]
fn already_tracking_refuses_an_entry_apply_would_refuse() {
    let temp = setup_default_test_config();
    let home = temp.path().canonicalize().unwrap();

    let packages = home.join("packages");
    std::fs::create_dir_all(packages.join("bat")).unwrap();
    std::fs::write(packages.join("bat/config"), "--theme=ansi").unwrap();
    // A relative target: `expand_target_path` leaves it relative, so the same
    // string on the command line matches it, and `deploy_target` refuses it.
    std::fs::write(
        packages.join("bat.yaml"),
        format!(
            "name: bat\nenvironments:\n  {SELFIE_ENV}:\n    install: \"echo i\"\ndotfiles:\n  \
             - source: \"bat/config\"\n    target: \"relative/config\"\n"
        ),
    )
    .unwrap();

    let output = sandboxed_command(&temp)
        .env("HOME", &home)
        .args(["track", "relative/config"])
        .output()
        .unwrap();

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let both = format!("{stdout}{stderr}");

    assert_eq!(
        output.status.code(),
        Some(1),
        "an entry that can never deploy must not exit 0, got:\n{both}"
    );
    assert!(
        both.contains("not absolute"),
        "the reason is not given, got:\n{both}"
    );
    // Asserted as an absence: a run that printed both sentences would still tell
    // the user the file is handled.
    assert!(
        !both.contains("Already tracking"),
        "still reports the entry as tracked, got:\n{both}"
    );
}

// The refusal points at the file holding the bad entry. `TargetRejection`'s own
// suggestion is advice for someone choosing a target, and the user here has one
// written down already — what they need is the path to open.
#[test]
fn already_tracking_names_the_spec_to_edit() {
    let temp = setup_default_test_config();
    let home = temp.path().canonicalize().unwrap();

    let packages = home.join("packages");
    std::fs::create_dir_all(packages.join("bat")).unwrap();
    std::fs::write(packages.join("bat/config"), "--theme=ansi").unwrap();
    std::fs::write(
        packages.join("bat.yaml"),
        format!(
            "name: bat\nenvironments:\n  {SELFIE_ENV}:\n    install: \"echo i\"\ndotfiles:\n  \
             - source: \"bat/config\"\n    target: \"relative/config\"\n"
        ),
    )
    .unwrap();

    let output = sandboxed_command(&temp)
        .env("HOME", &home)
        .args(["track", "relative/config"])
        .output()
        .unwrap();

    let both = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    assert!(
        both.contains("bat.yaml"),
        "the spec to edit is not named, got:\n{both}"
    );
    assert!(
        !both.contains("Use '~/.config/file'"),
        "still offers advice for choosing a target, got:\n{both}"
    );
}

// A tracked target that is a symlink is reported by the two library track
// commands. This answer is given before the library is reached, so without its
// own check the command a user is most likely to run stays the silent one.
#[cfg(unix)]
#[test]
fn already_tracking_reports_a_symlinked_target() {
    let temp = setup_default_test_config();
    let home = temp.path().canonicalize().unwrap();

    let config = home.join(".config").join("bat");
    std::fs::create_dir_all(&config).unwrap();
    let destination = home.join("real-config");
    std::fs::write(&destination, "--theme=ansi").unwrap();
    std::os::unix::fs::symlink(&destination, config.join("config")).unwrap();

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

    let output = sandboxed_command(&temp)
        .env("HOME", &home)
        .args(["track", config.join("config").to_str().unwrap()])
        .output()
        .unwrap();

    let both = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    assert_eq!(
        output.status.code(),
        Some(0),
        "an idempotent track must not be refused, got:\n{both}"
    );
    assert!(
        both.contains("symlink"),
        "the symlinked target is not reported, got:\n{both}"
    );
    assert!(
        both.contains("Already tracking"),
        "the entry is tracked and must still be reported so, got:\n{both}"
    );
}

// The control for the test above. Without it the report could be unconditional and
// that test would still pass, which is the whole failure mode a symlink fixture
// cannot see on its own.
#[test]
fn already_tracking_says_nothing_about_a_plain_target() {
    let temp = setup_default_test_config();
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

    let output = sandboxed_command(&temp)
        .env("HOME", &home)
        .args(["track", config.join("config").to_str().unwrap()])
        .output()
        .unwrap();

    let both = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    assert_eq!(output.status.code(), Some(0), "got:\n{both}");
    assert!(
        both.contains("Already tracking"),
        "the entry is tracked and must be reported so, got:\n{both}"
    );
    assert!(
        !both.contains("symlink"),
        "an ordinary target was reported as a symlink, got:\n{both}"
    );
}

// The sudo refusal is decided by `SUDO_UID` against the effective uid, not by
// holding privilege, so a test can set the variable and drive the real binary.
// Both track commands must stop before the service starts: the gate is in the
// library and refuses either way, but a run that prints "Started" first has
// announced work it is about to decline.
#[test]
fn track_commands_refuse_under_sudo_before_the_service_starts() {
    let temp = setup_default_test_config();
    let home = temp.path().canonicalize().unwrap();
    std::fs::write(home.join("thing.toml"), "x = 1").unwrap();

    let packages = home.join("packages");
    std::fs::create_dir_all(&packages).unwrap();
    std::fs::write(
        packages.join("bat.yaml"),
        format!("name: bat\nenvironments:\n  {SELFIE_ENV}:\n    install: \"echo i\"\n"),
    )
    .unwrap();

    // A uid that is not this process's, which is what makes `classify` answer
    // `Sudo`. 0 serves unless the suite itself runs as root, in which case the
    // run is not the one this test is about.
    if nix::unistd::Uid::effective().is_root() {
        eprintln!(
            "SKIP track_commands_refuse_under_sudo_before_the_service_starts: running as root"
        );
        return;
    }

    for args in [
        vec!["dotfiles", "track", "thing", "~/thing.toml"],
        vec!["package", "track-dotfile", "bat", "~/thing.toml"],
    ] {
        let output = sandboxed_command(&temp)
            .env("HOME", &home)
            .env("SUDO_UID", "0")
            .args(&args)
            .output()
            .unwrap();

        let both = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );

        assert_eq!(
            output.status.code(),
            Some(1),
            "{args:?} must refuse under sudo, got:\n{both}"
        );
        // Exit 1 alone proves almost nothing -- a missing config or an unreadable
        // spec satisfies it -- so the refusal itself is asserted, and only then
        // the absence this commit is about.
        assert!(
            both.contains("Refusing to run under sudo"),
            "{args:?} failed for some other reason, got:\n{both}"
        );
        assert!(
            !both.contains("Started"),
            "{args:?} announced work it then declined, got:\n{both}"
        );
    }
}

// **stderr** is not a terminal and stdin is left alone. `FuzzySelect` prompts on
// `Term::stderr`, so `selfie track x 2>log` from a real terminal is the commonest way
// to hit the spin, and a guard testing stdin lets it through: stdin is a tty there.
//
// What this proves depends on where it runs. Attached to a terminal it discriminates:
// a guard testing stdin passes, the prompt spins, and the deadline fails the test.
// Under a runner that already gives the suite no terminal on stdin, both guards
// refuse and it proves only that the command ends and says why. Kept rather than left
// out, because the case it covers is the one a user meets.
#[test]
fn track_without_a_terminal_on_stderr_refuses_rather_than_prompting_forever() {
    use std::io::Read as _;

    let temp = setup_default_test_config();
    let home = temp.path().canonicalize().unwrap();
    let untracked = home.join("untracked.conf");
    std::fs::write(&untracked, "x").unwrap();

    let mut child = common::sandboxed_std_command(&temp)
        .env("HOME", &home)
        .args(["track", untracked.to_str().unwrap()])
        // Inherited deliberately: the subject is stderr.
        .stdin(std::process::Stdio::inherit())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break Some(status);
        }
        if std::time::Instant::now() > deadline {
            child.kill().unwrap();
            child.wait().unwrap();
            break None;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    };

    let mut stderr = String::new();
    child
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut stderr)
        .unwrap();

    let status = status.expect("the command must end rather than prompt with no terminal");
    assert!(
        !status.success(),
        "nothing was tracked, so exiting 0 would mislead a script"
    );
    assert!(
        stderr.contains("needs a terminal"),
        "the refusal must say what is missing, got:\n{stderr}"
    );
}

// The companion case, with both streams redirected, which is what a script or a CI
// job looks like. Without the guard this does not fail, it never returns:
// `FuzzySelect` re-renders its menu forever with no terminal, flooding the output and
// pinning a core. A probe against the unguarded binary produced more than 64MB in 25
// seconds.
//
// So the assertion that matters is that the process ends at all, and the deadline is
// what makes it one. `assert_cmd`'s own runner would hang the suite, which is why
// this drives the child directly and kills it rather than waiting.
#[test]
fn track_without_a_terminal_refuses_rather_than_prompting_forever() {
    use std::io::Read as _;

    let temp = setup_default_test_config();
    let home = temp.path().canonicalize().unwrap();
    let untracked = home.join("untracked.conf");
    std::fs::write(&untracked, "x").unwrap();

    let mut child = common::sandboxed_std_command(&temp)
        .env("HOME", &home)
        .args(["track", untracked.to_str().unwrap()])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break Some(status);
        }
        if std::time::Instant::now() > deadline {
            child.kill().unwrap();
            child.wait().unwrap();
            break None;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    };

    let mut stderr = String::new();
    child
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut stderr)
        .unwrap();

    let status = status.expect("the command must end rather than prompt with no terminal");
    assert!(
        !status.success(),
        "nothing was tracked, so exiting 0 would tell a script the file is handled"
    );
    assert!(
        stderr.contains("needs a terminal"),
        "the refusal must say what is missing, got:\n{stderr}"
    );
    assert!(
        stderr.contains("selfie dotfiles track"),
        "the refusal must name a command that works instead, got:\n{stderr}"
    );
}
