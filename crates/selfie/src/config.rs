pub mod loader;
pub mod validate;
pub mod yaml;

pub use self::loader::ConfigLoadError;
pub use self::yaml::YamlLoader;

use std::{
    num::{NonZeroU64, NonZeroUsize},
    path::PathBuf,
    time::Duration,
};

use serde::Deserialize;

const STOP_ON_ERROR_DEFAULT: bool = true;

/// Comprehensive application configuration that combines file config and CLI args
#[derive(Debug, Clone, Deserialize)]
pub struct SelfieConfig {
    // Core settings
    pub(crate) environment: String,
    pub(crate) package_directory: PathBuf,

    // Execution settings
    #[serde(default = "default_command_timeout")]
    pub(crate) command_timeout: NonZeroU64,

    #[serde(default = "default_stop_on_error")]
    pub(crate) stop_on_error: bool,

    #[serde(default = "default_max_parallel")]
    pub(crate) max_parallel_installations: NonZeroUsize,
}

/// Returns the default command timeout of 60 seconds.
fn default_command_timeout() -> NonZeroU64 {
    const { NonZeroU64::new(60).unwrap() }
}

fn default_stop_on_error() -> bool {
    true
}

/// Returns the default max parallel installations, using the CPU count or falling back to 4.
fn default_max_parallel() -> NonZeroUsize {
    NonZeroUsize::new(num_cpus::get()).unwrap_or(const { NonZeroUsize::new(4).unwrap() })
}

impl SelfieConfig {
    /// Get the current environment name
    #[must_use]
    pub fn environment(&self) -> &str {
        &self.environment
    }

    /// Get the package directory path
    #[must_use]
    pub fn package_directory(&self) -> &PathBuf {
        &self.package_directory
    }

    /// Get the command execution timeout duration
    #[must_use]
    pub fn command_timeout(&self) -> Duration {
        Duration::from_secs(self.command_timeout.into())
    }

    /// Get the maximum number of parallel installations allowed
    #[must_use]
    pub fn max_parallel_installations(&self) -> NonZeroUsize {
        self.max_parallel_installations
    }

    /// Check if operations should stop on first error
    #[must_use]
    pub fn stop_on_error(&self) -> bool {
        self.stop_on_error
    }

    /// Get a mutable reference to the environment name
    pub fn environment_mut(&mut self) -> &mut String {
        &mut self.environment
    }

    /// Get a mutable reference to the package directory path
    pub fn package_directory_mut(&mut self) -> &mut PathBuf {
        &mut self.package_directory
    }
}

/// Builder pattern for `SelfieConfig` testing
///
/// Provides a convenient way to construct `SelfieConfig` instances for testing
/// with default values that can be selectively overridden.
#[derive(Default, Debug)]
pub struct SelfieConfigBuilder {
    environment: String,
    package_directory: PathBuf,
    command_timeout: Option<NonZeroU64>,
    max_parallel: Option<NonZeroUsize>,
    stop_on_error: Option<bool>,
}

impl SelfieConfigBuilder {
    #[must_use]
    pub fn environment(mut self, environment: &str) -> Self {
        self.environment = environment.to_string();
        self
    }

    #[must_use]
    pub fn package_directory<D>(mut self, package_directory: D) -> Self
    where
        D: AsRef<std::ffi::OsStr>,
    {
        self.package_directory = PathBuf::from(package_directory.as_ref());
        self
    }

    /// # Panics
    ///
    /// This panics if `timeout` is zero.
    #[must_use]
    pub fn command_timeout_unchecked(mut self, timeout: u64) -> Self {
        self.command_timeout = Some(NonZeroU64::new(timeout).unwrap());
        self
    }

    /// # Panics
    ///
    /// This panics if `max` is zero.
    #[must_use]
    pub fn max_parallel_unchecked(mut self, max: usize) -> Self {
        self.max_parallel = Some(NonZeroUsize::new(max).unwrap());
        self
    }

    #[must_use]
    pub fn stop_on_error(mut self, stop: bool) -> Self {
        self.stop_on_error = Some(stop);
        self
    }

