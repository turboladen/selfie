//! Package creation helpers for tests to eliminate repetitive `PackageBuilder` usage.

use crate::constants::{ALT_TEST_ENV, TEST_ENV};
use selfie::package::{Package, PackageBuilder};

/// Creates a simple test package with just a name and install command.
/// This is the most basic package used in many tests.
#[must_use]
pub fn simple_test_package(name: &str) -> Package {
    PackageBuilder::default()
        .name(name)
        .environment(TEST_ENV, |b| b.install("echo 'Installing package'"))
        .build()
}

/// Creates a test package with both install and check commands.
/// Used for testing package checking functionality.
#[must_use]
pub fn test_package_with_check(name: &str) -> Package {
    PackageBuilder::default()
        .name(name)
        .environment(TEST_ENV, |b| {
            b.install("echo 'Installing package'")
                .check_some("echo 'Checking package'")
        })
        .build()
}

/// Creates a test package with install and check commands for a specific environment.
#[must_use]
pub fn test_package_with_check_for_env(name: &str, environment: &str) -> Package {
    PackageBuilder::default()
        .name(name)
        .environment(environment, |b| {
            b.install("echo 'Installing package'")
                .check_some("echo 'Checking package'")
        })
        .build()
}

/// Creates a test package with multiple environments.
/// Useful for testing cross-environment behavior.
#[must_use]
pub fn multi_env_test_package(name: &str) -> Package {
    PackageBuilder::default()
        .name(name)
        .environment(TEST_ENV, |b| {
            b.install("echo 'Installing in test env'")
                .check_some("echo 'Checking in test env'")
        })
        .environment(ALT_TEST_ENV, |b| {
            b.install("echo 'Installing in prod env'")
                .check_some("echo 'Checking in prod env'")
        })
        .build()
}

/// Creates a test package that will fail its check command.
/// Used for testing error handling in check operations.
#[must_use]
pub fn failing_check_package(name: &str) -> Package {
    PackageBuilder::default()
        .name(name)
        .environment(TEST_ENV, |b| {
            b.install("echo 'Installing package'").check_some("exit 1") // This will fail
        })
        .build()
}

/// Creates a test package with a timeout-inducing check command.
/// Used for testing command timeout handling.
#[must_use]
pub fn timeout_check_package(name: &str) -> Package {
    PackageBuilder::default()
        .name(name)
        .environment(TEST_ENV, |b| {
            b.install("echo 'Installing package'")
                .check_some("sleep 10") // This will timeout with default settings
        })
        .build()
}

/// Creates a test package with no check command.
/// Used for testing scenarios where packages don't have check methods.
#[must_use]
pub fn no_check_package(name: &str) -> Package {
    PackageBuilder::default()
        .name(name)
        .environment(TEST_ENV, |b| b.install("echo 'Installing package'"))
        .build()
}
