//! CLI configuration types and loading logic
//!
//! This module defines CLI-specific configuration that wraps the library's
//! `SelfieConfig` with presentation settings like verbosity and color output.
//!
//! # Configuration Precedence
//!
//! The configuration system follows a standard precedence order:
//! 1. Command-line arguments (highest priority)
//! 2. Configuration file settings
//! 3. Default values (lowest priority)

use std::path::PathBuf;

use selfie::config::SelfieConfig;
use serde::Deserialize;

use crate::cli::ClapCli;

/// CLI-specific settings from the `cli:` section of the config file.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct CliSection {
    #[serde(default)]
    pub(crate) verbose: bool,

    #[serde(default = "default_use_colors")]
    pub(crate) use_colors: bool,
}

fn default_use_colors() -> bool {
    true
}

impl Default for CliSection {
    fn default() -> Self {
        Self {
            verbose: false,
            use_colors: true,
        }
    }
}

/// Wrapper for deserializing just the `cli:` section from the config file.
#[derive(Deserialize)]
struct RawCliFile {
    #[serde(default)]
    cli: CliSection,
}

/// Complete CLI configuration: library config + CLI-specific settings.
///
/// All CLI command handlers should accept `&CliConfig`. It delegates
/// core getters to `SelfieConfig` so callers don't need to reach through.
#[derive(Debug, Clone)]
pub(crate) struct CliConfig {
    selfie: SelfieConfig,
    cli: CliSection,
}

impl CliConfig {
    /// Create a new `CliConfig` from its components.
    pub(crate) fn new(selfie: SelfieConfig, cli: CliSection) -> Self {
        Self { selfie, cli }
    }

    /// Get the underlying library config for passing to service calls.
    pub(crate) fn selfie_config(&self) -> &SelfieConfig {
        &self.selfie
    }

    // --- CLI-specific getters ---

    pub(crate) fn verbose(&self) -> bool {
        self.cli.verbose
    }

    pub(crate) fn use_colors(&self) -> bool {
        self.cli.use_colors
    }

    // --- Delegated core getters ---

    pub(crate) fn environment(&self) -> &str {
        self.selfie.environment()
    }

    pub(crate) fn package_directory(&self) -> &PathBuf {
        self.selfie.package_directory()
    }

    pub(crate) fn command_timeout(&self) -> std::time::Duration {
        self.selfie.command_timeout()
    }

    pub(crate) fn stop_on_error(&self) -> bool {
        self.selfie.stop_on_error()
    }

    pub(crate) fn max_parallel_installations(&self) -> std::num::NonZeroUsize {
        self.selfie.max_parallel_installations()
    }
}

#[cfg(test)]
impl CliConfig {
    /// Wrap a `SelfieConfig` with default CLI settings (colors disabled) for testing.
    pub(crate) fn wrap_for_test(selfie: SelfieConfig) -> Self {
        Self::new(
            selfie,
            CliSection {
                verbose: false,
                use_colors: false,
            },
        )
    }

    /// Wrap a `SelfieConfig` with colors enabled for testing color output.
    pub(crate) fn wrap_for_test_with_colors(selfie: SelfieConfig) -> Self {
        Self::new(selfie, CliSection::default())
    }
}

impl ClapCli {
    /// Apply CLI flag overrides to build a `CliConfig`.
    pub(crate) fn build_cli_config(
        &self,
        mut selfie_config: SelfieConfig,
        mut cli_section: CliSection,
    ) -> CliConfig {
        // Override core fields
        if let Some(env) = self.environment.as_ref() {
            selfie_config.environment_mut().clone_from(env);
        }
        if let Some(dir) = self.package_directory.as_ref() {
            selfie_config.package_directory_mut().clone_from(dir);
        }

        // Override CLI-specific fields
        if self.verbose {
            cli_section.verbose = true;
        }
        if self.no_color {
            cli_section.use_colors = false;
        }

        CliConfig::new(selfie_config, cli_section)
    }
}

