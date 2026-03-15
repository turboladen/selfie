use std::path::PathBuf;

use thiserror::Error;

use crate::{config::SelfieConfig, fs::filesystem::FileSystemError};

/// Port for loading configuration from disk
///
/// This trait abstracts configuration loading to allow for different implementations
/// (e.g., YAML files, TOML files, environment variables) and to enable mocking in tests.
#[cfg_attr(any(test, feature = "with_mocks"), mockall::automock)]
pub trait ConfigLoader: Send + Sync {
    /// Load configuration from standard locations
    ///
    /// Searches for configuration files in standard locations and loads the first one found.
    /// The specific search locations depend on the implementation.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigLoadError`] if:
    /// - No configuration file is found in standard locations
    /// - Multiple configuration files are found (ambiguous)
    /// - File system access fails
    /// - Configuration file content is invalid
    fn load_config(&self) -> Result<SelfieConfig, ConfigLoadError>;

    /// Find possible configuration file paths
    ///
    /// Returns a list of paths where configuration files might be located,
    /// typically including user config directory, current directory, etc.
    ///
    /// # Errors
    ///
    /// Returns an error with the searched path if no valid configuration
    /// directories can be determined.
    fn find_config_file_paths(&self) -> Result<Vec<std::path::PathBuf>, PathBuf>;
}

/// Errors that can occur during configuration loading
#[derive(Error, Debug)]
pub enum ConfigLoadError {
    /// File system operation failed
    #[error(transparent)]
    FileSystemError(#[from] FileSystemError),

    /// No configuration file found in any of the searched locations
    #[error("No configuration file found in locations: {searched}")]
    NotFound { searched: PathBuf },

    /// Multiple configuration files found, creating ambiguity
    #[error("Multiple configuration files found: {}", .0.join(", "))]
    MultipleFound(Vec<String>),

    /// Configuration file content is invalid or malformed
    #[error(transparent)]
    ConfigError(#[from] ::config::ConfigError),
}
