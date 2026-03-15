pub mod common;

use std::fs;

use common::{add_package, get_command_with_test_config, setup_default_test_config};
use predicates::prelude::*;
use selfie::package::PackageBuilder;

const SELFIE_ENV: &str = "test-env";

#[test]
fn test_package_list_empty() {
    // Test with no packages
    let temp_dir = setup_default_test_config();
    let packages_dir = temp_dir.path().join("packages");
    fs::create_dir_all(&packages_dir).unwrap();

    let mut cmd = get_command_with_test_config(&temp_dir);
    cmd.args(["package", "list"]);

    // Should succeed but not list any packages
    cmd.assert()
        .success()
        .stdout(predicate::str::contains("No packages found."));
}

#[test]
fn test_package_list_single_package() {
    let temp_dir = setup_default_test_config();

    // Create a single package
    let package = PackageBuilder::default()
        .name("test-package")
        .version("1.0.0")
        .environment(SELFIE_ENV, |b| b.install("echo 'Hello'"))
        .build();

    add_package(&temp_dir, &package);

    let mut cmd = get_command_with_test_config(&temp_dir);
    cmd.args(["package", "list"]);

    cmd.assert()
        .success()
        .stdout(predicate::str::contains("test-package"))
        .stdout(predicate::str::contains("v1.0.0"));
}

#[test]
fn test_package_list_multiple_packages() {
    let temp_dir = setup_default_test_config();

    // Create multiple packages
    let packages = vec![
        PackageBuilder::default()
            .name("package-a")
            .version("1.0.0")
            .environment(SELFIE_ENV, |b| b.install("echo 'Install A'"))
            .build(),
        PackageBuilder::default()
            .name("package-b")
            .version("2.0.0")
            .environment(SELFIE_ENV, |b| b.install("echo 'Install B'"))
            .build(),
        PackageBuilder::default()
            .name("package-c")
            .version("3.0.0")
            .environment("other-env", |b| b.install("echo 'Install C'"))
            .build(),
    ];

    for package in &packages {
        add_package(&temp_dir, package);
    }

    let mut cmd = get_command_with_test_config(&temp_dir);
    cmd.args(["package", "list"]);

    // Should list only packages relevant to current environment (package-a and package-b)
    cmd.assert()
        .success()
        .stdout(predicate::str::contains("package-a"))
        .stdout(predicate::str::contains("package-b"))
        .stdout(predicate::str::contains("v1.0.0"))
        .stdout(predicate::str::contains("v2.0.0"));

    // package-c should NOT be listed since it doesn't support current environment
    let output = cmd.assert().success().get_output().stdout.clone();
    let output_str = String::from_utf8_lossy(&output);
    assert!(!output_str.contains("package-c"));
    assert!(!output_str.contains("v3.0.0"));
}

#[test]
fn test_package_list_with_invalid_yaml() {
    let temp_dir = setup_default_test_config();

    // Create a valid package
    let package = PackageBuilder::default()
        .name("valid-package")
        .version("1.0.0")
        .environment(SELFIE_ENV, |b| b.install("echo 'Valid'"))
        .build();

    add_package(&temp_dir, &package);

    // Add an invalid package file
    let packages_dir = temp_dir.path().join("packages");
    let invalid_path = packages_dir.join("invalid-package.yaml");
    let invalid_yaml = r#"
    name: "invalid-package"
    version: 1.0.0
    invalid_yaml: :::
    "#;

    fs::write(invalid_path, invalid_yaml).unwrap();

    let mut cmd = get_command_with_test_config(&temp_dir);
    cmd.args(["package", "list"]);

    // Should show the valid package but report error for invalid one
    cmd.assert()
        .success()
        .stdout(predicate::str::contains("valid-package"))
        .stderr(predicate::str::contains("invalid-package.yaml"));
}

