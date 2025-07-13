pub mod loader;
pub mod validate;
pub mod yaml;

pub use self::yaml::YamlLoader;

use std::{
    num::{NonZeroU64, NonZeroUsize},
    path::PathBuf,
    time::Duration,
};

use serde::Deserialize;

const VERBOSE_DEFAULT: bool = false;
const USE_COLORS_DEFAULT: bool = true;
const STOP_ON_ERROR_DEFAULT: bool = true;

/// Comprehensive application configuration that combines file config and CLI args
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppConfig {
    // Core settings
    pub(crate) environment: String,
    pub(crate) package_directory: PathBuf,

    // UI settings
    #[serde(default)]
    pub(crate) verbose: bool,

    #[serde(default = "default_use_colors")]
    pub(crate) use_colors: bool,

    // Execution settings
    #[serde(default = "default_command_timeout")]
    pub(crate) command_timeout: NonZeroU64,

    #[serde(default = "default_stop_on_error")]
    pub(crate) stop_on_error: bool,

    #[serde(default = "default_max_parallel")]
    pub(crate) max_parallel_installations: NonZeroUsize,
}

/// # Panics
///
/// This function uses `unsafe` code but cannot panic because it is called with
/// the constant value 60, which is guaranteed to be non-zero.
fn default_command_timeout() -> NonZeroU64 {
    unsafe { NonZeroU64::new_unchecked(60) }
}

fn default_stop_on_error() -> bool {
    true
}

/// # Panics
///
/// This function uses `unsafe` code in the fallback case but cannot panic because
/// it is called with the constant value 4, which is guaranteed to be non-zero.
/// The `num_cpus::get()` call may return 0 on some systems, which is handled
/// safely by the `unwrap_or_else` fallback.
fn default_max_parallel() -> NonZeroUsize {
    NonZeroUsize::new(num_cpus::get()).unwrap_or_else(|| unsafe { NonZeroUsize::new_unchecked(4) })
}

fn default_use_colors() -> bool {
    true
}

impl AppConfig {
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

    /// Check if verbose logging is enabled
    #[must_use]
    pub fn verbose(&self) -> bool {
        self.verbose
    }

    /// Check if colored output is enabled
    #[must_use]
    pub fn use_colors(&self) -> bool {
        self.use_colors
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

    /// Get a mutable reference to the verbose flag
    pub fn verbose_mut(&mut self) -> &mut bool {
        &mut self.verbose
    }

    /// Get a mutable reference to the `use_colors` flag
    pub fn use_colors_mut(&mut self) -> &mut bool {
        &mut self.use_colors
    }
}

/// Builder pattern for `AppConfig` testing
///
/// Provides a convenient way to construct `AppConfig` instances for testing
/// with default values that can be selectively overridden.
#[derive(Default, Debug)]
pub struct AppConfigBuilder {
    environment: String,
    package_directory: PathBuf,
    verbose: Option<bool>,
    use_colors: Option<bool>,
    command_timeout: Option<NonZeroU64>,
    max_parallel: Option<NonZeroUsize>,
    stop_on_error: Option<bool>,
}

impl AppConfigBuilder {
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

    #[must_use]
    pub fn verbose(mut self, verbose: bool) -> Self {
        self.verbose = Some(verbose);
        self
    }

    #[must_use]
    pub fn use_colors(mut self, use_colors: bool) -> Self {
        self.use_colors = Some(use_colors);
        self
    }

    /// # Panics
    ///
    /// This panics if `timeout` is non-zero.
    #[must_use]
    pub fn command_timeout_unchecked(mut self, timeout: u64) -> Self {
        self.command_timeout = Some(NonZeroU64::new(timeout).unwrap());
        self
    }

