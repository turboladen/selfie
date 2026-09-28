pub mod diagnostics;
pub mod loader;
pub mod validate;
pub mod yaml;

pub use self::diagnostics::{IgnoredKey, LoadedConfig};
pub use self::loader::ConfigLoadError;
pub use self::yaml::YamlLoader;

#[cfg(feature = "with_mocks")]
pub use self::loader::MockConfigLoader;

use std::{
    num::{NonZeroU64, NonZeroUsize},
    path::{Path, PathBuf},
    time::Duration,
};

use serde::Deserialize;

use crate::fs::FileSystem;

// Off, so an apply reports every failure in one run rather than the first. Read
// by both the resolver and the builder, so the two cannot disagree.
const STOP_ON_ERROR_DEFAULT: bool = false;

/// The settings a run uses, after the configuration file and any overrides have
/// been resolved into one. Built by [`ConfigFile::resolve`].
#[derive(Debug, Clone)]
pub struct SelfieConfig {
    // Core settings
    pub(crate) environment: String,
    pub(crate) package_directory: PathBuf,

    // Optional override for standalone dotfiles directory (YAML definitions + source files
    // for dotfiles not tied to any package; defaults to sibling of package_directory)
    dotfiles_directory: Option<PathBuf>,

    // Optional override for deploy state directory (defaults to ~/.local/state/selfie per XDG)
    state_directory: Option<PathBuf>,

    // Execution settings
    pub(crate) command_timeout: NonZeroU64,
    pub(crate) stop_on_error: bool,
    pub(crate) max_concurrency: NonZeroUsize,
}

/// Returns the default command timeout of 60 seconds.
fn default_command_timeout() -> NonZeroU64 {
    const { NonZeroU64::new(60).unwrap() }
}

/// Returns the default max concurrency, using available parallelism or falling back to 4.
fn default_max_concurrency() -> NonZeroUsize {
    std::thread::available_parallelism().unwrap_or(const { NonZeroUsize::new(4).unwrap() })
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

    /// Get the standalone dotfiles directory path.
    ///
    /// This directory contains standalone dotfile YAML definitions and their
    /// associated source files — dotfiles that aren't tied to any package.
    ///
    /// Defaults to a sibling `dotfiles` directory next to `package_directory`.
    /// For example, if `package_directory` is `~/selfie/packages`, this returns
    /// `~/selfie/dotfiles`. Can be overridden in config.
    #[must_use]
    pub fn dotfiles_directory(&self) -> PathBuf {
        self.dotfiles_directory
            .clone()
            .unwrap_or_else(|| default_dotfiles_directory(&self.package_directory))
    }

    /// Whether a dotfiles directory that is not there is worth reporting.
    ///
    /// True when the user named the path: a configured directory that is not there
    /// is a mistake they can fix. An absent default is the ordinary state of anyone
    /// who keeps no standalone dotfiles, and saying so on every command would be
    /// noise.
    ///
    /// This answers whether an **absence** is worth a word, and nothing else. A
    /// directory selfie could not read or could not classify is refused either way,
    /// because what is behind it is unknown whether or not the user named the path.
    /// Deciding that from this rule is the confusion ADR-0005 decision 2 settles.
    #[must_use]
    pub fn dotfiles_directory_is_expected(&self) -> bool {
        self.dotfiles_directory.is_some()
    }

    /// Get the deploy state directory path.
    ///
    /// `None` means no directory was configured, and deploy state falls back to
    /// `~/.local/state/selfie`.
    #[must_use]
    pub fn state_directory(&self) -> Option<&PathBuf> {
        self.state_directory.as_ref()
    }

    /// Get the command execution timeout duration
    #[must_use]
    pub fn command_timeout(&self) -> Duration {
        Duration::from_secs(self.command_timeout.into())
    }

    /// Get the maximum concurrency for bulk operations
    #[must_use]
    pub fn max_concurrency(&self) -> NonZeroUsize {
        self.max_concurrency
    }

    /// Whether an apply stops at its first failure: an entry or package it
    /// refused, a write that failed, or a dotfiles directory it could not read.
    /// A conflict or a warning never stops one. Off by default.
    #[must_use]
    pub fn stop_on_error(&self) -> bool {
        self.stop_on_error
    }
}

