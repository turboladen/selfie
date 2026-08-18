use std::path::{Path, PathBuf};

#[cfg(test)]
use std::sync::Arc;

use config::FileFormat;

use crate::{config::SelfieConfig, fs::FileSystem};

use super::{
    diagnostics::{FRONTEND_SECTIONS, LoadedConfig, library_ignored_keys},
    loader::{
        ConfigLoadError, ConfigLoader, irregular_config_refusal, unresolvable_config_refusal,
    },
};

/// YAML-based configuration loader implementation
///
/// Loads application configuration from YAML files in standard locations.
/// Supports both `.yaml` and `.yml` file extensions and handles path expansion.
pub struct YamlLoader<'a, F: FileSystem> {
    /// File system abstraction for reading files and paths
    fs: &'a F,
}

impl<'a, F: FileSystem> YamlLoader<'a, F> {
    /// Create a loader that reads configuration through `fs`.
    #[must_use]
    pub fn new(fs: &'a F) -> Self {
        Self { fs }
    }
}

impl<F: FileSystem> ConfigLoader for YamlLoader<'_, F> {
    /// Load configuration from YAML files in standard locations
    ///
    /// Searches for `config.yaml` or `config.yml` in the user's configuration directory
    /// and loads the first one found. Performs path expansion for the package directory.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigLoadError`] if:
    /// - No configuration file is found in standard locations
    /// - Multiple configuration files are found (both .yaml and .yml)
    /// - File system access fails
    /// - YAML content is malformed or invalid
    /// - Required configuration fields are missing
    /// - Configuration field types are incorrect
    fn load_config(&self) -> Result<LoadedConfig, ConfigLoadError> {
        let config_paths = match self.find_config_file_paths() {
            Ok(paths) => paths,
            Err(ConfigLoadError::NotFound { searched }) => {
                // "Nothing there" and "there, but the link goes nowhere" are
                // different answers, and `path_exists` follows so it cannot tell
                // them apart on its own.
                if let Some(refusal) = unresolvable_config_refusal(self.fs, &searched) {
                    return Err(refusal);
                }
                return Err(ConfigLoadError::NotFound { searched });
            }
            Err(other) => return Err(other),
        };

        if config_paths.len() > 1 {
            return Err(ConfigLoadError::MultipleFound(
                config_paths
                    .into_iter()
                    .map(|p| p.display().to_string())
                    .collect::<Vec<_>>(),
            ));
        }

        // This should never happen now since find_config_file_paths
        // either returns a non-empty vector or an error
        if config_paths.is_empty() {
            return Err(ConfigLoadError::NotFound {
                searched: PathBuf::from("~/.config/selfie"),
            });
        }

        // Start with default configuration
        let mut builder = config::Config::builder();

        let config_path = &config_paths[0];

        // Before the read, not after it. `find_config_file_paths` selects a path
        // with `path_exists`, which a fifo satisfies, and reading one blocks
        // until a writer arrives.
        if let Some(refusal) = irregular_config_refusal(self.fs, config_path) {
            return Err(refusal);
        }

        let file_contents = self.fs.read_file(config_path)?;

        builder = builder.add_source(config::File::from_str(&file_contents, FileFormat::Yaml));

        // Build the config
        let config = builder.build()?;

        // Lift out each frontend's section *before* deserializing, so a frontend
        // reads its own settings from this parse instead of opening the file
        // again with a different YAML library.
        let sections: std::collections::BTreeMap<String, config::Value> = FRONTEND_SECTIONS
            .iter()
            .filter_map(|name| {
                config
                    .get::<config::Value>(name)
                    .ok()
                    .map(|value| ((*name).to_string(), value))
            })
            .collect();

        // Every key it did not consume, rather than dropping them.
        // `serde_ignored` wraps the deserializer, so the set is serde's own
        // answer and cannot go stale when a field is added here.
        let mut ignored_paths = Vec::new();
        let mut selfie_config: SelfieConfig =
            serde_ignored::deserialize(config, |path| ignored_paths.push(path.to_string()))?;
        let ignored_keys = library_ignored_keys(ignored_paths);

        // Special handling for ~ expansion on path fields
        if let Ok(expanded) = self.fs.expand_path(selfie_config.package_directory()) {
            selfie_config.package_directory = expanded;
        }
        // For dotfiles_directory and state_directory, expand ~ without canonicalizing.
        // These directories may not exist yet (especially state_directory on first run),
        // so canonicalize() would fail. Instead, resolve just "~" and join the rest.
        if let Some(ref dotfiles_dir) = selfie_config.dotfiles_directory
            && let Some(expanded) = expand_tilde_only(self.fs, dotfiles_dir)
        {
            selfie_config.dotfiles_directory = Some(expanded);
        }
        if let Some(ref state_dir) = selfie_config.state_directory
            && let Some(expanded) = expand_tilde_only(self.fs, state_dir)
        {
            selfie_config.state_directory = Some(expanded);
        }

        Ok(LoadedConfig::new(selfie_config, ignored_keys, sections))
    }

    /// Find configuration file paths in standard locations
    ///
    /// Searches for both `config.yaml` and `config.yml` in the user's configuration
    /// directory. Returns all found configuration files.
    ///
    /// # Errors
    ///
    /// Returns the searched directory path if:
    /// - The user's configuration directory cannot be determined
    /// - No configuration files are found in the search location
    fn find_config_file_paths(&self) -> Result<Vec<PathBuf>, ConfigLoadError> {
        let mut paths = Vec::new();

        // Not "no configuration file found in ~/.config/selfie": there is no
        // directory to search, and naming one selfie never looked in sends the
        // user to the wrong place. `SELFIE_CONFIG_DIR` and `XDG_CONFIG_HOME`
        // both move it.
        let config_dir = self.fs.config_dir()?;

        let first_yaml = config_dir.join("config.yaml");
        let second_yaml = config_dir.join("config.yml");

        if self.fs.path_exists(&first_yaml) {
            paths.push(first_yaml);
        }
        if self.fs.path_exists(&second_yaml) {
            paths.push(second_yaml);
        }

        if paths.is_empty() {
            return Err(ConfigLoadError::NotFound {
                searched: config_dir,
            });
        }

        Ok(paths)
    }
}