#[test]
fn test_package_list_different_environments() {
    let temp_dir = setup_default_test_config();

    // Create packages with different environment configurations
    let packages = vec![
        // Package with current environment
        PackageBuilder::default()
            .name("current-env-package")
            .version("1.0.0")
            .environment(SELFIE_ENV, |b| b.install("echo 'Current'"))
            .build(),
        // Package with multiple environments including current
        PackageBuilder::default()
            .name("multi-env-package")
            .version("2.0.0")
            .environment(SELFIE_ENV, |b| b.install("echo 'Multi current'"))
            .environment("other-env", |b| b.install("echo 'Multi other'"))
            .build(),
        // Package without the current environment
        PackageBuilder::default()
            .name("different-env-package")
            .version("3.0.0")
            .environment("other-env", |b| b.install("echo 'Different'"))
            .build(),
    ];

    for package in &packages {
        add_package(&temp_dir, package);
    }

    let mut cmd = get_command_with_test_config(&temp_dir);
    cmd.args(["package", "list"]);

    // Should show only packages relevant to current environment
    let output = cmd.assert().success().get_output().stdout.clone();
    let output_str = String::from_utf8_lossy(&output);

    // Verify only relevant packages are shown
    assert!(output_str.contains("current-env-package"));
    assert!(output_str.contains("multi-env-package"));

    // different-env-package should NOT be shown since it doesn't support current environment
    assert!(!output_str.contains("different-env-package"));
}

#[test]
fn test_package_list_with_no_color_flag() {
    let temp_dir = setup_default_test_config();

    let package = PackageBuilder::default()
        .name("test-package")
        .version("1.0.0")
        .environment(SELFIE_ENV, |b| b.install("echo 'Hello'"))
        .build();

    add_package(&temp_dir, &package);

    let mut cmd = get_command_with_test_config(&temp_dir);
    cmd.args(["--no-color", "package", "list"]);

    // Should not contain ANSI color codes
    let output = cmd.assert().success().get_output().stdout.clone();
    let output_str = String::from_utf8_lossy(&output);
    assert!(!output_str.contains("\x1B["), "Output: {output_str}");
}

#[test]
fn test_package_list_shows_status() {
    let temp_dir = setup_default_test_config();

    // Create a package with a check command
    let package = PackageBuilder::default()
        .name("test-package-with-check")
        .version("1.0.0")
        .environment(SELFIE_ENV, |b| {
            b.install("echo 'Installing'")
                .check(Some("echo 'check command' > /dev/null && exit 0"))
        })
        .build();

    add_package(&temp_dir, &package);

    let mut cmd = get_command_with_test_config(&temp_dir);
    cmd.args(["package", "list"]);

    // Should contain the package name and a status indicator
    cmd.assert()
        .success()
        .stdout(predicate::str::contains("test-package-with-check"))
        .stdout(predicate::str::contains("Installed"));
}

#[test]
fn test_package_list_shows_no_check_status() {
    let temp_dir = setup_default_test_config();

    // Create a package without a check command
    let package = PackageBuilder::default()
        .name("no-check-package")
        .version("1.0.0")
        .environment(SELFIE_ENV, |b| b.install("echo 'Installing'"))
        .build();

    add_package(&temp_dir, &package);

    let mut cmd = get_command_with_test_config(&temp_dir);
    cmd.args(["package", "list"]);

    // Should show the package name and "No check" status
    cmd.assert()
        .success()
        .stdout(predicate::str::contains("no-check-package"))
        .stdout(predicate::str::contains("No check"));
}

#[test]
fn test_package_list_non_existent_directory() {
    let temp_dir = setup_default_test_config();

    // Remove the packages directory that was created
    let packages_dir = temp_dir.path().join("packages");
    fs::remove_dir_all(&packages_dir).unwrap();
    // fs::remove_dir_all(&packages_dir).ok();

    let mut cmd = get_command_with_test_config(&temp_dir);
    cmd.args(["package", "list"]);

    // Should fail with appropriate error about missing directory
    cmd.assert()
        .failure()
        .stderr(predicate::str::contains("Package directory not found"));
}

