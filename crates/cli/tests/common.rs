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
    let yaml = serde_saphyr::to_string(package).unwrap();
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
    let mut cmd = Command::cargo_bin(SELFIE_BIN_NAME).unwrap();

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