/// Expand `~` in a path without canonicalizing. Returns the expanded path if the
/// input starts with `~`, or `None` if it doesn't need expansion. This avoids
/// the failure mode of `expand_path` (which canonicalizes) when the target
/// directory doesn't exist yet.
fn expand_tilde_only(fs: &impl FileSystem, path: &Path) -> Option<PathBuf> {
    let path_str = path.to_string_lossy();
    if !path_str.starts_with('~') {
        return None;
    }
    // Expand just "~" to get the home directory, then join the remainder
    let home = fs.expand_path(&PathBuf::from("~")).ok()?;
    let rest = path_str.strip_prefix("~/").unwrap_or(&path_str[1..]);
    if rest.is_empty() {
        Some(home)
    } else {
        Some(home.join(rest))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fs::filesystem::{FileSystemError, MockFileSystem};
    use std::path::Path;

    fn setup_test_fs() -> (MockFileSystem, PathBuf) {
        let mut fs = MockFileSystem::default();

        // Set up mock HOME environment for test
        let home_dir = Path::new("/home/test");

        // Create .config/selfie/config.yaml
        let config_yaml = r#"
            environment: "test-env"
            package_directory: "/test/packages"
        "#;

        let config_dir = home_dir.join(".config").join("selfie");
        let config_path = config_dir.join("config.yaml");

        fs.mock_path_exists(&config_path, true);
        fs.mock_path_exists(&config_dir.join("config.yml"), false);
        fs.mock_read_file(config_path, config_yaml);
        mock_regular_file(&mut fs);

        (fs, home_dir.into())
    }

    // The loader asks this immediately before the read, so any fixture whose
    // config file is an ordinary one needs it. `mock_config_file` includes it;
    // fixtures that build the expectations by hand call this.
    fn mock_regular_file(fs: &mut MockFileSystem) {
        fs.expect_irregular_target_refusal().returning(|_| None);
    }

    // Answer the dangling-link question with "no". Needed by any fixture that
    // reaches the not-found path, which is where that question is asked.
    fn mock_no_symlinks(fs: &mut MockFileSystem) {
        fs.expect_symlink_refusal().returning(|_| None);
    }

    // Every expectation for finding a config file, and none for reading it.
    // A read attempt then fails the test on an unexpected call, which is what
    // pins that the guard runs *before* the read rather than after it.
    fn mock_config_path_without_read(
        fs: &mut MockFileSystem,
        config_dir: &Path,
        refusal: FileSystemError,
    ) {
        fs.mock_config_dir_ok(config_dir);
        fs.mock_path_exists(config_dir.join("config.yaml"), true);
        fs.mock_path_exists(config_dir.join("config.yml"), false);
        fs.expect_irregular_target_refusal()
            .return_once(move |_| Some(refusal));
    }

    mod find_config_file_paths {
        use super::*;

        #[test]
        fn test_find_config_paths() {
            let (mut fs, home_dir) = setup_test_fs();
            let config_dir = home_dir.join(".config").join("selfie");
            fs.mock_config_dir_ok(&config_dir);
            fs.mock_path_exists(config_dir.join("selfie").join("config.yaml"), true);

            let loader = YamlLoader::new(&fs);

            let paths = loader.find_config_file_paths().unwrap();

            // Should find at least the one we set up
            assert!(!paths.is_empty());
            assert!(paths.iter().any(|p| p.ends_with("config.yaml")));
        }

        #[test]
        fn test_find_config_paths_multiple_formats() {
            let mut fs = MockFileSystem::default();
            let config_dir = Path::new("/home/test/.config/selfie");

            // Mock both .yaml and .yml existing
            let yaml_path = config_dir.join("config.yaml");
            let yml_path = config_dir.join("config.yml");

            fs.mock_config_dir_ok(&config_dir);
            fs.mock_path_exists(&yaml_path, true);
            fs.mock_path_exists(&yml_path, true);

            let loader = YamlLoader::new(&fs);
            let paths = loader.find_config_file_paths().unwrap();

            // Should find both files
            assert_eq!(paths.len(), 2);
            assert!(paths.contains(&yaml_path));
            assert!(paths.contains(&yml_path));
        }

        #[test]
        fn test_find_config_paths_no_config_dir() {
            let mut fs = MockFileSystem::default();

            // Mock config_dir failing
            fs.expect_config_dir()
                .return_once(|| Err(FileSystemError::HomeDirNotFound));

            let loader = YamlLoader::new(&fs);
            let result = loader.find_config_file_paths();

            // "There is no directory to search" — not "no file found in
            // ~/.config/selfie", which names somewhere selfie never looked.
            assert!(matches!(
                result.unwrap_err(),
                ConfigLoadError::FileSystemError(FileSystemError::HomeDirNotFound)
            ));
        }
    }

    mod load_config {
        use super::*;

        #[test]
        fn test_load_config() {
            let (mut fs, home_dir) = setup_test_fs();
            let config_dir = home_dir.join(".config").join("selfie");
            fs.mock_config_dir_ok(&config_dir);
            fs.mock_path_exists(config_dir.join("config.yaml"), true);

            let package_dir = Path::new("/test/packages");
            fs.mock_path_exists(&package_dir, true);
            fs.mock_expand_path(&package_dir, &package_dir);

            let loader = YamlLoader::new(&fs);
            let config = loader.load_config().unwrap().into_config();

            // Check the loaded values
            assert_eq!(config.environment, "test-env");
            assert_eq!(config.package_directory, package_dir);
        }

        #[test]
        fn test_load_config_not_found() {
            let mut fs = MockFileSystem::default(); // Empty file system
            let config_dir = Path::new("/home/test/.config/selfie");
            fs.mock_config_dir_ok(&config_dir);
            fs.mock_path_exists(config_dir, true);
            fs.mock_path_exists(config_dir.join("config.yaml"), false);
            fs.mock_path_exists(config_dir.join("config.yml"), false);
            mock_no_symlinks(&mut fs);

            let loader = YamlLoader::new(&fs);

            // Should return error
            let result = loader.load_config();
            assert!(matches!(
                result,
                Err(ConfigLoadError::NotFound { searched: _ })
            ));
        }

        #[test]
        fn test_load_config_with_extended_settings() {
            let mut fs = MockFileSystem::default();
            let config_dir = Path::new("/home/test/.config/selfie");

            // Config with extended settings
            let config_yaml = r#"
            environment: "test-env"
            package_directory: "/test/packages"
            command_timeout: 120
            stop_on_error: false
            max_concurrency: 8
        "#;

            fs.mock_config_file(config_dir, config_yaml);
            fs.mock_expand_path("/test/packages", "/test/packages");

            let loader = YamlLoader::new(&fs);
            let config = loader.load_config().unwrap().into_config();

            // Check basic settings
            assert_eq!(config.environment, "test-env");
            assert_eq!(config.package_directory, Path::new("/test/packages"));

            // Check extended settings
            assert_eq!(config.command_timeout, 120.try_into().unwrap());
            assert!(!config.stop_on_error);
            assert_eq!(config.max_concurrency, 8.try_into().unwrap());
        }

        #[test]
        fn test_load_config_invalid_yaml() {
            let mut fs = MockFileSystem::default();
            let config_dir = Path::new("/home/test/.config/selfie");

            // Set up an invalid YAML file
            let invalid_yaml = r#"
        environment: "test-env"
        package_directory: "/test/packages"
        invalid:yaml:format
    "#;

            fs.mock_config_file(config_dir, invalid_yaml);

            let loader = YamlLoader::new(&fs);
            let result = loader.load_config();

            assert!(result.is_err());
            if let Err(err) = result {
                match err {
                    ConfigLoadError::ConfigError(_) => {
                        // Expected error type
                    }
                    _ => panic!("Expected ConfigError, got: {err:?}"),
                }
            }
        }

        #[test]
        fn test_load_config_missing_required_fields() {
            let mut fs = MockFileSystem::default();
            let config_dir = Path::new("/home/test/.config/selfie");

            // Config missing required fields
            let incomplete_yaml = r#"
        # Missing environment field
        package_directory: "/test/packages"
    "#;

            fs.mock_config_file(config_dir, incomplete_yaml);

            let loader = YamlLoader::new(&fs);
            let result = loader.load_config();

            assert!(result.is_err());
            if let Err(err) = result {
                match err {
                    ConfigLoadError::ConfigError(_) => {
                        // Expected error type for missing fields
                    }
                    _ => panic!("Expected ConfigError, got: {err:?}"),
                }
            }
        }

        #[test]
        fn test_load_config_invalid_field_types() {
            let mut fs = MockFileSystem::default();
            let config_dir = Path::new("/home/test/.config/selfie");

            // Config with invalid types
            let invalid_types_yaml = r#"
        environment: "test-env"
        package_directory: "/test/packages"
        command_timeout: "not-a-number"  # Should be a number
    "#;

            fs.mock_config_file(config_dir, invalid_types_yaml);

            let loader = YamlLoader::new(&fs);
            let result = loader.load_config();

            assert!(result.is_err());
        }

        #[test]
        fn test_load_config_with_tilde_expansion() {
            let mut fs = MockFileSystem::default();
            let config_dir = Path::new("/home/test/.config/selfie");
            let home_dir = Path::new("/home/test");

            // Config with tilde in path
            let tilde_yaml = r#"
        environment: "test-env"
        package_directory: "~/packages"
    "#;

            let expanded_path = home_dir.join("packages");

            fs.mock_config_file(config_dir, tilde_yaml);
            fs.mock_expand_path(Path::new("~/packages"), &expanded_path);

            let loader = YamlLoader::new(&fs);
            let config = loader.load_config().unwrap().into_config();

            assert_eq!(config.package_directory, expanded_path);
        }

        #[test]
        fn test_load_config_defaults() {
            let mut fs = MockFileSystem::default();
            let config_dir = Path::new("/home/test/.config/selfie");

            // Minimal valid config
            let minimal_yaml = r#"
        environment: "test-env"
        package_directory: "/test/packages"
        # All other fields will use defaults
    "#;

            let config_path = config_dir.join("config.yaml");
            fs.mock_config_dir_ok(&config_dir);
            fs.mock_path_exists(&config_path, true);
            fs.mock_path_exists(&config_dir.join("config.yml"), false);
            fs.mock_read_file(&config_path, minimal_yaml);
            mock_regular_file(&mut fs);
            fs.mock_expand_path(Path::new("/test/packages"), Path::new("/test/packages"));

            let loader = YamlLoader::new(&fs);
            let config = loader.load_config().unwrap().into_config();

            // Check defaults were properly applied
            assert_eq!(config.environment, "test-env");
            assert_eq!(config.package_directory, Path::new("/test/packages"));
            assert!(config.stop_on_error); // Default

            // Check command_timeout has default value (60)
            assert_eq!(config.command_timeout.get(), 60);

            // Check max_concurrency has sensible default value
            assert!(config.max_concurrency.get() > 0);
        }

        #[test]
        fn test_multiple_files() {
            let mut fs = MockFileSystem::default();
            let config_dir = Path::new("/home/test/.config/selfie");

            // Mock both .yaml and .yml existing
            let yaml_path = config_dir.join("config.yaml");
            let yml_path = config_dir.join("config.yml");

            fs.mock_config_dir_ok(&config_dir);
            fs.mock_path_exists(&yaml_path, true);
            fs.mock_path_exists(&yml_path, true);

            let loader = YamlLoader::new(&fs);
            let err = loader.load_config();

            // Should find both files
            assert!(matches!(err, Err(ConfigLoadError::MultipleFound(_))));
        }

        #[test]
        fn test_config_load_invalid_yaml_error() {
            let mut fs = MockFileSystem::default();
            let config_dir = Path::new("/home/test/.config/selfie");
            let config_path = config_dir.join("config.yaml");

            fs.mock_config_dir_ok(&config_dir);
            fs.mock_path_exists(&config_path, true);
            fs.mock_path_exists(&config_dir.join("config.yml"), false);

            // Mock invalid YAML content
            let invalid_yaml = "invalid: yaml: content: [";
            fs.mock_read_file(&config_path, invalid_yaml);
            mock_regular_file(&mut fs);

            let loader = YamlLoader::new(&fs);
            let result = loader.load_config();

            assert!(result.is_err());
            match result.unwrap_err() {
                ConfigLoadError::ConfigError(_) => {
                    // Expected config error for invalid YAML
                }
                _ => panic!("Expected ConfigError for invalid YAML"),
            }
        }

        #[test]
        fn test_config_load_filesystem_error() {
            let mut fs = MockFileSystem::default();
            let config_dir = Path::new("/home/test/.config/selfie");
            let config_path = config_dir.join("config.yaml");

            fs.mock_config_dir_ok(&config_dir);
            fs.mock_path_exists(&config_path, true);
            fs.mock_path_exists(&config_dir.join("config.yml"), false);

            mock_regular_file(&mut fs);

            // Mock filesystem error when reading file
            fs.expect_read_file()
                .with(mockall::predicate::eq(config_path.clone()))
                .return_once(|_| {
                    Err(FileSystemError::IoError(Arc::new(std::io::Error::new(
                        std::io::ErrorKind::PermissionDenied,
                        "Permission denied",
                    ))))
                });

            let loader = YamlLoader::new(&fs);
            let result = loader.load_config();

            assert!(result.is_err());
            match result.unwrap_err() {
                ConfigLoadError::FileSystemError(FileSystemError::IoError(e)) => {
                    assert_eq!(e.kind(), std::io::ErrorKind::PermissionDenied);
                }
                other => panic!("Expected FileSystemError::IoError, got: {other:?}"),
            }
        }

        #[test]
        fn test_config_load_config_dir_error() {
            let mut fs = MockFileSystem::default();

            // Mock config_dir failing
            fs.expect_config_dir()
                .return_once(|| Err(FileSystemError::HomeDirNotFound));
            mock_no_symlinks(&mut fs);

            let loader = YamlLoader::new(&fs);
            let result = loader.load_config();

            assert!(matches!(
                result.unwrap_err(),
                ConfigLoadError::FileSystemError(FileSystemError::HomeDirNotFound)
            ));
        }

        // A fifo at the config path wedged every command: they all load
        // configuration first, and opening a fifo blocks until a writer arrives.
        // No `read_file` expectation is registered here on purpose -- if the
        // guard moves after the read, mockall fails the test on the unexpected
        // call. That is the assertion; the returned error is the secondary one.
        #[test]
        fn a_fifo_config_is_refused_without_reading_it() {
            let mut fs = MockFileSystem::default();
            let config_dir = Path::new("/home/test/.config/selfie");
            mock_config_path_without_read(
                &mut fs,
                config_dir,
                FileSystemError::IrregularTarget {
                    path: config_dir.join("config.yaml"),
                    kind: "named pipe (fifo)",
                },
            );

            let result = YamlLoader::new(&fs).load_config();

            match result.unwrap_err() {
                ConfigLoadError::IrregularFile { path, kind } => {
                    assert_eq!(path, config_dir.join("config.yaml"));
                    assert_eq!(kind, "named pipe (fifo)");
                }
                other => panic!("Expected IrregularFile, got: {other:?}"),
            }
        }

        #[test]
        fn a_socket_config_is_refused_without_reading_it() {
            let mut fs = MockFileSystem::default();
            let config_dir = Path::new("/home/test/.config/selfie");
            mock_config_path_without_read(
                &mut fs,
                config_dir,
                FileSystemError::IrregularTarget {
                    path: config_dir.join("config.yaml"),
                    kind: "socket",
                },
            );

            let result = YamlLoader::new(&fs).load_config();

            assert!(matches!(
                result.unwrap_err(),
                ConfigLoadError::IrregularFile { kind: "socket", .. }
            ));
        }

        // The refusal must not read as "there is no config file". A fix that
        // skipped irregular files during discovery instead of refusing them
        // would produce exactly that, and would look like a pass on the variant
        // check alone.
        #[test]
        fn a_fifo_config_is_not_reported_as_missing() {
            let mut fs = MockFileSystem::default();
            let config_dir = Path::new("/home/test/.config/selfie");
            mock_config_path_without_read(
                &mut fs,
                config_dir,
                FileSystemError::IrregularTarget {
                    path: config_dir.join("config.yaml"),
                    kind: "named pipe (fifo)",
                },
            );

            let message = YamlLoader::new(&fs).load_config().unwrap_err().to_string();

            assert!(message.contains("not a regular file"), "got: {message}");
            assert!(message.contains("named pipe (fifo)"), "got: {message}");
            assert!(
                !message.contains("No configuration file found"),
                "got: {message}"
            );
        }

        // The `other` arm of `irregular_config_refusal` fails closed. Nothing
        // reaches it today, so only a mock can drive it -- and a wildcard that
        // let the value through would un-guard the read.
        #[test]
        fn an_unexpected_refusal_kind_is_still_fatal() {
            let mut fs = MockFileSystem::default();
            let config_dir = Path::new("/home/test/.config/selfie");
            mock_config_path_without_read(&mut fs, config_dir, FileSystemError::HomeDirNotFound);

            let result = YamlLoader::new(&fs).load_config();

            assert!(matches!(
                result.unwrap_err(),
                ConfigLoadError::FileSystemError(FileSystemError::HomeDirNotFound)
            ));
        }

        // A config file symlinked into a dotfiles repository that has not been
        // checked out yet. `path_exists` follows, so discovery answers "absent" —
        // and the file is right there. Reported as absent it becomes either an
        // error naming a directory the file is in, or, once flags can stand in
        // for a missing file, a silent run with the user's configuration ignored.
        #[test]
        fn a_dangling_config_symlink_is_not_reported_as_absent() {
            let mut fs = MockFileSystem::default();
            let config_dir = Path::new("/home/test/.config/selfie");
            fs.mock_config_dir_ok(config_dir);
            fs.mock_path_exists(config_dir.join("config.yaml"), false);
            fs.mock_path_exists(config_dir.join("config.yml"), false);
            fs.expect_symlink_refusal().returning(|target| {
                target
                    .path()
                    .ends_with("config.yaml")
                    .then(|| FileSystemError::SymlinkedTarget {
                        path: target.path().to_path_buf(),
                        points_to: Some(PathBuf::from("/not/checked/out/config.yaml")),
                    })
            });

            let message = YamlLoader::new(&fs).load_config().unwrap_err().to_string();

            assert!(
                message.contains("does not resolve"),
                "the link must be named as unresolvable, got: {message}"
            );
            assert!(
                !message.contains("No configuration file found"),
                "a link that is present must not be reported as absent, got: {message}"
            );
        }

        // The other control: nothing there at all is still plain `NotFound`, not
        // an unresolvable link.
        #[test]
        fn an_empty_config_directory_is_still_reported_as_not_found() {
            let mut fs = MockFileSystem::default();
            let config_dir = Path::new("/home/test/.config/selfie");
            fs.mock_config_dir_ok(config_dir);
            fs.mock_path_exists(config_dir.join("config.yaml"), false);
            fs.mock_path_exists(config_dir.join("config.yml"), false);
            mock_no_symlinks(&mut fs);

            let result = YamlLoader::new(&fs).load_config();

            assert!(matches!(
                result.unwrap_err(),
                ConfigLoadError::NotFound { .. }
            ));
        }

        // The wording and filtering have their own tests beside the types. These
        // drive the real loader instead, because what they pin is the behavior of
        // `serde_ignored` over the `config` crate's deserializer -- which keys it
        // reports, and in what shape.
        fn ignored_keys_for(config_yaml: &str) -> Vec<String> {
            let mut fs = MockFileSystem::default();
            let config_dir = Path::new("/home/test/.config/selfie");
            fs.mock_config_file(config_dir, config_yaml);
            fs.mock_expand_path("/test/packages", "/test/packages");

            YamlLoader::new(&fs)
                .load_config()
                .unwrap()
                .ignored_keys()
                .iter()
                .map(|k| k.key().to_string())
                .collect()
        }

        #[test]
        fn a_stale_key_is_reported_by_the_loader() {
            let keys = ignored_keys_for(
                r#"
                environment: "test-env"
                package_directory: "/test/packages"
                configs_directory: "/test/configs"
            "#,
            );

            assert_eq!(keys, vec!["configs_directory".to_string()]);
        }

        // The trap. `cli:` is another frontend's section and the library must
        // stay silent about it, or every user sees a warning on every run.
        // This also pins the shape the filter depends on: an ignored subtree is
        // reported as its root, so `cli` never arrives as `cli.verbose`.
        #[test]
        fn a_valid_cli_section_produces_no_diagnostics() {
            let keys = ignored_keys_for(
                r#"
                environment: "test-env"
                package_directory: "/test/packages"
                dotfiles_directory: "/test/dotfiles"
                state_directory: "/test/state"
                command_timeout: 120
                stop_on_error: false
                max_concurrency: 4
                cli:
                  verbose: true
                  use_colors: false
            "#,
            );

            assert!(keys.is_empty(), "expected no diagnostics, got: {keys:?}");
        }

        #[test]
        fn a_nested_frontend_key_is_not_reported() {
            let keys = ignored_keys_for(
                r#"
                environment: "test-env"
                package_directory: "/test/packages"
                cli:
                  deeply:
                    nested: 1
            "#,
            );

            assert!(keys.is_empty(), "expected no diagnostics, got: {keys:?}");
        }

        // Validation must not stop at the first problem.
        #[test]
        fn two_ignored_keys_are_both_reported_by_the_loader() {
            let mut keys = ignored_keys_for(
                r#"
                environment: "test-env"
                package_directory: "/test/packages"
                configs_directory: "/test/configs"
                totally_bogus: 1
            "#,
            );
            keys.sort();

            assert_eq!(
                keys,
                vec!["configs_directory".to_string(), "totally_bogus".to_string()]
            );
        }

        // The control: a file using every setting selfie has must produce
        // nothing, or the diagnostic is just noise with extra steps.
        #[test]
        fn a_config_using_every_setting_produces_no_diagnostics() {
            let keys = ignored_keys_for(
                r#"
                environment: "test-env"
                package_directory: "/test/packages"
                dotfiles_directory: "/test/dotfiles"
                state_directory: "/test/state"
                command_timeout: 120
                stop_on_error: false
                max_concurrency: 4
            "#,
            );

            assert!(keys.is_empty(), "expected no diagnostics, got: {keys:?}");
        }

        // The `cli:` section comes back from the parse the library already did,
        // so a frontend never opens the file a second time with a second YAML
        // library. These pin what that hands back.
        #[derive(Debug, serde::Deserialize)]
        struct FakeSection {
            #[serde(default)]
            verbose: bool,
        }

        fn load_section(config_yaml: &str) -> Result<Option<(bool, Vec<String>)>, ConfigLoadError> {
            let mut fs = MockFileSystem::default();
            let config_dir = Path::new("/home/test/.config/selfie");
            fs.mock_config_file(config_dir, config_yaml);
            fs.mock_expand_path("/test/packages", "/test/packages");

            let loaded = YamlLoader::new(&fs).load_config().unwrap();
            Ok(loaded
                .frontend_section::<FakeSection>("cli")?
                .map(|section| {
                    let keys = section
                        .ignored_keys()
                        .iter()
                        .map(|k| k.key().to_string())
                        .collect();
                    (section.value().verbose, keys)
                }))
        }

        const BASE: &str = "environment: \"test-env\"\npackage_directory: \"/test/packages\"\n";

        #[test]
        fn a_frontend_section_is_read_from_the_library_s_own_parse() {
            let (verbose, ignored) = load_section(&format!("{BASE}cli:\n  verbose: true\n"))
                .unwrap()
                .unwrap();

            assert!(verbose);
            assert!(ignored.is_empty());
        }

        // Rooted at the section, so the key is `verbos`, not `cli.verbos`.
        #[test]
        fn an_unknown_key_inside_a_section_is_reported_relative_to_it() {
            let (_, ignored) = load_section(&format!("{BASE}cli:\n  verbos: true\n"))
                .unwrap()
                .unwrap();

            assert_eq!(ignored, vec!["verbos".to_string()]);
        }

        // A dotted top-level key is **path syntax** to the `config` crate, not a
        // key whose name contains a dot: `"cli.verbose": true` is merged into the
        // `cli` table and read as that section's `verbose`. So it is consumed
        // rather than ignored, at either level.
        //
        // Worth pinning because the obvious reading is the opposite one, and
        // because it is why a top-level key can no longer be mistaken for one
        // inside a section: reading the section from this same parse means there
        // is no second, differently-parsed view to disagree with.
        #[test]
        fn a_dotted_top_level_key_is_merged_into_the_section() {
            let mut fs = MockFileSystem::default();
            let config_dir = Path::new("/home/test/.config/selfie");
            fs.mock_config_file(config_dir, &format!("{BASE}\"cli.verbose\": true\n"));
            fs.mock_expand_path("/test/packages", "/test/packages");

            let loaded = YamlLoader::new(&fs).load_config().unwrap();
            let section = loaded
                .frontend_section::<FakeSection>("cli")
                .unwrap()
                .expect("the dotted key creates the section");

            assert!(section.value().verbose, "it is read as the section's key");
            assert!(
                loaded.ignored_keys().is_empty(),
                "and so is not reported at the top level, got: {:?}",
                loaded.ignored_keys()
            );
        }

        // An empty section is a legitimate thing to write. It parses as null, and
        // deserializing a struct from null fails — so it is treated as absent
        // rather than reported as malformed.
        #[test]
        fn an_empty_section_reads_as_absent() {
            assert!(load_section(&format!("{BASE}cli:\n")).unwrap().is_none());
        }

        #[test]
        fn a_missing_section_reads_as_absent() {
            assert!(load_section(BASE).unwrap().is_none());
        }

        // `cli: true` is not an empty section — it is a value of the wrong shape,
        // and the frontend needs to hear about it. It was invisible to both the
        // library (which filters the section out as another frontend's) and the
        // CLI (which could not parse it and quietly used defaults).
        #[test]
        fn a_section_of_the_wrong_shape_is_an_error() {
            assert!(load_section(&format!("{BASE}cli: true\n")).is_err());
        }

        #[test]
        fn test_config_error_display_formatting() {
            // Test NotFound error
            let searched_path = PathBuf::from("/searched/paths");
            let not_found_error = ConfigLoadError::NotFound {
                searched: searched_path,
            };
            assert!(
                not_found_error
                    .to_string()
                    .contains("No configuration file found")
            );
            assert!(not_found_error.to_string().contains("/searched/paths"));

            // Test MultipleFound error
            let multiple_error = ConfigLoadError::MultipleFound(vec![
                "config1.yaml".to_string(),
                "config2.yml".to_string(),
            ]);
            assert!(
                multiple_error
                    .to_string()
                    .contains("Multiple configuration files found")
            );
            assert!(multiple_error.to_string().contains("config1.yaml"));
        }
    }
}