/// Load the CLI section from the config file.
///
/// Reads the same config file the library uses, but only deserializes
/// the `cli:` section. Unknown keys are ignored.
/// If anything fails, just use defaults - the library already validated the file.
pub(crate) fn load_cli_section(fs: &impl selfie::fs::FileSystem) -> CliSection {
    let Ok(config_dir) = fs.config_dir() else {
        return CliSection::default();
    };

    let yaml_path = config_dir.join("config.yaml");
    let yml_path = config_dir.join("config.yml");

    let config_path = if fs.path_exists(&yaml_path) {
        yaml_path
    } else if fs.path_exists(&yml_path) {
        yml_path
    } else {
        return CliSection::default();
    };

    let Ok(contents) = fs.read_file(&config_path) else {
        return CliSection::default();
    };

    serde_yaml::from_str::<RawCliFile>(&contents)
        .map(|raw| raw.cli)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;
    use selfie::config::SelfieConfigBuilder;

    struct FakeArgs {
        environment: Option<&'static str>,
        package_directory: Option<&'static str>,
        verbose: bool,
        no_color: bool,
    }

    impl FakeArgs {
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
            args.push("config");
            args.push("validate");
            ClapCli::parse_from(args)
        }
    }

    fn default_selfie_config() -> SelfieConfig {
        SelfieConfigBuilder::default()
            .environment("original-env")
            .package_directory("/original/path")
            .build()
    }

    #[test]
    fn test_cli_config_delegates_core_getters() {
        let config = CliConfig::new(default_selfie_config(), CliSection::default());
        assert_eq!(config.environment(), "original-env");
        assert_eq!(config.package_directory(), &PathBuf::from("/original/path"));
        assert!(!config.verbose());
        assert!(config.use_colors());
    }

    #[test]
    fn test_build_cli_config_environment_override() {
        let args = FakeArgs {
            environment: Some("cli-env"),
            package_directory: None,
            verbose: false,
            no_color: false,
        }
        .into_cli();

        let config = args.build_cli_config(default_selfie_config(), CliSection::default());
        assert_eq!(config.environment(), "cli-env");
        assert_eq!(config.package_directory(), &PathBuf::from("/original/path"));
        assert!(!config.verbose());
        assert!(config.use_colors());
    }

    #[test]
    fn test_build_cli_config_package_dir_override() {
        let args = FakeArgs {
            environment: None,
            package_directory: Some("/cli/path"),
            verbose: false,
            no_color: false,
        }
        .into_cli();

        let config = args.build_cli_config(default_selfie_config(), CliSection::default());
        assert_eq!(config.environment(), "original-env");
        assert_eq!(config.package_directory(), &PathBuf::from("/cli/path"));
    }

    #[test]
    fn test_build_cli_config_ui_settings() {
        let args = FakeArgs {
            environment: None,
            package_directory: None,
            verbose: true,
            no_color: true,
        }
        .into_cli();

        let config = args.build_cli_config(default_selfie_config(), CliSection::default());
        assert!(config.verbose());
        assert!(!config.use_colors());
    }

    #[test]
    fn test_build_cli_config_multiple_overrides() {
        let args = FakeArgs {
            environment: Some("cli-env"),
            package_directory: Some("/cli/path"),
            verbose: true,
            no_color: true,
        }
        .into_cli();

        let config = args.build_cli_config(default_selfie_config(), CliSection::default());
        assert_eq!(config.environment(), "cli-env");
        assert_eq!(config.package_directory(), &PathBuf::from("/cli/path"));
        assert!(config.verbose());
        assert!(!config.use_colors());
    }

    #[test]
    fn test_build_cli_config_no_overrides() {
        let args = FakeArgs {
            environment: None,
            package_directory: None,
            verbose: false,
            no_color: false,
        }
        .into_cli();

        let config = args.build_cli_config(default_selfie_config(), CliSection::default());
        assert_eq!(config.environment(), "original-env");
        assert_eq!(config.package_directory(), &PathBuf::from("/original/path"));
        assert!(!config.verbose());
        assert!(config.use_colors());
    }

    #[test]
    fn test_build_cli_config_preserves_execution_settings() {
        let selfie_config = SelfieConfigBuilder::default()
            .environment("original-env")
            .package_directory("/original/path")
            .command_timeout_unchecked(120)
            .stop_on_error(false)
            .max_parallel_unchecked(8)
            .build();

        let args = FakeArgs {
            environment: Some("cli-env"),
            package_directory: None,
            verbose: true,
            no_color: false,
        }
        .into_cli();

        let config = args.build_cli_config(selfie_config, CliSection::default());
        assert_eq!(config.command_timeout().as_secs(), 120);
        assert!(!config.stop_on_error());
        assert_eq!(config.max_parallel_installations().get(), 8);
    }

    #[test]
    fn test_cli_section_deserialization() {
        let yaml = r#"
            cli:
              verbose: true
              use_colors: false
        "#;
        let raw: RawCliFile = serde_yaml::from_str(yaml).unwrap();
        assert!(raw.cli.verbose);
        assert!(!raw.cli.use_colors);
    }

    #[test]
    fn test_cli_section_deserialization_defaults() {
        let yaml = "environment: test\n";
        let raw: RawCliFile = serde_yaml::from_str(yaml).unwrap();
        assert!(!raw.cli.verbose);
        assert!(raw.cli.use_colors);
    }

    #[test]
    fn test_selfie_config_accessor() {
        let selfie = default_selfie_config();
        let config = CliConfig::new(selfie.clone(), CliSection::default());
        assert_eq!(config.selfie_config().environment(), selfie.environment());
    }
}
