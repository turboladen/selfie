//! Test package file creation helpers to eliminate duplication in service tests.

use crate::constants::{SERVICE_TEST_ENV, TEST_ENV, TEST_VERSION};
use std::{fs, path::PathBuf};
use tempfile::TempDir;

/// Creates a standard test package file with install and check commands.
/// This is the most commonly used package file in service tests.
///
/// # Example
/// ```rust
/// let temp_dir = TempDir::new().unwrap();
/// let package_path = create_test_package_file(&temp_dir, "my-package");
/// ```
#[must_use]
pub fn create_test_package_file(dir: &TempDir, name: &str) -> PathBuf {
    create_package_file_with_check(dir, name, true)
}

/// Creates a test package file with optional check command.
/// Gives you control over whether the package has a check command defined.
///
/// # Arguments
/// * `dir` - Temporary directory to create the package file in
/// * `name` - Name of the package
/// * `has_check` - Whether to include a check command
///
/// # Example
/// ```rust
/// // Package with check command
/// let with_check = create_package_file_with_check(&temp_dir, "pkg1", true);
///
/// // Package without check command
/// let no_check = create_package_file_with_check(&temp_dir, "pkg2", false);
/// ```
///
/// # Panics
///
/// Panics if it can't write the package file to disk.
#[must_use]
pub fn create_package_file_with_check(dir: &TempDir, name: &str, has_check: bool) -> PathBuf {
    let check_command = if has_check {
        format!("\n    check: \"echo 'checking {name}'\"")
    } else {
        String::new()
    };

    let content = format!(
        r#"name: "{name}"
version: "{TEST_VERSION}"
description: "Test package for service layer testing"
homepage: "https://example.com/{name}"

environments:
  {TEST_ENV}:
    install: "echo 'installing {name}'"{check_command}
    dependencies: []
"#
    );

    let file_path = dir.path().join(format!("{name}.yml"));
    fs::write(&file_path, content).unwrap();
    file_path
}

/// Creates a test package file for multiple environments.
/// Useful for testing cross-environment behavior and environment selection.
///
/// # Example
/// ```rust
/// let package_path = create_multi_env_package_file(&temp_dir, "cross-platform-tool");
/// ```
///
/// # Panics
///
/// Panics if it can't write the package file to disk.
#[must_use]
pub fn create_multi_env_package_file(dir: &TempDir, name: &str) -> PathBuf {
    let content = format!(
        r#"name: "{name}"
version: "{TEST_VERSION}"
description: "Multi-environment test package"
homepage: "https://example.com/{name}"

environments:
  {TEST_ENV}:
    install: "echo 'installing {name} in test'"
    check: "echo 'checking {name} in test'"
    dependencies: []
  prod:
    install: "apt-get install {name}"
    check: "which {name}"
    dependencies: ["build-essential"]
  macos:
    install: "brew install {name}"
    check: "which {name}"
    dependencies: []
"#
    );

    let file_path = dir.path().join(format!("{name}.yml"));
    fs::write(&file_path, content).unwrap();
    file_path
}

/// Creates an invalid package file for error testing.
/// Contains malformed YAML that should cause parsing errors.
///
/// # Example
/// ```rust
/// let invalid_path = create_invalid_package_file(&temp_dir, "broken-package");
/// // This file will cause YAML parsing errors when loaded
/// ```
///
/// # Panics
///
/// Panics if it can't write the package file to disk.
#[must_use]
pub fn create_invalid_package_file(dir: &TempDir, name: &str) -> PathBuf {
    let content = r#"# Invalid YAML - syntax error
name: "invalid-package"
version: "1.0.0"
environments:
  test:
    install: "echo 'test'
    # Missing closing quote above - this will cause YAML parse error
"#;

    let file_path = dir.path().join(format!("{name}.yml"));
    fs::write(&file_path, content).unwrap();
    file_path
}

/// Creates a package file missing required fields for validation testing.
/// Contains valid YAML but missing fields required by the package schema.
///
/// # Example
/// ```rust
/// let incomplete_path = create_incomplete_package_file(&temp_dir, "incomplete-pkg");
/// // This file will cause validation errors when processed
/// ```
///
/// # Panics
///
/// Panics if it can't write the package file to disk.
#[must_use]
pub fn create_incomplete_package_file(dir: &TempDir, name: &str) -> PathBuf {
    let content = format!(
        r#"name: "{name}"
# Missing version field - this should cause validation errors
description: "Package missing required fields"
environments:
  {TEST_ENV}:
    install: "echo 'installing'"
    # Missing other potentially required fields
"#
    );

    let file_path = dir.path().join(format!("{name}.yml"));
    fs::write(&file_path, content).unwrap();
    file_path
}

