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

// Helper function to get a command instance with environment variables pointing to our test config
/// # Panics
///
/// Panics if the `selfie-cli` binary cannot be found by `cargo_bin`.
#[must_use]
pub fn get_command_with_test_config(temp_dir: &TempDir) -> Command {
    let mut cmd = Command::cargo_bin(SELFIE_BIN_NAME).unwrap();

    // Override the config directory location
    // This assumes we can add a CLI flag or env var to override the config directory
    cmd.env(
        "SELFIE_CONFIG_DIR",
        temp_dir.path().join(".config").join("selfie"),
    );

    cmd
}

// Helper function to get a command instance
/// # Panics
///
/// Panics if the `selfie-cli` binary cannot be found by `cargo_bin`.
#[must_use]
pub fn get_command() -> Command {
    Command::cargo_bin(SELFIE_BIN_NAME).unwrap()
}
