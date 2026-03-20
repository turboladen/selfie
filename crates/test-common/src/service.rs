//! Service creation helpers to eliminate service setup duplication in tests.

use crate::config::{
    service_test_config_with_dir, test_config_with_dir, test_config_with_dir_and_env,
};
use selfie::{
    commands::shell::ShellCommandRunner,
    config::SelfieConfig,
    fs::real::RealFileSystem,
    package::{
        git_adapter::GixGitStatusProvider, repository::YamlPackageRepository,
        service::PackageServiceImpl,
    },
};
use std::time::Duration;
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;

/// Creates a test service with real filesystem and default settings.
/// This is the most commonly used service setup for integration tests.
#[must_use]
pub fn create_test_service(
    temp_dir: &TempDir,
) -> PackageServiceImpl<YamlPackageRepository<RealFileSystem>, ShellCommandRunner> {
    let config = test_config_with_dir(temp_dir.path());
    create_test_service_with_config(config)
}

/// Creates a test service with a specific configuration.
/// Useful when you need custom config settings like different environments.
#[must_use]
pub fn create_test_service_with_config(
    config: SelfieConfig,
) -> PackageServiceImpl<YamlPackageRepository<RealFileSystem>, ShellCommandRunner> {
    let fs = RealFileSystem;
    let repo = YamlPackageRepository::new(fs, config.package_directory().clone());
    let runner =
        ShellCommandRunner::new(ShellCommandRunner::default_shell(), Duration::from_secs(30));
    PackageServiceImpl::new(
        repo,
        runner,
        GixGitStatusProvider,
        config,
        CancellationToken::new(),
    )
}

/// Creates a test service with custom command timeout.
/// Useful for testing timeout scenarios or when you need longer-running commands.
#[must_use]
pub fn create_test_service_with_timeout(
    temp_dir: &TempDir,
    timeout: Duration,
) -> PackageServiceImpl<YamlPackageRepository<RealFileSystem>, ShellCommandRunner> {
    let config = test_config_with_dir(temp_dir.path());
    let fs = RealFileSystem;
    let repo = YamlPackageRepository::new(fs, config.package_directory().clone());
    let runner = ShellCommandRunner::new(ShellCommandRunner::default_shell(), timeout);
    PackageServiceImpl::new(
        repo,
        runner,
        GixGitStatusProvider,
        config,
        CancellationToken::new(),
    )
}

/// Creates a test service for a specific environment.
/// Useful for testing environment-specific behavior.
#[must_use]
pub fn create_test_service_for_env(
    temp_dir: &TempDir,
    environment: &str,
) -> PackageServiceImpl<YamlPackageRepository<RealFileSystem>, ShellCommandRunner> {
    let config = test_config_with_dir_and_env(temp_dir.path(), environment);
    create_test_service_with_config(config)
}

/// Creates the standard CLI service setup used in command handlers.
/// This matches the exact pattern used in CLI commands for consistency.
#[must_use]
pub fn create_cli_service(
    config: &SelfieConfig,
) -> PackageServiceImpl<YamlPackageRepository<RealFileSystem>, ShellCommandRunner> {
    let repo = YamlPackageRepository::new(RealFileSystem, config.package_directory().clone());
    let command_runner = ShellCommandRunner::new(
        ShellCommandRunner::default_shell(),
        config.command_timeout(),
    );
    PackageServiceImpl::new(
        repo,
        command_runner,
        GixGitStatusProvider,
        config.clone(),
        CancellationToken::new(),
    )
}

/// Creates a test service specifically for service layer integration tests.
/// Uses the correct "test" environment expected by service tests.
#[must_use]
pub fn create_service_test_service(
    temp_dir: &TempDir,
) -> PackageServiceImpl<YamlPackageRepository<RealFileSystem>, ShellCommandRunner> {
    let config = service_test_config_with_dir(temp_dir.path());
    create_test_service_with_config(config)
}