#[test]
fn test_package_list_all_flag_environment_ordering() {
    let temp_dir = setup_default_test_config();

    // Create packages with multiple environments in different orders
    let packages = vec![
        // Package where current environment is not first alphabetically
        PackageBuilder::default()
            .name("bacon")
            .version("1.0.0")
            .environment("arch-home", |b| b.install("echo 'Install on arch'"))
            .environment(SELFIE_ENV, |b| b.install("echo 'Install on test-env'"))
            .build(),
        // Package where current environment is first alphabetically
        PackageBuilder::default()
            .name("bat")
            .version("1.0.0")
            .environment(SELFIE_ENV, |b| b.install("echo 'Install on test-env'"))
            .environment("ubuntu-server", |b| b.install("echo 'Install on ubuntu'"))
            .build(),
    ];

    for package in &packages {
        add_package(&temp_dir, package);
    }

    let mut cmd = get_command_with_test_config(&temp_dir);
    cmd.args(["package", "list", "--all"]);

    let output = cmd.assert().success().get_output().stdout.clone();
    let output_str = String::from_utf8_lossy(&output);

    // For bacon: current environment (test-env) should come first, then arch-home
    let bacon_line = output_str
        .lines()
        .find(|line| line.contains("bacon"))
        .expect("bacon package should be in output");

    // In streaming spinner format, environments are shown in parentheses at end of line
    assert!(
        bacon_line.contains("*test-env"),
        "Current environment should be marked for bacon: {bacon_line}"
    );
    assert!(
        bacon_line.contains("arch-home"),
        "Should contain arch-home for bacon: {bacon_line}"
    );

    // For bat: current environment (test-env) should come first, then ubuntu-server
    let bat_line = output_str
        .lines()
        .find(|line| line.contains("bat"))
        .expect("bat package should be in output");

    assert!(
        bat_line.contains("*test-env"),
        "Current environment should be marked for bat: {bat_line}"
    );
    assert!(
        bat_line.contains("ubuntu-server"),
        "Should contain ubuntu-server for bat: {bat_line}"
    );
}

#[test]
fn test_package_list_all_flag_shows_all_packages() {
    let temp_dir = setup_default_test_config();

    // Create packages with different environment support
    let packages = vec![
        // Package with current environment
        PackageBuilder::default()
            .name("current-env-package")
            .version("1.0.0")
            .environment(SELFIE_ENV, |b| b.install("echo 'Current'"))
            .build(),
        // Package without current environment
        PackageBuilder::default()
            .name("different-env-package")
            .version("2.0.0")
            .environment("other-env", |b| b.install("echo 'Different'"))
            .build(),
    ];

    for package in &packages {
        add_package(&temp_dir, package);
    }

    // Test default behavior (only relevant packages)
    let mut cmd = get_command_with_test_config(&temp_dir);
    cmd.args(["package", "list"]);

    let output = cmd.assert().success().get_output().stdout.clone();
    let output_str = String::from_utf8_lossy(&output);

    assert!(output_str.contains("current-env-package"));
    assert!(!output_str.contains("different-env-package"));

    // Test --all flag behavior (all packages)
    let mut cmd_all = get_command_with_test_config(&temp_dir);
    cmd_all.args(["package", "list", "--all"]);

    let output_all = cmd_all.assert().success().get_output().stdout.clone();
    let output_all_str = String::from_utf8_lossy(&output_all);

    assert!(output_all_str.contains("current-env-package"));
    assert!(output_all_str.contains("different-env-package"));
    // In streaming format, environments are shown in parentheses on each line
    assert!(
        output_all_str.contains("other-env"),
        "Should show environment names in --all mode"
    );
}

