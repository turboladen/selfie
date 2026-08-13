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

use selfie::config::{IgnoredKey, LoadedConfig, SelfieConfig};
use serde::Deserialize;

use crate::{cli::ClapCli, display_manager::DisplayManager};

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

/// Something in the configuration file selfie read and did not use.
///
/// Rendered once, before the command runs. Carries both halves so the CLI
/// renders library diagnostics and its own the same way.
#[derive(Debug)]
pub(crate) struct ConfigNotice {
    message: String,
    suggestion: String,
}

impl ConfigNotice {
    fn from_ignored_key(ignored: &IgnoredKey) -> Self {
        Self {
            message: ignored.message(),
            suggestion: ignored.suggestion(),
        }
    }

    /// A key inside `cli:` that the CLI does not read.
    ///
    /// Its own wording rather than the library's: the library cannot know what
    /// this section accepts, and each frontend reports only its own keys.
    fn unknown_cli_key(key: &str) -> Self {
        Self {
            message: format!("`cli.{key}` is not a recognized CLI setting. Selfie ignored it."),
            suggestion: format!(
                "Remove `{key}` from the `cli:` section, or check it against the configuration guide."
            ),
        }
    }

    /// A `cli:` section that is present but cannot be read.
    ///
    /// Not fatal: the library loaded the file, so the run is usable with default
    /// CLI settings.
    fn unreadable_cli_section(error: &impl std::fmt::Display) -> Self {
        Self {
            message: format!(
                "The `cli:` section could not be read ({error}). Selfie used the default CLI settings."
            ),
            suggestion:
                "Make `cli:` a mapping, for example `cli:` followed by an indented `verbose: true`."
                    .to_string(),
        }
    }
}

/// The `cli:` section, plus anything in the file the CLI did not use.
#[derive(Debug, Default)]
pub(crate) struct CliSectionLoad {
    pub(crate) section: CliSection,
    pub(crate) notices: Vec<ConfigNotice>,
}

/// Render the library's ignored keys as notices this crate can print.
pub(crate) fn library_config_notices(ignored: &[IgnoredKey]) -> Vec<ConfigNotice> {
    ignored.iter().map(ConfigNotice::from_ignored_key).collect()
}

/// Print every notice once, before the command runs.
// One warning, so the whole notice goes to stderr. `print_suggestion` writes to
// stdout, which would put it in redirected command output.
pub(crate) fn report_config_notices(notices: &[ConfigNotice], display: &DisplayManager) {
    for notice in notices {
        display.print_warning(format!("{} {}", notice.message, notice.suggestion));
    }
}

/// The `cli:` section, taken from the parse the library already did.
///
/// Returns the defaults and a notice when the section is present but is not a
/// mapping.
pub(crate) fn cli_section(loaded: &LoadedConfig) -> CliSectionLoad {
    match loaded.frontend_section::<CliSection>("cli") {
        Ok(Some(section)) => CliSectionLoad {
            notices: section
                .ignored_keys()
                .iter()
                .map(|ignored| ConfigNotice::unknown_cli_key(ignored.key()))
                .collect(),
            section: section.value(),
        },
        Ok(None) => CliSectionLoad::default(),
        Err(error) => CliSectionLoad {
            section: CliSection::default(),
            notices: vec![ConfigNotice::unreadable_cli_section(&error)],
        },
    }
}

/// Complete CLI configuration: library config + CLI-specific settings.
///
/// All CLI command handlers should accept `&CliConfig`. It delegates
/// core getters to `SelfieConfig` so callers don't need to reach through.
#[derive(Debug, Clone)]
pub(crate) struct CliConfig {
    selfie: SelfieConfig,
    cli: CliSection,
    /// Whether `--allow-sudo` was passed.
    ///
    /// Sits here rather than in [`CliSection`], which is what gets deserialized
    /// from the config file. A `cli: { allow_sudo: true }` would permanently
    /// disable a guard whose whole value is that it fires on the run someone did
    /// not think through; keeping the field out of that struct makes writing one
    /// impossible rather than merely discouraged.
    allow_sudo: bool,
}