/// The standalone dotfiles directory an unset `dotfiles_directory` takes: a
/// `dotfiles` directory beside `package_directory`.
fn default_dotfiles_directory(package_directory: &Path) -> PathBuf {
    package_directory
        .parent()
        .map(|p| p.join("dotfiles"))
        .unwrap_or_else(|| package_directory.join("dotfiles"))
}

/// What the configuration file says, before anything is filled in.
///
/// Every setting is optional here. A setting that is absent, or empty, is
/// filled from an override or a default by [`resolve`](Self::resolve), which
/// refuses a run that still lacks `environment` or `package_directory`.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ConfigFile {
    environment: Option<String>,
    package_directory: Option<PathBuf>,
    dotfiles_directory: Option<PathBuf>,
    state_directory: Option<PathBuf>,
    command_timeout: Option<NonZeroU64>,
    stop_on_error: Option<bool>,
    max_concurrency: Option<NonZeroUsize>,
}

/// Settings given somewhere other than the file, such as on the command line,
/// which take precedence over the file's.
///
/// A value that is empty counts as not given, so the file's value is used.
#[derive(Debug, Clone, Default)]
pub struct Overrides {
    /// Replaces `environment`.
    pub environment: Option<String>,
    /// Replaces `package_directory`.
    pub package_directory: Option<PathBuf>,
    /// Replaces `dotfiles_directory`.
    pub dotfiles_directory: Option<PathBuf>,
    /// Replaces `state_directory`.
    pub state_directory: Option<PathBuf>,
}

/// A setting a run cannot go without, since it has no default.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequiredSetting {
    /// `environment`.
    Environment,
    /// `package_directory`.
    PackageDirectory,
}

impl RequiredSetting {
    /// The setting's key in the configuration file.
    #[must_use]
    pub fn key(self) -> &'static str {
        match self {
            Self::Environment => "environment",
            Self::PackageDirectory => "package_directory",
        }
    }
}

/// The required settings neither the file nor the overrides supplied.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("Missing required settings: {}", self.keys().join(", "))]
pub struct MissingSettings {
    missing: Vec<RequiredSetting>,
}

impl MissingSettings {
    /// Every required setting that is missing, never only the first.
    #[must_use]
    pub fn missing(&self) -> &[RequiredSetting] {
        &self.missing
    }

    fn keys(&self) -> Vec<&'static str> {
        self.missing.iter().map(|setting| setting.key()).collect()
    }
}

/// Whether a setting's `value` holds nothing but whitespace, and so counts as
/// not given. A value that is not UTF-8 is never blank.
#[must_use]
pub fn is_blank(value: &std::ffi::OsStr) -> bool {
    value.to_str().is_some_and(|value| value.trim().is_empty())
}

/// `raw`, unless it is absent or blank. A value is used as written: the
/// whitespace around it is not trimmed.
fn setting_str(raw: Option<&str>) -> Option<&str> {
    raw.filter(|value| !is_blank(std::ffi::OsStr::new(value)))
}

/// `raw`, unless it is absent or blank.
fn setting_path(raw: Option<&Path>) -> Option<&Path> {
    raw.filter(|value| !is_blank(value.as_os_str()))
}

impl ConfigFile {
    /// The environment the file names, when it names one.
    #[must_use]
    pub fn environment(&self) -> Option<&str> {
        setting_str(self.environment.as_deref())
    }

    /// The package directory the file names, with a leading `~` expanded.
    #[must_use]
    pub fn package_directory(&self, fs: &impl FileSystem) -> Option<PathBuf> {
        setting_path(self.package_directory.as_deref())
            .map(|path| expand_package_directory(fs, path))
    }

    /// The standalone dotfiles directory: the one the file names, or the
    /// default beside the package directory. `None` when the file names
    /// neither.
    #[must_use]
    pub fn dotfiles_directory(&self, fs: &impl FileSystem) -> Option<PathBuf> {
        self.configured_dotfiles_directory(fs).or_else(|| {
            self.package_directory(fs)
                .map(|package_directory| default_dotfiles_directory(&package_directory))
        })
    }