/// Creates a package file with custom fields for advanced testing.
/// Allows you to specify custom YAML content while still using standard structure.
///
/// # Arguments
/// * `dir` - Temporary directory to create the package file in
/// * `name` - Name of the package
/// * `version` - Version string
/// * `environment` - Environment name
/// * `install_cmd` - Install command to use
/// * `check_cmd` - Optional check command
///
/// # Example
/// ```rust
/// let custom_path = create_custom_package_file(
///     &temp_dir,
///     "custom-tool",
///     "2.1.0",
///     "development",
///     "make install",
///     Some("make test")
/// );
/// ```
///
/// # Panics
///
/// Panics if it can't write the package file to disk.
#[must_use]
pub fn create_custom_package_file(
    dir: &TempDir,
    name: &str,
    version: &str,
    environment: &str,
    install_cmd: &str,
    check_cmd: Option<&str>,
) -> PathBuf {
    let check_section = if let Some(cmd) = check_cmd {
        format!("\n    check: \"{cmd}\"")
    } else {
        String::new()
    };

    let content = format!(
        r#"name: "{name}"
version: "{version}"
description: "Custom test package"

environments:
  {environment}:
    install: "{install_cmd}"{check_section}
    dependencies: []
"#
    );

    let file_path = dir.path().join(format!("{name}.yml"));
    fs::write(&file_path, content).unwrap();
    file_path
}

/// Creates a package file with failing commands for error scenario testing.
/// Commands are designed to fail when executed, useful for testing error handling.
///
/// # Example
/// ```rust
/// let failing_path = create_failing_package_file(&temp_dir, "broken-tool");
/// // This package's commands will fail when executed
/// ```
#[must_use]
pub fn create_failing_package_file(dir: &TempDir, name: &str) -> PathBuf {
    create_custom_package_file(
        dir,
        name,
        TEST_VERSION,
        TEST_ENV,
        "exit 1",       // Install command that fails
        Some("exit 1"), // Check command that fails
    )
}

/// Creates a package file with slow commands for timeout testing.
/// Commands include sleep statements to test timeout handling.
///
/// # Arguments
/// * `dir` - Temporary directory to create the package file in
/// * `name` - Name of the package
/// * `sleep_seconds` - Number of seconds for commands to sleep
///
/// # Example
/// ```rust
/// let slow_path = create_slow_package_file(&temp_dir, "slow-tool", 10);
/// // This package's commands will sleep for 10 seconds
/// ```
#[must_use]
pub fn create_slow_package_file(dir: &TempDir, name: &str, sleep_seconds: u32) -> PathBuf {
    create_custom_package_file(
        dir,
        name,
        TEST_VERSION,
        TEST_ENV,
        &format!("sleep {sleep_seconds} && echo 'installed'"),
        Some(&format!("sleep {sleep_seconds} && echo 'checked'")),
    )
}

/// Creates a test package file for service tests using the correct "test" environment.
/// This is specifically for service layer integration tests.
#[must_use]
pub fn create_service_test_package_file(dir: &TempDir, name: &str, has_check: bool) -> PathBuf {
    create_service_test_package_file_with_behavior(
        dir,
        name,
        has_check,
        TestPackageBehavior::CheckSuccess,
    )
}

/// Behavior configuration for test packages
#[derive(Clone, Copy, Debug)]
pub enum TestPackageBehavior {
    /// Check command always succeeds (for testing successful check operations)
    CheckSuccess,
    /// Check command always fails (for testing installation flow)
    CheckFailure,
    /// Realistic behavior: check fails initially, succeeds after install
    InstallFlow,
}

