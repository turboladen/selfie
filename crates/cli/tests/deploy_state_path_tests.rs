pub mod common;

// Every route that reports the deploy state names its directory or its file. A
// state_directory holding a line break and U+200B must show as one escaped path
// on each of them, the same way, and the sentence naming it must stay on one
// line, including where a failed write's cause names the path again.

use std::fs;

use common::{CLEAN, FAILED, FOUND, RunOutput, SELFIE_ENV, run_sandboxed};
use tempfile::TempDir;

// The state directory's last component, as the YAML config spells it and as a
// path shows it.
const SPELLED: &str = "st\\u200Ba\\nte";
const SHOWN: &str = r"st\u{200b}a\nte";

// What is at the configured state directory.
#[derive(Clone, Copy)]
enum State {
    // A state file that will not parse.
    Unparsable,
    // Nothing: the first write would create the directory.
    Absent,
    // A directory that refuses a new file.
    Unwritable,
    // A symlink to nothing, at a destination holding U+200B.
    DanglingLink,
}

// A sandbox with one package that deploys one dotfile, a file to track, and the
// state directory set up as `state` says. The package directory is a git
// repository with an upstream, for `sync status`.
struct Sandbox {
    // Dropped first, so the mode is restored before the sandbox is removed.
    locked: Option<test_common::LockedDir>,
    temp: TempDir,
}

impl Sandbox {
    fn new(state: State) -> Self {
        let temp = common::setup_default_test_config();
        let home = temp.path();
        let packages = home.join("packages");
        let directory = home.join("st\u{200B}a\nte");
        fs::create_dir_all(home.join("dotfiles")).unwrap();
        fs::write(packages.join("bat.conf"), "theme = x\n").unwrap();
        fs::write(
            packages.join("bat.yml"),
            format!(
                "name: bat\ndotfiles:\n  - source: bat.conf\n    target: \"~/.batrc\"\n\
                 environments:\n  {SELFIE_ENV}:\n    install: \"true\"\n"
            ),
        )
        .unwrap();
        fs::write(home.join(".trackme"), "a\n").unwrap();
        fs::write(
            home.join(".config/selfie/config.yaml"),
            format!(
                "environment: {SELFIE_ENV}\npackage_directory: {}\nstate_directory: \"{}/{SPELLED}\"\n",
                packages.display(),
                home.display()
            ),
        )
        .unwrap();
        let mut locked = None;
        match state {
            State::Unparsable => {
                fs::create_dir_all(&directory).unwrap();
                fs::write(directory.join("deploy-state.yml"), "not: [valid\n").unwrap();
            }
            State::Absent => {}
            State::Unwritable => {
                locked = Some(test_common::LockedDir::create(&directory, 0o500));
            }
            State::DanglingLink => {
                std::os::unix::fs::symlink(home.join("g\u{200B}one"), &directory).unwrap();
            }
        }
        common::package_repo_with_remote(&temp);
        Self { locked, temp }
    }

    fn run(&self, args: &[&str]) -> RunOutput {
        let mut all = vec!["--no-color"];
        all.extend_from_slice(args);
        run_sandboxed(&self.temp, &all)
    }
}

// The unwritable sandbox, or `None`, after saying so, when this user can write
// into a 0o500 directory, as root can: the run would then deploy, and the test
// would show nothing about the refusal.
fn unwritable(test: &str) -> Option<Sandbox> {
    let sandbox = Sandbox::new(State::Unwritable);
    if sandbox
        .locked
        .as_ref()
        .is_some_and(test_common::LockedDir::refuses_new_files)
    {
        Some(sandbox)
    } else {
        eprintln!("SKIP {test}: this user can write into a 0o500 directory");
        None
    }
}

// Asserts that `run` names the state path escaped, with no raw U+200B anywhere,
// on the line holding `opening`.
fn assert_shows_the_state_path(run: &RunOutput, opening: &str) {
    let both = format!("{}{}", run.stdout, run.stderr);
    assert!(!both.contains('\u{200B}'), "a raw U+200B: {both:?}");
    let line = both
        .lines()
        .find(|line| line.contains(opening))
        .unwrap_or_else(|| panic!("no line holds {opening:?}: {both}"));
    assert!(
        line.contains(SHOWN),
        "the sentence must name the path escaped on its own line: {both}"
    );
}

#[test]
fn apply_names_an_unparsable_state_by_its_path() {
    let sandbox = Sandbox::new(State::Unparsable);

    let run = sandbox.run(&["apply"]);

    assert_shows_the_state_path(&run, "Cannot parse deploy state");
    assert_eq!(run.code, Some(FAILED), "{run:?}");
}

