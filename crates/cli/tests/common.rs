use std::{fs, io::Write};

use assert_cmd::Command;
use selfie::package::{Package, PackageBuilder};
use tempfile::TempDir;

pub const SELFIE_ENV: &str = "test-env";
const SELFIE_BIN_NAME: &str = "selfie";

/// Exit code of a command that did what it was asked and found nothing to report.
pub const CLEAN: i32 = 0;
/// Exit code of an error, a refusal, a declined prompt, or a run that ends without a result.
pub const FAILED: i32 = 1;
/// Exit code of a command line that could not be parsed, or a command that had to ask and had no
/// terminal.
pub const USAGE: i32 = 2;
/// Exit code of a command that finished with something to report: a finding, or a package it could
/// not answer for.
pub const FOUND: i32 = 3;
/// Exit code of a run canceled with Ctrl+C, the usual `128 + SIGINT` value.
pub const CANCELED: i32 = 130;

// Helper to create a temporary config environment
#[must_use]
pub fn setup_default_test_config() -> TempDir {
    setup_optional_test_config(None)
}

// Helper to create a temporary config environment
#[must_use]
pub fn setup_test_config(config_yaml: &str) -> TempDir {
    setup_optional_test_config(Some(config_yaml))
}

fn setup_optional_test_config(config_yaml: Option<&str>) -> TempDir {
    let temp_dir = tempfile::tempdir().unwrap();

    // Create config directory
    let config_dir = temp_dir.path().join(".config").join("selfie");
    fs::create_dir_all(&config_dir).unwrap();

    // Create package directory. selfie runs install and check commands here, so a
    // fixture's relative `touch installed` writes only inside this throwaway
    // sandbox. A fixture needs that state when its check has to say not installed
    // before the install and installed after it.
    let package_dir = temp_dir.path().join("packages");
    fs::create_dir_all(&package_dir).unwrap();

    let config_path = config_dir.join("config.yaml");
    let mut config_file = fs::File::create(&config_path).unwrap();

    if let Some(yaml) = config_yaml {
        config_file.write_all(yaml.as_bytes()).unwrap();
    } else {
        // Write minimal valid config
        writeln!(config_file, "environment: {SELFIE_ENV}").unwrap();
        writeln!(
            config_file,
            "package_directory: {}",
            temp_dir.path().join("packages").display()
        )
        .unwrap();
    }
    temp_dir
}

/// Set `command_timeout` to one second, the shortest the config accepts, so a
/// check that sleeps past it gives no answer.
///
/// # Panics
///
/// Panics if the config file cannot be read or written.
pub fn set_one_second_timeout(base_dir: &TempDir) {
    let config_path = base_dir.path().join(".config/selfie/config.yaml");
    let mut config = fs::read_to_string(&config_path).unwrap();
    config.push_str("command_timeout: 1\n");
    fs::write(&config_path, config).unwrap();
}

/// Write `<name>.yaml` into the package directory, with `install`, an
/// optional `check` and `dependencies` for [`SELFIE_ENV`].
///
/// # Panics
///
/// Panics if the package cannot be serialized or its file written.
pub fn write_package(
    base_dir: &TempDir,
    name: &str,
    install: &str,
    check: Option<&str>,
    dependencies: &[&str],
) {
    let package = PackageBuilder::default()
        .name(name)
        .environment(SELFIE_ENV, |env| {
            let env = env.install(install).dependencies(dependencies.to_vec());
            match check {
                Some(check) => env.check_some(check),
                None => env,
            }
        })
        .build();
    add_package(base_dir, &package);
}

/// # Panics
///
/// Panics if:
/// - YAML serialization of the package fails
/// - The packages directory cannot be created
/// - Writing the package file fails
pub fn add_package(base_dir: &TempDir, package: &Package) {
    let yaml = selfie::yaml::serialize(package).unwrap();
    let packages_path = base_dir.path().join("packages");
    fs::create_dir_all(&packages_path).unwrap();
    let package_path = packages_path.join(format!("{}.yaml", package.name()));

    fs::write(package_path, yaml).unwrap();
}