#[test]
fn test_package_list_all_flag_not_relevant_status() {
    let temp_dir = setup_default_test_config();

    // Create a package that doesn't support the current environment
    let package_not_relevant = PackageBuilder::default()
        .name("not-relevant-package")
        .version("1.0.0")
        .environment("other-env", |b| {
            b.install("echo 'Install on other-env'")
                .check(Some("echo 'check on other-env'"))
        })
        .build();

    // Create a package that supports current environment but has no check
    let package_no_check = PackageBuilder::default()
        .name("no-check-package")
        .version("1.0.0")
        .environment(SELFIE_ENV, |b| b.install("echo 'Install on test-env'"))
        .build();

    add_package(&temp_dir, &package_not_relevant);
    add_package(&temp_dir, &package_no_check);

    let mut cmd = get_command_with_test_config(&temp_dir);
    cmd.args(["package", "list", "--all"]);

    let output = cmd.assert().success().get_output().stdout.clone();
    let output_str = String::from_utf8_lossy(&output);

    // Package not relevant to current environment should show N/A
    let not_relevant_line = output_str
        .lines()
        .find(|line| line.contains("not-relevant-package"))
        .expect("not-relevant-package should be in output");
    assert!(
        not_relevant_line.contains("N/A"),
        "Package not relevant should show N/A: {not_relevant_line}"
    );

    // Package with no check command should show "No check"
    let no_check_line = output_str
        .lines()
        .find(|line| line.contains("no-check-package"))
        .expect("no-check-package should be in output");
    assert!(
        no_check_line.contains("No check"),
        "Package with no check should show 'No check': {no_check_line}"
    );
}

#[test]
fn test_package_list_default_behavior_filters_by_environment() {
    let temp_dir = setup_default_test_config();

    // Create a package that supports current environment but has no check
    let package_no_check = PackageBuilder::default()
        .name("no-check-package")
        .version("1.0.0")
        .environment(SELFIE_ENV, |b| b.install("echo 'Install on test-env'"))
        .build();

    // Create a package that supports current environment with check
    let package_with_check = PackageBuilder::default()
        .name("with-check-package")
        .version("1.0.0")
        .environment(SELFIE_ENV, |b| {
            b.install("echo 'Install on test-env'")
                .check(Some("echo 'check on test-env'"))
        })
        .build();

    add_package(&temp_dir, &package_no_check);
    add_package(&temp_dir, &package_with_check);

    let mut cmd = get_command_with_test_config(&temp_dir);
    cmd.args(["package", "list"]);

    let output = cmd.assert().success().get_output().stdout.clone();
    let output_str = String::from_utf8_lossy(&output);

    // Both packages should be shown since they support current environment
    assert!(output_str.contains("no-check-package"));
    assert!(output_str.contains("with-check-package"));

    // Package with no check command should show "No check"
    let no_check_line = output_str
        .lines()
        .find(|line| line.contains("no-check-package"))
        .expect("no-check-package should be in output");
    assert!(
        no_check_line.contains("No check"),
        "Package with no check should show 'No check': {no_check_line}"
    );
}

#[test]
fn test_package_list_environment_mismatch_shows_stats() {
    let temp_dir = setup_default_test_config();

    // Create packages that support different environments but not the current one
    let packages = vec![
        PackageBuilder::default()
            .name("macos-package")
            .version("1.0.0")
            .environment("macos", |b| b.install("echo 'Install on macOS'"))
            .build(),
        PackageBuilder::default()
            .name("ubuntu-package")
            .version("1.0.0")
            .environment("ubuntu", |b| b.install("echo 'Install on Ubuntu'"))
            .environment("debian", |b| b.install("echo 'Install on Debian'"))
            .build(),
        PackageBuilder::default()
            .name("multi-env-package")
            .version("1.0.0")
            .environment("windows", |b| b.install("echo 'Install on Windows'"))
            .environment("macos", |b| b.install("echo 'Install on macOS'"))
            .environment("ubuntu", |b| b.install("echo 'Install on Ubuntu'"))
            .build(),
    ];

    for package in packages {
        add_package(&temp_dir, &package);
    }

    let mut cmd = get_command_with_test_config(&temp_dir);
    cmd.args(["package", "list"]);

    cmd.assert()
        .success()
        .stdout(predicate::str::contains(
            "No packages found for environment 'test-env'.",
        ))
        .stdout(predicate::str::contains(
            "📊 Packages by environment in this directory:",
        ))
        .stdout(predicate::str::contains("Environment"))
        .stdout(predicate::str::contains("Package Count"))
        .stdout(predicate::str::contains("macos"))
        .stdout(predicate::str::contains("ubuntu"))
        .stdout(predicate::str::contains("windows"))
        .stdout(predicate::str::contains("debian"))
        .stdout(predicate::str::contains("💡 Try:"))
        .stdout(predicate::str::contains("--environment <env>"))
        .stdout(predicate::str::contains("--all"));
}