#[test]
fn apply_dry_run_names_an_unparsable_state_by_its_path() {
    let sandbox = Sandbox::new(State::Unparsable);

    let run = sandbox.run(&["apply", "--dry-run"]);

    assert_shows_the_state_path(&run, "continuing as though nothing had been deployed");
    assert_eq!(run.code, Some(CLEAN), "{run:?}");
}

#[test]
fn drift_names_an_unparsable_state_by_its_path() {
    let sandbox = Sandbox::new(State::Unparsable);

    let run = sandbox.run(&["dotfiles", "drift"]);

    assert_shows_the_state_path(&run, "continuing as though nothing had been deployed");
    assert_eq!(run.code, Some(FOUND), "{run:?}");
}

#[test]
fn sync_status_names_an_unparsable_state_by_its_path() {
    let sandbox = Sandbox::new(State::Unparsable);

    let run = sandbox.run(&["sync", "status"]);

    assert_shows_the_state_path(&run, "continuing as though nothing had been deployed");
    assert_eq!(run.code, Some(CLEAN), "{run:?}");
}

#[test]
fn track_names_an_unparsable_state_by_its_path() {
    let sandbox = Sandbox::new(State::Unparsable);

    let run = sandbox.run(&["dotfiles", "track", "rc", "~/.trackme"]);

    assert_shows_the_state_path(&run, "Cannot parse deploy state");
    assert_eq!(run.code, Some(FAILED), "{run:?}");
}

#[test]
fn config_validate_names_an_unparsable_state_by_its_path() {
    let sandbox = Sandbox::new(State::Unparsable);

    let run = sandbox.run(&["config", "validate"]);

    assert_shows_the_state_path(&run, "Cannot parse deploy state");
    assert_eq!(run.code, Some(FAILED), "{run:?}");
}

// A configured directory that is not there yet is named by its path on each
// command that reads the state.
#[test]
fn a_state_directory_not_there_yet_is_named_by_its_path() {
    let sandbox = Sandbox::new(State::Absent);

    for (args, code) in [
        (&["apply", "--dry-run"][..], CLEAN),
        (&["dotfiles", "drift"], FOUND),
        (&["config", "validate"], CLEAN),
    ] {
        let run = sandbox.run(args);

        assert_shows_the_state_path(&run, "is not there yet");
        assert_eq!(run.code, Some(code), "{args:?}: {run:?}");
    }
}

#[test]
fn apply_stopped_before_writing_names_the_state_directory() {
    let Some(sandbox) = unwritable("apply_stopped_before_writing_names_the_state_directory") else {
        return;
    };

    let run = sandbox.run(&["apply"]);

    assert_shows_the_state_path(&run, "Stopped with nothing written");
    assert_eq!(run.code, Some(FAILED), "{run:?}");
}

#[test]
fn track_stopped_before_writing_names_the_state_directory() {
    let Some(sandbox) = unwritable("track_stopped_before_writing_names_the_state_directory") else {
        return;
    };

    let run = sandbox.run(&["dotfiles", "track", "rc", "~/.trackme"]);

    assert_shows_the_state_path(&run, "Nothing was tracked");
    assert_eq!(run.code, Some(FAILED), "{run:?}");
}

// A dry run and a check of the configuration say what a real run would meet,
// naming the directory by its path.
#[test]
fn a_dry_run_and_validate_name_an_unwritable_state_directory() {
    let Some(sandbox) = unwritable("a_dry_run_and_validate_name_an_unwritable_state_directory")
    else {
        return;
    };

    let dry = sandbox.run(&["apply", "--dry-run"]);
    let validate = sandbox.run(&["config", "validate"]);

    assert_shows_the_state_path(&dry, "A real run stops before its first write");
    assert_shows_the_state_path(&validate, "does not accept a new file");
    assert_eq!(dry.code, Some(CLEAN), "{dry:?}");
    assert_eq!(validate.code, Some(FOUND), "{validate:?}");
}

// A state directory that is a symlink to nothing names both the link and its
// destination as paths.
#[test]
fn a_dangling_state_directory_names_its_destination_by_its_path() {
    let sandbox = Sandbox::new(State::DanglingLink);

    let run = sandbox.run(&["apply"]);

    assert_shows_the_state_path(&run, "is a symlink to nothing");
    assert!(run.stderr.contains(r"g\u{200b}one"), "{run:?}");
    assert_eq!(run.code, Some(FAILED), "{run:?}");
}