/// A `selfie` command whose every path lookup lands inside `temp_dir`.
///
/// Use this for any test that runs the binary. Each variable closes a different
/// route to the developer's own files, so they are set together:
///
/// `config_dir` takes `SELFIE_CONFIG_DIR` when it is set, and otherwise asks
/// etcetera for `choose_app_strategy` — the XDG strategy on every platform
/// except Windows, so `$XDG_CONFIG_HOME/selfie` and then `$HOME/.config/selfie`.
/// All three are set rather than only the first, so the sandbox still holds if
/// a caller clears `SELFIE_CONFIG_DIR` or that resolution order changes.
///
/// - `SELFIE_CONFIG_DIR` is the route selfie takes today.
/// - `XDG_CONFIG_HOME` and `HOME` are the next two, in that order.
/// - `HOME` additionally decides where a `~` dotfile target is written and
///   where deploy state goes when no `state_directory` is configured, so it is
///   load-bearing even when the config file is found by the first route.
/// - `EDITOR` is not passed, so `spec edit` takes its "not set" branch instead
///   of launching the developer's editor under captured stdio.
///
/// It does **not** sandbox execution: `install`, `check`, `audit` and any
/// `command:` dotfile source run for real on this machine, through a login
/// shell that still sources `/etc/profile`, and a remote a fixture names is
/// reached over the real network. The environment starts empty: only `PATH`,
/// `TMPDIR`, the sandbox's `HOME`, `XDG_CONFIG_HOME` and `SELFIE_CONFIG_DIR`,
/// `SHELL`, and settings that turn off git's global and system configuration
/// are passed, so no `GIT_*` variable, credential socket or editor of the
/// developer's reaches the run.
/// Fixtures must use inert commands such as `true` or `echo`. `SHELL` is
/// pinned for determinism, not safety.
///
/// # Panics
///
/// Panics if the `selfie-cli` binary cannot be found by `cargo_bin`.
#[must_use]
pub fn sandboxed_command(temp_dir: &TempDir) -> Command {
    // Wraps the other builder rather than repeating it. Two copies of one
    // environment can drift, and a fixture that then passes under one and fails
    // under the other says nothing about the code.
    Command::from_std(sandboxed_std_command(temp_dir))
}

/// The same sandbox as [`sandboxed_command`], as a [`std::process::Command`].
///
/// For a test that has to spawn the child and watch it rather than wait on it:
/// `assert_cmd`'s runner waits for completion, which hangs the suite when the point
/// of the test is that the command might never end.
///
/// # Panics
///
/// Panics if the `selfie-cli` binary cannot be found by `cargo_bin`.
#[must_use]
pub fn sandboxed_std_command(temp_dir: &TempDir) -> std::process::Command {
    let binary = Command::cargo_bin(SELFIE_BIN_NAME).unwrap();
    let mut cmd = std::process::Command::new(binary.get_program());

    // Nothing of the caller's environment reaches the run but what is added
    // back here: no outer git's `GIT_DIR` or `-c` settings, no `EDITOR`.
    cmd.env_clear();
    for kept in ["PATH", "TMPDIR"] {
        if let Some(value) = std::env::var_os(kept) {
            cmd.env(kept, value);
        }
    }
    cmd.env("HOME", temp_dir.path());
    cmd.env("XDG_CONFIG_HOME", temp_dir.path().join(".config"));
    cmd.env(
        "SELFIE_CONFIG_DIR",
        temp_dir.path().join(".config").join("selfie"),
    );
    cmd.env("SHELL", "/bin/sh");
    // The git selfie runs reads no global or system configuration.
    cmd.env("GIT_CONFIG_GLOBAL", "/dev/null");
    cmd.env("GIT_CONFIG_NOSYSTEM", "1");
    cmd.env("GIT_ATTR_NOSYSTEM", "1");

    cmd
}

