//! CLI configuration integration and command-line argument processing
//!
//! This module provides the integration between command-line arguments and the
//! application configuration system. It implements the `ApplyToConfig` trait to
//! allow CLI arguments to override configuration file settings.
//!
//! # Configuration Precedence
//!
//! The configuration system follows a standard precedence order:
//! 1. Command-line arguments (highest priority)
//! 2. Configuration file settings
//! 3. Default values (lowest priority)
//!
//! # Examples
//!
//! ```bash
//! # Override environment from config file
//! selfie --environment=production package install node
//!
//! # Override package directory and enable verbose output
//! selfie --package-directory=/custom/path --verbose package list
//! ```

use selfie::config::{AppConfig, loader::ApplyToConfig};

use crate::cli::ClapCli;

impl ApplyToConfig for ClapCli {
    /// Apply command-line arguments to the base configuration
    ///
    /// Takes a base configuration (typically loaded from a file) and applies
    /// any CLI arguments that were provided at runtime. CLI arguments override
    /// corresponding values in the base configuration.
    ///
    /// # Arguments
    ///
    /// * `config` - The base configuration to modify
    ///
    /// # Returns
    ///
    /// A new [`AppConfig`] with CLI arguments applied on top of the base configuration
    ///
    /// # Configuration Overrides
    ///
    /// The following CLI arguments can override configuration file settings:
    /// - `--environment`: Overrides the target environment
    /// - `--package-directory`: Overrides the package directory path
    /// - `--verbose`: Overrides the verbosity setting
    /// - `--no-color`: Disables colored output (overrides `use_colors` setting)
    fn apply_to_config(&self, mut config: AppConfig) -> AppConfig {
        // Override environment if specified via CLI
        if let Some(env) = self.environment.as_ref() {
            config.environment_mut().clone_from(env);
        }

        // Override package directory if specified via CLI
        if let Some(dir) = self.package_directory.as_ref() {
            config.package_directory_mut().clone_from(dir);
        }

        // Apply UI settings from CLI arguments
        // Note: verbose flag directly sets the value
        *config.verbose_mut() = self.verbose;
        // Note: no_color flag inverts the use_colors setting
        *config.use_colors_mut() = !self.no_color;

        config
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;
    use selfie::config::AppConfigBuilder;
    use std::path::PathBuf;

    /// Helper structure to create CLI arguments for testing
    ///
    /// This provides a convenient way to construct CLI arguments without
    /// having to deal with the complexity of clap's argument parsing in tests.
    struct FakeArgs {
        environment: Option<&'static str>,
        package_directory: Option<&'static str>,
        verbose: bool,
        no_color: bool,
    }

    impl FakeArgs {
        /// Convert test arguments into a parsed CLI structure
        ///
        /// Constructs the command-line arguments as if they were passed to the
        /// real CLI and parses them using clap. This ensures our tests exercise
        /// the same parsing logic as the real application.
        fn into_cli(self) -> ClapCli {
            let mut args = vec!["selfie"];

            if let Some(env) = self.environment {
                args.push("--environment");
                args.push(env);
            }

            if let Some(dir) = self.package_directory {
                args.push("--package-directory");
                args.push(dir);
            }

            if self.verbose {
                args.push("--verbose");
            }

            if self.no_color {
                args.push("--no-color");
            }

            // Add a required subcommand
            args.push("config");
            args.push("validate");

            ClapCli::parse_from(args)
        }
    }

    #[test]
    fn test_apply_cli_args_environment_override() {
        let config = AppConfigBuilder::default()
            .environment("original-env")
            .package_directory("/original/path")
            .build();

        let args = FakeArgs {
            environment: Some("cli-env"),
            package_directory: None,
            verbose: false,
            no_color: false,
        }
        .into_cli();

        let updated = args.apply_to_config(config);

        assert_eq!(updated.environment(), "cli-env");
        assert_eq!(
            updated.package_directory(),
            &PathBuf::from("/original/path")
        );
        assert!(!updated.verbose());
        assert!(updated.use_colors());
    }

    #[test]
    fn test_apply_cli_args_package_dir_override() {
        let config = AppConfigBuilder::default()
            .environment("original-env")
            .package_directory("/original/path")
            .build();

        let args = FakeArgs {
            environment: None,
            package_directory: Some("/cli/path"),
            verbose: false,
            no_color: false,
        }
        .into_cli();

        let updated = args.apply_to_config(config);

        assert_eq!(updated.environment(), "original-env");
        assert_eq!(updated.package_directory(), &PathBuf::from("/cli/path"));
        assert!(!updated.verbose());
        assert!(updated.use_colors());
    }

    #[test]
    fn test_apply_cli_args_ui_settings() {
        let config = AppConfigBuilder::default()
            .environment("original-env")
            .package_directory("/original/path")
            .verbose(false)
            .use_colors(true)
            .build();

        let args = FakeArgs {
            environment: None,
            package_directory: None,
            verbose: true,
            no_color: true,
        }
        .into_cli();

        let updated = args.apply_to_config(config);

        assert_eq!(updated.environment(), "original-env");
        assert_eq!(
            updated.package_directory(),
            &PathBuf::from("/original/path")
        );
        assert!(updated.verbose());
        assert!(!updated.use_colors());
    }

    #[test]
    fn test_apply_cli_args_multiple_overrides() {
        let config = AppConfigBuilder::default()
            .environment("original-env")
            .package_directory("/original/path")
            .verbose(false)
            .use_colors(true)
            .build();

        let args = FakeArgs {
            environment: Some("cli-env"),
            package_directory: Some("/cli/path"),
            verbose: true,
            no_color: true,
        }
        .into_cli();

        let updated = args.apply_to_config(config);

        assert_eq!(updated.environment(), "cli-env");
        assert_eq!(updated.package_directory(), &PathBuf::from("/cli/path"));
        assert!(updated.verbose());
        assert!(!updated.use_colors());
    }

    #[test]
    fn test_apply_cli_args_no_overrides() {
        let config = AppConfigBuilder::default()
            .environment("original-env")
            .package_directory("/original/path")
            .verbose(false)
            .use_colors(true)
            .build();

        let args = FakeArgs {
            environment: None,
            package_directory: None,
            verbose: false,
            no_color: false,
        }
        .into_cli();

        let updated = args.apply_to_config(config);

        // Config should remain unchanged
        assert_eq!(updated.environment(), "original-env");
        assert_eq!(
            updated.package_directory(),
            &PathBuf::from("/original/path")
        );
        assert!(!updated.verbose());
        assert!(updated.use_colors());
    }

    #[test]
    fn test_apply_cli_args_preserves_other_settings() {
        // Create config with non-default values for settings not affected by CLI
        let config = AppConfigBuilder::default()
            .environment("original-env")
            .package_directory("/original/path")
            .command_timeout_unchecked(120)
            .stop_on_error(false)
            .max_parallel_unchecked(8)
            .use_colors(true)
            .build();

        let args = FakeArgs {
            environment: Some("cli-env"),
            package_directory: Some("/cli/path"),
            verbose: true,
            no_color: true,
        }
        .into_cli();

        let updated = args.apply_to_config(config);

        // CLI-modifiable settings should be updated
        assert_eq!(updated.environment(), "cli-env");
        assert_eq!(updated.package_directory(), &PathBuf::from("/cli/path"));
        assert!(updated.verbose());
        assert!(!updated.use_colors());

        // Other settings should be preserved
        assert_eq!(updated.command_timeout().as_secs(), 120);
        assert!(!updated.stop_on_error());
        assert_eq!(updated.max_parallel_installations().get(), 8);
    }
}