    fn configured_dotfiles_directory(&self, fs: &impl FileSystem) -> Option<PathBuf> {
        setting_path(self.dotfiles_directory.as_deref())
            .map(|path| expand_other_directory(fs, path))
    }

    /// The deploy state directory the file names, with a leading `~` expanded.
    /// `None` when it names none and the default applies.
    #[must_use]
    pub fn state_directory(&self, fs: &impl FileSystem) -> Option<PathBuf> {
        setting_path(self.state_directory.as_deref()).map(|path| expand_other_directory(fs, path))
    }

    /// The command timeout, or its default.
    #[must_use]
    pub fn command_timeout(&self) -> Duration {
        Duration::from_secs(
            self.command_timeout
                .unwrap_or_else(default_command_timeout)
                .get(),
        )
    }

    /// The maximum concurrency, or its default.
    #[must_use]
    pub fn max_concurrency(&self) -> NonZeroUsize {
        self.max_concurrency.unwrap_or_else(default_max_concurrency)
    }

    /// Whether an apply stops at its first failure, or the default.
    #[must_use]
    pub fn stop_on_error(&self) -> bool {
        self.stop_on_error.unwrap_or(STOP_ON_ERROR_DEFAULT)
    }

    /// The settings a run uses: each override where one is given, the file's
    /// value otherwise, and the default where neither says anything.
    ///
    /// # Errors
    ///
    /// [`MissingSettings`] naming every required setting that is still absent.
    pub fn resolve(
        &self,
        fs: &impl FileSystem,
        overrides: &Overrides,
    ) -> Result<SelfieConfig, MissingSettings> {
        let environment = setting_str(overrides.environment.as_deref())
            .or_else(|| self.environment())
            .map(str::to_string);
        let package_directory = setting_path(overrides.package_directory.as_deref())
            .map(Path::to_path_buf)
            .or_else(|| self.package_directory(fs));

        let mut missing = Vec::new();
        if environment.is_none() {
            missing.push(RequiredSetting::Environment);
        }
        if package_directory.is_none() {
            missing.push(RequiredSetting::PackageDirectory);
        }
        let (Some(environment), Some(package_directory)) = (environment, package_directory) else {
            return Err(MissingSettings { missing });
        };

        Ok(SelfieConfig {
            environment,
            package_directory,
            dotfiles_directory: setting_path(overrides.dotfiles_directory.as_deref())
                .map(Path::to_path_buf)
                .or_else(|| self.configured_dotfiles_directory(fs)),
            state_directory: setting_path(overrides.state_directory.as_deref())
                .map(Path::to_path_buf)
                .or_else(|| self.state_directory(fs)),
            command_timeout: self.command_timeout.unwrap_or_else(default_command_timeout),
            stop_on_error: self.stop_on_error(),
            max_concurrency: self.max_concurrency(),
        })
    }
}

// The package directory is canonicalized when it resolves, and kept as written
// when it does not, such as when nothing is there yet.
fn expand_package_directory(fs: &impl FileSystem, path: &Path) -> PathBuf {
    fs.expand_path(path).unwrap_or_else(|_| path.to_path_buf())
}

