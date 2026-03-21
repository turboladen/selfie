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
    test_config, test_config_for_env, test_config_with_dir, test_config_with_dir_and_env,
};
pub use constants::*;
pub use events::{
    assert_failed_operation, assert_successful_operation, collect_events,
    create_test_operation_info, get_operation_result,
};
pub use fixtures::{
    TestPackageBehavior, create_circular_dependency, create_dependency_chain,
    create_invalid_package_file, create_package_file_with_check,
    create_service_install_test_package_file, create_service_install_test_package_file_with_note,
    create_service_invalid_package_file, create_service_test_package_file,
    create_service_test_package_file_with_behavior, create_service_test_package_file_with_deps,
    create_test_package_file,
};
pub use package::{multi_env_test_package, simple_test_package, test_package_with_check};
pub use service::{
    create_cli_service, create_service_test_service, create_test_service,
    create_test_service_for_env, create_test_service_with_config,
};

// Re-export commonly used external dependencies for convenience
pub use selfie::{
    config::SelfieConfigBuilder,
    package::{Package, PackageBuilder},
};
pub use tempfile::TempDir;