/// A `selfie` command with no sandbox at all.
///
/// Only for runs that exit before any config, `HOME` or `EDITOR` lookup: a clap
/// usage error, `--help`, or a caller that sets its own `SELFIE_CONFIG_DIR`.
/// Anything reaching a command handler wants [`sandboxed_command`], or it reads
/// the developer's own config and writes their home directory.
///
/// # Panics
///
/// Panics if the `selfie-cli` binary cannot be found by `cargo_bin`.
#[must_use]
pub fn get_command() -> Command {
    Command::cargo_bin(SELFIE_BIN_NAME).unwrap()
}

/// The default config plus a `state_directory` inside the sandbox, so the
/// deploy state lands there whatever `XDG_STATE_HOME` the suite inherits.
///
/// The directory itself is not created, and `config validate` warns that it is
/// not there yet. A test of a clean config uses [`setup_default_test_config`].
///
/// # Panics
///
/// Panics if the sandbox or its config file cannot be written.
#[must_use]
pub fn setup_test_config_with_state_directory() -> TempDir {
    let temp = setup_default_test_config();
    let config = temp.path().join(".config/selfie/config.yaml");
    let mut text = fs::read_to_string(&config).unwrap();
    text.push_str(&format!(
        "state_directory: {}\n",
        temp.path().join("state").display()
    ));
    fs::write(config, text).unwrap();
    temp
}

/// What a finished `selfie` run printed, and how it exited.
pub struct RunOutput {
    /// The exit code, or `None` when a signal ended the run.
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

/// Run `selfie` with `args` in `temp_dir`'s sandbox (see [`sandboxed_command`])
/// and capture what it printed.
///
/// # Panics
///
/// Panics if the binary cannot be run.
#[must_use]
pub fn run_sandboxed(temp_dir: &TempDir, args: &[&str]) -> RunOutput {
    let output = sandboxed_std_command(temp_dir).args(args).output().unwrap();
    RunOutput {
        code: output.status.code(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    }
}

/// Run `git` with `args` in `dir`, as a fixed identity with commit signing off
/// and no global or system configuration read, so neither the result nor the
/// run depends on the developer's git configuration: no signer, credential
/// helper or hook path of theirs is reached.
///
/// # Panics
///
/// Panics if git cannot be run or exits non-zero.
pub fn git(dir: &std::path::Path, args: &[&str]) {
    let status = isolated_git()
        .arg("-C")
        .arg(dir)
        .args([
            "-c",
            "user.email=t@t.example",
            "-c",
            "user.name=t",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .status()
        .unwrap();
    assert!(status.success(), "git {args:?} failed in {dir:?}");
}

pub use test_common::isolated_git;

/// Make `temp_dir`'s package directory a git repository holding what is there
/// now as one commit, with a bare clone beside it as its upstream `origin`. The
/// repository's own config carries a fixed identity with signing off, so a
/// commit selfie makes there does not depend on the developer's either.
///
/// # Panics
///
/// Panics if any git command fails.
pub fn package_repo_with_remote(temp_dir: &TempDir) {
    let packages = temp_dir.path().join("packages");
    let remote = temp_dir.path().join("remote.git");
    git(&packages, &["init", "-q", "-b", "main"]);
    git(&packages, &["config", "user.email", "t@t.example"]);
    git(&packages, &["config", "user.name", "t"]);
    git(&packages, &["config", "commit.gpgsign", "false"]);
    git(&packages, &["add", "-A"]);
    git(&packages, &["commit", "-q", "-m", "init"]);
    git(
        temp_dir.path(),
        &[
            "clone",
            "-q",
            "--bare",
            packages.to_str().unwrap(),
            remote.to_str().unwrap(),
        ],
    );
    git(
        &packages,
        &["remote", "add", "origin", remote.to_str().unwrap()],
    );
    git(&packages, &["fetch", "-q", "origin"]);
    git(&packages, &["branch", "-q", "-u", "origin/main"]);
}