impl CliConfig {
    /// Create a new `CliConfig` from its components.
    pub(crate) fn new(selfie: SelfieConfig, cli: CliSection) -> Self {
        Self {
            selfie,
            cli,
            allow_sudo: false,
        }
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

    pub(crate) fn allow_sudo(&self) -> bool {
        self.allow_sudo
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
        if let Some(dir) = self.dotfiles_directory.as_ref() {
            *selfie_config.dotfiles_directory_mut() = Some(dir.clone());
        }
        if let Some(dir) = self.state_directory.as_ref() {
            *selfie_config.state_directory_mut() = Some(dir.clone());
        }

        // Override CLI-specific fields
        if self.verbose {
            cli_section.verbose = true;
        }
        if self.no_color {
            cli_section.use_colors = false;
        }

        let mut config = CliConfig::new(selfie_config, cli_section);
        config.allow_sudo = self.allow_sudo;
        config
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;
    use selfie::config::SelfieConfigBuilder;

    struct FakeArgs {
        environment: Option<&'static str>,
        package_directory: Option<&'static str>,
        dotfiles_directory: Option<&'static str>,
        state_directory: Option<&'static str>,
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
            if let Some(dir) = self.dotfiles_directory {
                args.push("--dotfiles-directory");
                args.push(dir);
            }
            if let Some(dir) = self.state_directory {
                args.push("--state-directory");
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
            dotfiles_directory: None,
            state_directory: None,
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
            dotfiles_directory: None,
            state_directory: None,
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
            dotfiles_directory: None,
            state_directory: None,
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
            dotfiles_directory: None,
            state_directory: None,
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
            dotfiles_directory: None,
            state_directory: None,
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
            .max_concurrency_unchecked(8)
            .build();

        let args = FakeArgs {
            environment: Some("cli-env"),
            package_directory: None,
            dotfiles_directory: None,
            state_directory: None,
            verbose: true,
            no_color: false,
        }
        .into_cli();

        let config = args.build_cli_config(selfie_config, CliSection::default());
        assert_eq!(config.command_timeout().as_secs(), 120);
        assert!(!config.selfie_config().stop_on_error());
        assert_eq!(config.selfie_config().max_concurrency().get(), 8);
    }

    #[test]
    fn test_cli_section_deserialization() {
        let yaml = "verbose: true\nuse_colors: false\n";
        let cli: CliSection = serde_saphyr::from_str(yaml).unwrap();
        assert!(cli.verbose);
        assert!(!cli.use_colors);
    }

    #[test]
    fn test_cli_section_deserialization_defaults() {
        // An empty section: every field takes its own default, and `use_colors`
        // stays true rather than falling to `bool::default()`.
        let cli: CliSection = serde_saphyr::from_str("{}").unwrap();
        assert!(!cli.verbose);
        assert!(cli.use_colors);
    }

    // Parsed directly rather than through `FakeArgs`: the flag is global, so the
    // subcommand it precedes is irrelevant, and this keeps the fixture from
    // growing a field every unrelated test would have to set.
    #[test]
    fn allow_sudo_reaches_the_config() {
        let args = ClapCli::parse_from(["selfie", "--allow-sudo", "apply"]);
        let config = args.build_cli_config(default_selfie_config(), CliSection::default());
        assert!(config.allow_sudo());
    }

    #[test]
    fn allow_sudo_is_off_without_the_flag() {
        let args = ClapCli::parse_from(["selfie", "apply"]);
        let config = args.build_cli_config(default_selfie_config(), CliSection::default());
        assert!(!config.allow_sudo());
    }

    // The guard's value is that it fires on the run someone did not think
    // through, so a config file must not be able to turn it off for good. That
    // holds because `allow_sudo` is not a `CliSection` field — this asserts the
    // consequence, so moving it into that struct fails here rather than silently
    // opening the route.
    #[test]
    fn a_config_file_cannot_turn_the_sudo_guard_off() {
        let cli: CliSection = serde_saphyr::from_str("allow_sudo: true\n").unwrap();

        let args = ClapCli::parse_from(["selfie", "apply"]);
        let config = args.build_cli_config(default_selfie_config(), cli);

        assert!(!config.allow_sudo());
    }

    #[test]
    fn test_selfie_config_accessor() {
        let selfie = default_selfie_config();
        let config = CliConfig::new(selfie.clone(), CliSection::default());
        assert_eq!(config.selfie_config().environment(), selfie.environment());
    }

    #[test]
    fn test_build_cli_config_dotfiles_dir_override() {
        let args = FakeArgs {
            environment: None,
            package_directory: None,
            dotfiles_directory: Some("/cli/configs"),
            state_directory: None,
            verbose: false,
            no_color: false,
        }
        .into_cli();

        let config = args.build_cli_config(default_selfie_config(), CliSection::default());
        assert_eq!(
            config.selfie_config().dotfiles_directory(),
            PathBuf::from("/cli/configs")
        );
    }

    #[test]
    fn test_build_cli_config_state_dir_override() {
        let args = FakeArgs {
            environment: None,
            package_directory: None,
            dotfiles_directory: None,
            state_directory: Some("/cli/state"),
            verbose: false,
            no_color: false,
        }
        .into_cli();

        let config = args.build_cli_config(default_selfie_config(), CliSection::default());
        assert_eq!(
            config.selfie_config().state_directory(),
            Some(&PathBuf::from("/cli/state"))
        );
    }

    #[test]
    fn test_build_cli_config_all_directory_overrides() {
        let args = FakeArgs {
            environment: None,
            package_directory: Some("/cli/packages"),
            dotfiles_directory: Some("/cli/configs"),
            state_directory: Some("/cli/state"),
            verbose: false,
            no_color: false,
        }
        .into_cli();

        let config = args.build_cli_config(default_selfie_config(), CliSection::default());
        assert_eq!(config.package_directory(), &PathBuf::from("/cli/packages"));
        assert_eq!(
            config.selfie_config().dotfiles_directory(),
            PathBuf::from("/cli/configs")
        );
        assert_eq!(
            config.selfie_config().state_directory(),
            Some(&PathBuf::from("/cli/state"))
        );
    }
}