    /// # Panics
    ///
    /// This panics if `max` is non-zero.
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
    pub fn build(self) -> AppConfig {
        AppConfig {
            environment: self.environment,
            package_directory: self.package_directory,
            verbose: self.verbose.unwrap_or(VERBOSE_DEFAULT),
            use_colors: self.use_colors.unwrap_or(USE_COLORS_DEFAULT),
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
    fn test_app_config_builder() {
        let config = AppConfigBuilder::default()
            .environment("test-env")
            .package_directory("/test/path")
            .verbose(true)
            .use_colors(false)
            .command_timeout_unchecked(120)
            .max_parallel_unchecked(8)
            .build();

        assert_eq!(config.environment, "test-env");
        assert_eq!(config.package_directory, PathBuf::from("/test/path"));
        assert!(config.verbose);
        assert!(!config.use_colors);
        assert_eq!(config.command_timeout(), Duration::from_secs(120));
        assert_eq!(
            config.max_parallel_installations,
            NonZeroUsize::new(8).unwrap()
        );
    }

    #[test]
    fn test_accessor_methods() {
        let config = AppConfigBuilder::default()
            .environment("test-env")
            .package_directory("/test/path")
            .verbose(true)
            .use_colors(false)
            .command_timeout_unchecked(120)
            .max_parallel_unchecked(8)
            .stop_on_error(false)
            .build();

        // Test read accessors
        assert_eq!(config.environment(), "test-env");
        assert_eq!(config.package_directory(), &PathBuf::from("/test/path"));
        assert!(config.verbose());
        assert!(!config.use_colors());
        assert_eq!(config.command_timeout(), Duration::from_secs(120));
        assert_eq!(config.max_parallel_installations().get(), 8);
        assert!(!config.stop_on_error());
    }

    #[test]
    fn test_mutable_accessors() {
        let mut config = AppConfigBuilder::default()
            .environment("old-env")
            .package_directory("/old/path")
            .verbose(false)
            .use_colors(true)
            .build();

        // Modify through mutable accessors
        *config.environment_mut() = "new-env".to_string();
        *config.package_directory_mut() = PathBuf::from("/new/path");
        *config.verbose_mut() = true;
        *config.use_colors_mut() = false;

        // Verify changes
        assert_eq!(config.environment(), "new-env");
        assert_eq!(config.package_directory(), &PathBuf::from("/new/path"));
        assert!(config.verbose());
        assert!(!config.use_colors());
    }

    #[test]
    fn test_default_values() {
        // Create config with minimal explicit values
        let config = AppConfigBuilder::default()
            .environment("test-env")
            .package_directory("/test/path")
            .build();

        // Verify default values
        assert_eq!(config.environment(), "test-env");
        assert_eq!(config.package_directory(), &PathBuf::from("/test/path"));
        assert_eq!(config.verbose(), VERBOSE_DEFAULT);
        assert_eq!(config.use_colors(), USE_COLORS_DEFAULT);
        assert_eq!(config.command_timeout().as_secs(), 60);
        assert!(config.max_parallel_installations().get() > 0); // Should be based on CPUs or default
        assert_eq!(config.stop_on_error(), STOP_ON_ERROR_DEFAULT);
    }

    #[test]
    fn test_command_timeout_conversion() {
        let timeout_secs = 180u64;
        let config = AppConfigBuilder::default()
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
            verbose: true
            use_colors: false
            command_timeout: 90
            stop_on_error: false
            max_parallel_installations: 4
        "#;

        let config: AppConfig = serde_yaml::from_str(yaml).unwrap();

        assert_eq!(config.environment, "prod");
        assert_eq!(config.package_directory, PathBuf::from("/opt/packages"));
        assert!(config.verbose);
        assert!(!config.use_colors);
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

        let config: AppConfig = serde_yaml::from_str(yaml).unwrap();

        // Explicit values
        assert_eq!(config.environment, "dev");
        assert_eq!(config.package_directory, PathBuf::from("/dev/packages"));

        // Default values
        assert!(!config.verbose); // Default
        assert!(config.use_colors); // Default from function
        assert_eq!(config.command_timeout.get(), 60); // Default
        assert!(config.max_parallel_installations.get() > 0); // Default based on CPUs
        assert!(config.stop_on_error); // Default
    }

    #[test]
    fn test_deny_unknown_fields() {
        // YAML string with an unknown field `unknown_field`
        let yaml = r#"
        environment: "prod"
        package_directory: "/opt/packages"
        unknown_field: "this should cause an error"
    "#;

        // Attempt to deserialize and expect an error
        let result: Result<AppConfig, _> = serde_yaml::from_str(yaml);
        assert!(result.is_err());
    }
}