// The line holding `opening` names the deploy state file's path, escaped, right
// after "IO error: ", once.
fn assert_names_the_failed_write(run: &RunOutput, home: &std::path::Path, opening: &str) {
    assert_shows_the_state_path(run, opening);
    let both = format!("{}{}", run.stdout, run.stderr);
    let line = both.lines().find(|line| line.contains(opening)).unwrap();
    let cause = format!("IO error: {}/{SHOWN}/deploy-state.yml: ", home.display());
    assert_eq!(line.matches(&cause).count(), 1, "{both}");
}

// A state file whose write fails after the directory's checks pass names the
// file, and the error's own path, escaped on one line, for apply and for
// track. An immutable file refuses the rename over it; only macOS lets its
// owner set that flag.
#[cfg(target_os = "macos")]
#[test]
fn a_state_file_selfie_cannot_replace_is_named_by_its_path() {
    // Clears the flag when dropped, so a failed test still leaves a sandbox
    // that can be removed.
    struct Immutable(std::path::PathBuf);
    impl Drop for Immutable {
        fn drop(&mut self) {
            let _ = nix::unistd::chflags(&self.0, nix::sys::stat::FileFlag::empty());
        }
    }

    let sandbox = Sandbox::new(State::Absent);
    let home = sandbox.temp.path();
    let deployed = sandbox.run(&["apply"]);
    assert_eq!(deployed.code, Some(CLEAN), "{deployed:?}");
    fs::write(home.join("packages/bat.conf"), "theme = y\n").unwrap();
    let immutable = Immutable(home.join("st\u{200B}a\nte/deploy-state.yml"));
    nix::unistd::chflags(&immutable.0, nix::sys::stat::FileFlag::UF_IMMUTABLE).unwrap();

    let apply = sandbox.run(&["apply"]);
    let track = sandbox.run(&["dotfiles", "track", "rc", "~/.trackme"]);
    drop(immutable);

    assert_names_the_failed_write(&apply, home, "The deploy state cannot be written");
    assert_names_the_failed_write(&track, home, "The deploy state cannot be written");
    assert_eq!(apply.code, Some(FAILED), "{apply:?}");
    assert_eq!(track.code, Some(FAILED), "{track:?}");
}

// A state directory that stops accepting files partway through apply names
// the state file, and the error's own path, escaped on one line. The second
// entry's command locks the directory after the first entry is recorded, so
// the third entry's record fails. Track runs nothing between its first write
// and its record, so only the macOS test above reaches it.
#[test]
fn apply_names_a_state_it_could_not_record_by_its_path() {
    // Unlocks the directory when dropped, so a failed test still leaves a
    // sandbox that can be removed.
    struct Unlock(std::path::PathBuf);
    impl Drop for Unlock {
        fn drop(&mut self) {
            use std::os::unix::fs::PermissionsExt as _;
            let _ = fs::set_permissions(&self.0, fs::Permissions::from_mode(0o755));
        }
    }

    let sandbox = Sandbox::new(State::Absent);
    let home = sandbox.temp.path();
    let probe = test_common::LockedDir::create(&home.join("probe"), 0o500);
    if !probe.refuses_new_files() {
        eprintln!(
            "SKIP apply_names_a_state_it_could_not_record_by_its_path: this user can write into \
             a 0o500 directory"
        );
        return;
    }
    let state = home.join("st\u{200B}a\nte");
    let lock = home.join("lock.sh");
    fs::write(
        &lock,
        format!("#!/bin/sh\n/bin/chmod 500 '{}'\necho c\n", state.display()),
    )
    .unwrap();
    fs::set_permissions(&lock, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    fs::write(home.join("packages/b.conf"), "b\n").unwrap();
    let spec = [
        "name: bat",
        "dotfiles:",
        "  - source: bat.conf",
        "    target: \"~/.batrc\"",
        &format!("  - command: {}", lock.display()),
        "    target: \"~/.c\"",
        "  - source: b.conf",
        "    target: \"~/.b\"",
        "environments:",
        &format!("  {SELFIE_ENV}:"),
        "    install: \"true\"",
    ];
    fs::write(home.join("packages/bat.yml"), spec.join("\n") + "\n").unwrap();
    let unlock = Unlock(state);

    let run = sandbox.run(&["apply"]);
    drop(unlock);

    assert_names_the_failed_write(&run, home, "but cannot record it");
    assert_eq!(run.code, Some(FAILED), "{run:?}");
}