    #[must_use]
    pub fn build(self) -> SelfieConfig {
        SelfieConfig {
            environment: self.environment,
            package_directory: self.package_directory,
            command_timeout: self.command_timeout.unwrap_or(default_command_timeout()),
            max_parallel_installations: self.max_parallel.unwrap_or(default_max_parallel()),
            stop_on_error: self.stop_on_error.unwrap_or(STOP_ON_ERROR_DEFAULT),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn test_selfie_config_builder() {
        let config = SelfieConfigBuilder::default()
            .environment("test-env")
            .package_directory("/test/path")
            .command_timeout_unchecked(120)
            .max_parallel_unchecked(8)
            .build();

        assert_eq!(config.environment, "test-env");
        assert_eq!(config.package_directory, PathBuf::from("/test/path"));
        assert_eq!(config.command_timeout(), Duration::from_secs(120));
        assert_eq!(
            config.max_parallel_installations,
            NonZeroUsize::new(8).unwrap()
        );
    }

    #[test]
    fn test_accessor_methods() {
        let config = SelfieConfigBuilder::default()
            .environment("test-env")
            .package_directory("/test/path")
            .command_timeout_unchecked(120)
            .max_parallel_unchecked(8)
            .stop_on_error(false)
            .build();

        // Test read accessors
        assert_eq!(config.environment(), "test-env");
        assert_eq!(config.package_directory(), &PathBuf::from("/test/path"));
        assert_eq!(config.command_timeout(), Duration::from_secs(120));
        assert_eq!(config.max_parallel_installations().get(), 8);
        assert!(!config.stop_on_error());
    }

    #[test]
    fn test_mutable_accessors() {
        let mut config = SelfieConfigBuilder::default()
            .environment("old-env")
            .package_directory("/old/path")
            .build();

        // Modify through mutable accessors
        *config.environment_mut() = "new-env".to_string();
        *config.package_directory_mut() = PathBuf::from("/new/path");

        // Verify changes
        assert_eq!(config.environment(), "new-env");
        assert_eq!(config.package_directory(), &PathBuf::from("/new/path"));
    }

    #[test]
    fn test_default_values() {
        // Create config with minimal explicit values
        let config = SelfieConfigBuilder::default()
            .environment("test-env")
            .package_directory("/test/path")
            .build();

        // Verify default values
        assert_eq!(config.environment(), "test-env");
        assert_eq!(config.package_directory(), &PathBuf::from("/test/path"));
        assert_eq!(config.command_timeout().as_secs(), 60);
        assert!(config.max_parallel_installations().get() > 0); // Should be based on CPUs or default
        assert_eq!(config.stop_on_error(), STOP_ON_ERROR_DEFAULT);
    }

    #[test]
    fn test_command_timeout_conversion() {
        let timeout_secs = 180u64;
        let config = SelfieConfigBuilder::default()
            .environment("test")
            .package_directory("/test")
            .command_timeout_unchecked(timeout_secs)
            .build();

        let duration = config.command_timeout();
        assert_eq!(duration, Duration::from_secs(timeout_secs));
    }

    #[test]
    fn test_serde_deserialization() {
        // Test deserialization from YAML string
        let yaml = r#"
            environment: "prod"
            package_directory: "/opt/packages"
            command_timeout: 90
            stop_on_error: false
            max_parallel_installations: 4
        "#;

        let config: SelfieConfig = serde_yaml::from_str(yaml).unwrap();

        assert_eq!(config.environment, "prod");
        assert_eq!(config.package_directory, PathBuf::from("/opt/packages"));
        assert_eq!(config.command_timeout.get(), 90);
        assert_eq!(config.max_parallel_installations.get(), 4);
        assert!(!config.stop_on_error);
    }

    #[test]
    fn test_serde_partial_deserialization() {
        // Test deserialization with only required fields
        let yaml = r#"
            environment: "dev"
            package_directory: "/dev/packages"
        "#;

        let config: SelfieConfig = serde_yaml::from_str(yaml).unwrap();

        // Explicit values
        assert_eq!(config.environment, "dev");
        assert_eq!(config.package_directory, PathBuf::from("/dev/packages"));

        // Default values
        assert_eq!(config.command_timeout.get(), 60); // Default
        assert!(config.max_parallel_installations.get() > 0); // Default based on CPUs
        assert!(config.stop_on_error); // Default
    }

    #[test]
    fn test_unknown_fields_are_allowed() {
        // YAML string with an unknown field `unknown_field`
        let yaml = r#"
        environment: "prod"
        package_directory: "/opt/packages"
        unknown_field: "this should be ignored"
    "#;

        // Deserialization should succeed since we no longer deny unknown fields
        let result: Result<SelfieConfig, _> = serde_yaml::from_str(yaml);
        assert!(result.is_ok());
        let config = result.unwrap();
        assert_eq!(config.environment, "prod");
        assert_eq!(config.package_directory, PathBuf::from("/opt/packages"));
    }
}
