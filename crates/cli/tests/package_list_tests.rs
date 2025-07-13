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

    // Should list all packages
    cmd.assert()
        .success()
        .stdout(predicate::str::contains("package-a"))
        .stdout(predicate::str::contains("package-b"))
        .stdout(predicate::str::contains("package-c"))
        .stdout(predicate::str::contains("v1.0.0"))
        .stdout(predicate::str::contains("v2.0.0"))
        .stdout(predicate::str::contains("v3.0.0"));
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

    // Should show all packages, but mark current environment with *
    let output = cmd.assert().success().get_output().stdout.clone();
    let output_str = String::from_utf8_lossy(&output);

    // Verify current environment marking
    assert!(output_str.contains("current-env-package"));
    assert!(output_str.contains("multi-env-package"));
    assert!(output_str.contains("different-env-package"));

    // Current environments should be marked
    assert!(output_str.contains(&format!("*{SELFIE_ENV}")));

    // The "other-env" should be listed but not marked
    assert!(output_str.contains("other-env"));
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
