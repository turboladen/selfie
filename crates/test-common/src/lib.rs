//! Common test utilities shared across all selfie crates.
//!
//! This crate provides standardized test helpers to eliminate code duplication
//! while maintaining test clarity and ergonomics.

pub mod config;
pub mod constants;
pub mod events;
pub mod fixtures;
pub mod package;
pub mod service;

// Re-export the most commonly used items for convenience
pub use config::{
    test_config, test_config_for_env, test_config_verbose, test_config_with_colors,
    test_config_with_dir,
};
pub use constants::*;
pub use events::{
    assert_failed_operation, assert_successful_operation, collect_events, get_operation_result,
};
pub use fixtures::{
    TestPackageBehavior, create_invalid_package_file, create_package_file_with_check,
    create_service_install_test_package_file, create_service_invalid_package_file,
    create_service_test_package_file, create_service_test_package_file_with_behavior,
    create_test_package_file,
};
pub use package::{multi_env_test_package, simple_test_package, test_package_with_check};
pub use service::{
    create_cli_service, create_service_test_service, create_test_service,
    create_test_service_with_config,
};

// Re-export commonly used external dependencies for convenience
pub use selfie::{
    config::AppConfigBuilder,
    package::{Package, PackageBuilder},
};
pub use tempfile::TempDir;