/// Creates a service test package file with configurable behavior
///
/// # Panics
///
/// Panics if it can't write the package file to disk.
#[must_use]
pub fn create_service_test_package_file_with_behavior(
    dir: &TempDir,
    name: &str,
    has_check: bool,
    behavior: TestPackageBehavior,
) -> PathBuf {
    let (check_command, install_command) = if has_check {
        match behavior {
            TestPackageBehavior::CheckSuccess => (
                format!("\n    check: \"echo 'checking {name}'\""),
                format!("echo 'installing {name}'"),
            ),
            TestPackageBehavior::CheckFailure => (
                "\n    check: \"exit 1\"".to_string(),
                format!("echo 'installing {name}'"),
            ),
            TestPackageBehavior::InstallFlow => {
                let unique_id = std::process::id();
                let timestamp = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos();
                let unique_file = format!("/tmp/{name}-{unique_id}-{timestamp}-installed");
                (
                    format!("\n    check: \"test -f {unique_file}\""),
                    format!("echo 'installing {name}' && touch {unique_file}"),
                )
            }
        }
    } else {
        (String::new(), format!("echo 'installing {name}'"))
    };

    let content = format!(
        r#"name: "{name}"
version: "{TEST_VERSION}"
description: "Test package for service layer testing"
homepage: "https://example.com/{name}"

environments:
  {SERVICE_TEST_ENV}:
    install: "{install_command}"{check_command}
    dependencies: []
"#
    );

    let file_path = dir.path().join(format!("{name}.yml"));
    fs::write(&file_path, content).unwrap();
    file_path
}

/// Creates a service test package specifically for install testing (realistic install flow)
#[must_use]
pub fn create_service_install_test_package_file(dir: &TempDir, name: &str) -> PathBuf {
    create_service_test_package_file_with_behavior(
        dir,
        name,
        true,
        TestPackageBehavior::InstallFlow,
    )
}

/// Creates a service test package file with specified dependencies.
///
/// Uses `InstallFlow` behavior (check fails before install, succeeds after).
///
/// # Arguments
/// * `dir` - Temporary directory to create the package file in
/// * `name` - Name of the package
/// * `deps` - List of dependency package names
#[must_use]
pub fn create_service_test_package_file_with_deps(
    dir: &TempDir,
    name: &str,
    deps: &[&str],
) -> PathBuf {
    let unique_id = std::process::id();
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let unique_file = format!("/tmp/{name}-{unique_id}-{timestamp}-installed");
    let deps_yaml: Vec<String> = deps.iter().map(|d| format!("\"{d}\"")).collect();
    let deps_str = deps_yaml.join(", ");

    let content = format!(
        r#"name: "{name}"
version: "{TEST_VERSION}"
description: "Test package with dependencies"
homepage: "https://example.com/{name}"

environments:
  {SERVICE_TEST_ENV}:
    install: "echo 'installing {name}' && touch {unique_file}"
    check: "test -f {unique_file}"
    dependencies: [{deps_str}]
"#
    );

    let file_path = dir.path().join(format!("{name}.yml"));
    fs::write(&file_path, content).unwrap();
    file_path
}

/// Creates a chain of packages where each depends on the next.
///
/// For example, `create_dependency_chain(dir, &["A", "B", "C"])` creates:
/// - A depends on B
/// - B depends on C
/// - C has no dependencies
pub fn create_dependency_chain(dir: &TempDir, chain: &[&str]) {
    for (i, name) in chain.iter().enumerate() {
        let deps: Vec<&str> = if i + 1 < chain.len() {
            vec![chain[i + 1]]
        } else {
            vec![]
        };
        let _ = create_service_test_package_file_with_deps(dir, name, &deps);
    }
}

/// Creates a circular dependency among the given packages.
///
/// For example, `create_circular_dependency(dir, &["A", "B"])` creates:
/// - A depends on B
/// - B depends on A
pub fn create_circular_dependency(dir: &TempDir, cycle: &[&str]) {
    for (i, name) in cycle.iter().enumerate() {
        let next = cycle[(i + 1) % cycle.len()];
        let _ = create_service_test_package_file_with_deps(dir, name, &[next]);
    }
}

/// Creates an invalid package file for service tests using the correct "test" environment.
///
/// # Panics
///
/// Panics if it can't write the package file to disk.
#[must_use]
pub fn create_service_invalid_package_file(dir: &TempDir, name: &str) -> PathBuf {
    let content = r#"# Invalid YAML - syntax error
name: "invalid-package"
version: "1.0.0"
environments:
  test:
    install: "echo 'test'
    # Missing closing quote above - this will cause YAML parse error
"#;

    let file_path = dir.path().join(format!("{name}.yml"));
    fs::write(&file_path, content).unwrap();
    file_path
}
