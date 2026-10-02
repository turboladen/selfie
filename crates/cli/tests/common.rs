use std::{fs, io::Write};

use assert_cmd::Command;
use selfie::package::Package;
use tempfile::TempDir;

pub const SELFIE_ENV: &str = "test-env";
const SELFIE_BIN_NAME: &str = "selfie";

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

    // Create package directory
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
/// - `EDITOR` is removed rather than set, so `spec edit` takes its "not set"
///   branch instead of launching the developer's editor under captured stdio.
///   The environment is inherited, so leaving it alone is not neutral.
///
/// It does **not** sandbox execution: `install`, `check`, `audit` and any
/// `command:` dotfile source run for real on this machine, through a login
/// shell that still sources `/etc/profile`. Nor does it sandbox the network or
/// the developer's credentials — the rest of the environment is inherited, so
/// `sync push` and `sync pull` reach the real remote with the real
/// `SSH_AUTH_SOCK` and any `GIT_*` variables in scope; only `~/.gitconfig`
/// moves with `HOME`. Fixtures must use inert commands such as `true` or
/// `echo`. `SHELL` is pinned for determinism, not safety.
///
/// # Panics
///
/// Panics if the `selfie-cli` binary cannot be found by `cargo_bin`.
#[must_use]
pub fn sandboxed_command(temp_dir: &TempDir) -> Command {
    // Wraps the other builder rather than repeating it. Two copies of the same five
    // variables can drift, and a fixture that then passes under one and fails under
    // the other says nothing about the code.
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

    cmd.env("HOME", temp_dir.path());
    cmd.env("XDG_CONFIG_HOME", temp_dir.path().join(".config"));
    cmd.env(
        "SELFIE_CONFIG_DIR",
        temp_dir.path().join(".config").join("selfie"),
    );
    cmd.env("SHELL", "/bin/sh");
    cmd.env_remove("EDITOR");

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

/// Run `git` with `args` in `dir`, as a fixed identity with commit signing off,
/// so a commit does not depend on the developer's git configuration.
///
/// # Panics
///
/// Panics if git cannot be run or exits non-zero.
pub fn git(dir: &std::path::Path, args: &[&str]) {
    let status = std::process::Command::new("git")
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
