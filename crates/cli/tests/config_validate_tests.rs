pub mod common;

use common::{get_command_with_test_config, setup_test_config};

#[test]
fn test_validate_valid_config() {
    // Valid config with all required fields
    let yaml = r#"
environment: "test-env"
package_directory: "/test/packages"
"#;

    let temp_dir = setup_test_config(yaml);
    let mut cmd = get_command_with_test_config(&temp_dir);
    cmd.args(["config", "validate"]);

    cmd.assert()
        .success()
        .stdout(predicates::str::contains("Configuration is valid"));
}

#[test]
fn test_validate_invalid_config() {
    // Invalid config with missing required fields
    let yaml = r#"
# Missing environment
package_directory: "/test/packages"
"#;

    let temp_dir = setup_test_config(yaml);
    let mut cmd = get_command_with_test_config(&temp_dir);
    cmd.args(["config", "validate"]);

    cmd.assert()
        .failure()
        .stderr(predicates::str::contains("environment"));
}

#[test]
fn test_validate_config_with_invalid_path() {
    // Config with invalid package directory (not absolute)
    let yaml = r#"
environment: "test-env"
package_directory: "relative/path"
"#;

    let temp_dir = setup_test_config(yaml);
    let mut cmd = get_command_with_test_config(&temp_dir);
    cmd.args(["config", "validate"]);

    cmd.assert()
        .failure()
        .stderr(predicates::str::contains("exists, but cannot be expanded"));
}