// The other directories may not exist yet, so only `~` is resolved. A home
// directory that cannot be found leaves the path as written.
fn expand_other_directory(fs: &impl FileSystem, path: &Path) -> PathBuf {
    match self::yaml::expand_tilde_only(fs, path) {
        Ok(Some(expanded)) => expanded,
        _ => path.to_path_buf(),
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
    dotfiles_directory: Option<PathBuf>,
    state_directory: Option<PathBuf>,
    command_timeout: Option<NonZeroU64>,
    max_concurrency_opt: Option<NonZeroUsize>,
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

    #[must_use]
    pub fn dotfiles_directory(mut self, path: PathBuf) -> Self {
        self.dotfiles_directory = Some(path);
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
    pub fn max_concurrency_unchecked(mut self, max: usize) -> Self {
        self.max_concurrency_opt = Some(NonZeroUsize::new(max).unwrap());
        self
    }

    #[must_use]
    pub fn stop_on_error(mut self, stop: bool) -> Self {
        self.stop_on_error = Some(stop);
        self
    }

    #[must_use]
    pub fn state_directory(mut self, path: PathBuf) -> Self {
        self.state_directory = Some(path);
        self
    }

    #[must_use]
    pub fn build(self) -> SelfieConfig {
        SelfieConfig {
            environment: self.environment,
            package_directory: self.package_directory,
            dotfiles_directory: self.dotfiles_directory,
            state_directory: self.state_directory,
            command_timeout: self.command_timeout.unwrap_or(default_command_timeout()),
            max_concurrency: self
                .max_concurrency_opt
                .unwrap_or(default_max_concurrency()),
            stop_on_error: self.stop_on_error.unwrap_or(STOP_ON_ERROR_DEFAULT),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{path::Path, time::Duration};

    #[test]
    fn test_selfie_config_builder() {
        let config = SelfieConfigBuilder::default()
            .environment("test-env")
            .package_directory("/test/path")
            .command_timeout_unchecked(120)
            .max_concurrency_unchecked(8)
            .build();

        assert_eq!(config.environment, "test-env");
        assert_eq!(config.package_directory, PathBuf::from("/test/path"));
        assert_eq!(config.command_timeout(), Duration::from_secs(120));
        assert_eq!(config.max_concurrency, NonZeroUsize::new(8).unwrap());
    }

    #[test]
    fn test_accessor_methods() {
        let config = SelfieConfigBuilder::default()
            .environment("test-env")
            .package_directory("/test/path")
            .command_timeout_unchecked(120)
            .max_concurrency_unchecked(8)
            .stop_on_error(false)
            .build();

        // Test read accessors
        assert_eq!(config.environment(), "test-env");
        assert_eq!(config.package_directory(), &PathBuf::from("/test/path"));
        assert_eq!(config.command_timeout(), Duration::from_secs(120));
        assert_eq!(config.max_concurrency().get(), 8);
        assert!(!config.stop_on_error());
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
        assert!(config.max_concurrency().get() > 0); // Should be based on CPUs or default
        // The literal, not the constant: comparing against the constant passes
        // whatever the default is.
        assert!(!config.stop_on_error());
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
        let yaml = r#"
            environment: "prod"
            package_directory: "/opt/packages"
            command_timeout: 90
            stop_on_error: true
            max_concurrency: 4
        "#;

        let file: ConfigFile = crate::yaml::parse(yaml).unwrap();

        assert_eq!(file.environment(), Some("prod"));
        assert_eq!(file.command_timeout(), Duration::from_secs(90));
        assert_eq!(file.max_concurrency().get(), 4);
        assert!(file.stop_on_error());
    }

    #[test]
    fn test_serde_partial_deserialization() {
        // Every setting is optional in the file, required ones included.
        let file: ConfigFile = crate::yaml::parse("environment: \"dev\"").unwrap();

        assert_eq!(file.environment(), Some("dev"));
        assert!(file.package_directory.is_none());
        assert_eq!(file.command_timeout().as_secs(), 60);
        assert!(file.max_concurrency().get() > 0);
        assert!(!file.stop_on_error());
    }

    #[test]
    fn test_dotfiles_directory_defaults_to_sibling_of_package_directory() {
        let config = SelfieConfigBuilder::default()
            .package_directory("/home/user/selfie-packages/packages")
            .build();
        assert_eq!(
            config.dotfiles_directory(),
            Path::new("/home/user/selfie-packages/dotfiles")
        );
    }

    #[test]
    fn test_dotfiles_directory_can_be_overridden() {
        let config = SelfieConfigBuilder::default()
            .package_directory("/home/user/selfie-packages/packages")
            .dotfiles_directory(PathBuf::from("/custom/dotfiles"))
            .build();
        assert_eq!(config.dotfiles_directory(), Path::new("/custom/dotfiles"));
    }

    #[test]
    fn test_unknown_fields_are_allowed() {
        // YAML string with an unknown field `unknown_field`
        let yaml = r#"
        environment: "prod"
        package_directory: "/opt/packages"
        unknown_field: "this should be ignored"
    "#;

        // Unknown fields are ignored, not denied.
        let file: ConfigFile = crate::yaml::parse(yaml).unwrap();
        assert_eq!(file.environment(), Some("prod"));
        assert_eq!(file.package_directory, Some(PathBuf::from("/opt/packages")));
    }

    mod resolve {
        use std::path::{Path, PathBuf};

        use super::super::{ConfigFile, Overrides, RequiredSetting};
        use crate::fs::{FileSystemError, MockFileSystem};

        // A file naming both required settings, with paths the mock expands to
        // themselves.
        fn file(environment: &str, package_directory: &str) -> ConfigFile {
            ConfigFile {
                environment: Some(environment.to_string()),
                package_directory: Some(PathBuf::from(package_directory)),
                ..ConfigFile::default()
            }
        }

        fn fs() -> MockFileSystem {
            let mut fs = MockFileSystem::default();
            fs.expect_expand_path()
                .returning(|path| Ok(path.to_path_buf()));
            fs
        }

        fn overrides(environment: Option<&str>, package_directory: Option<&str>) -> Overrides {
            Overrides {
                environment: environment.map(str::to_string),
                package_directory: package_directory.map(PathBuf::from),
                ..Overrides::default()
            }
        }

        #[test]
        fn an_override_beats_the_file() {
            let config = file("file-env", "/file/packages")
                .resolve(&fs(), &overrides(Some("flag-env"), Some("/flag/packages")))
                .unwrap();

            assert_eq!(config.environment(), "flag-env");
            assert_eq!(config.package_directory(), Path::new("/flag/packages"));
        }

        #[test]
        fn a_file_value_fills_a_missing_override() {
            let partial = ConfigFile {
                environment: Some("file-env".to_string()),
                ..ConfigFile::default()
            };

            let config = partial
                .resolve(&fs(), &overrides(None, Some("/flag/packages")))
                .unwrap();

            assert_eq!(config.environment(), "file-env");
            assert_eq!(config.package_directory(), Path::new("/flag/packages"));
        }

        #[test]
        fn an_empty_override_falls_back_to_the_file() {
            let config = file("file-env", "/file/packages")
                .resolve(&fs(), &overrides(Some(" "), Some("")))
                .unwrap();

            assert_eq!(config.environment(), "file-env");
            assert_eq!(config.package_directory(), Path::new("/file/packages"));
        }

        #[test]
        fn an_empty_file_value_is_missing() {
            let error = file("", "/file/packages")
                .resolve(&fs(), &Overrides::default())
                .unwrap_err();

            assert_eq!(error.missing(), [RequiredSetting::Environment]);
        }

        // A path of nothing but whitespace names no directory, so it is missing
        // too, rather than a relative directory named ` `.
        #[test]
        fn a_whitespace_path_is_missing() {
            let error = file("env", "  ")
                .resolve(&fs(), &Overrides::default())
                .unwrap_err();

            assert_eq!(error.missing(), [RequiredSetting::PackageDirectory]);
        }

        #[test]
        fn both_missing_settings_are_named() {
            let error = ConfigFile::default()
                .resolve(&fs(), &Overrides::default())
                .unwrap_err();

            assert_eq!(
                error.missing(),
                [
                    RequiredSetting::Environment,
                    RequiredSetting::PackageDirectory
                ]
            );
            assert_eq!(
                error.to_string(),
                "Missing required settings: environment, package_directory"
            );
        }

        // A home directory selfie cannot find leaves a `~` path as written. For
        // the state directory that is then refused as not absolute, which is the
        // safe outcome: the other choice is to fall back to a default nobody
        // named.
        #[test]
        fn a_home_failure_keeps_the_literal_path() {
            let mut fs = MockFileSystem::default();
            fs.expect_expand_path()
                .returning(|_| Err(FileSystemError::HomeDirNotFound));
            let with_state = ConfigFile {
                state_directory: Some(PathBuf::from("~/state")),
                ..file("env", "/file/packages")
            };

            let config = with_state.resolve(&fs, &Overrides::default()).unwrap();

            assert_eq!(config.state_directory(), Some(&PathBuf::from("~/state")));
        }
    }
}
