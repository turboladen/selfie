mod builder;
pub mod event;
pub mod port;
pub mod repository;
pub mod service;
pub mod validate;

/// Re-export read-only git status types used by the package service layer.
pub mod git {
    pub use crate::git::{GitDirectoryStatus, GitFileStatus, GitStatusError, GitStatusProvider};

    #[cfg(any(test, feature = "with_mocks"))]
    pub use crate::git::MockGitStatusProvider;
}

/// Re-export the concrete git adapter under its package-layer name.
pub mod git_adapter {
    pub use crate::git::GixGitAdapter as GixGitStatusProvider;
}

pub use self::builder::{EnvironmentConfigBuilder, PackageBuilder};
pub use self::service::{InstallOptions, PackageService, SpecService};

// Core package entity and related types
use std::{collections::HashMap, path::PathBuf};

use serde::{Deserialize, Serialize};

/// Package data for editing operations
///
/// Contains a package and its file metadata for editing workflows.
/// Can represent either an existing package loaded from the repository
/// or a new package template ready for creation.
#[derive(Debug, Clone)]
pub struct GetPackage {
    /// The package content (either loaded or template)
    pub(crate) package: Package,
    /// The file path where the package is/should be stored
    pub(crate) file_path: PathBuf,
    /// Whether this is a new package (true) or existing (false)
    pub(crate) is_new: bool,
}

impl GetPackage {
    /// Create a new package template for the given name and directory
    ///
    /// This creates a basic package template with minimal configuration
    /// that can be used as a starting point for new packages.
    ///
    /// # Arguments
    ///
    /// * `name` - The package name
    /// * `packages_directory` - The directory where package files are stored
    #[must_use]
    pub fn new(name: &str, packages_directory: &std::path::Path) -> Self {
        let file_path = packages_directory.join(format!("{name}.yml"));
        let package = Package::new_template(name);

        Self {
            package,
            file_path,
            is_new: true,
        }
    }

    /// Create a `GetPackage` from an existing package and file path
    ///
    /// This is used when loading existing packages from the repository.
    ///
    /// # Arguments
    ///
    /// * `package` - The loaded package
    /// * `file_path` - The path where the package file is stored
    #[must_use]
    pub fn from_existing(package: Package, file_path: PathBuf) -> Self {
        Self {
            package,
            file_path,
            is_new: false,
        }
    }

    /// Get a reference to the package content
    #[must_use]
    pub fn package(&self) -> &Package {
        &self.package
    }

    /// Get a mutable reference to the package content
    pub fn package_mut(&mut self) -> &mut Package {
        &mut self.package
    }

    /// Get the file path where the package is/should be stored
    #[must_use]
    pub fn file_path(&self) -> &std::path::Path {
        &self.file_path
    }

    /// Whether this is a new package (true) or existing (false)
    #[must_use]
    pub fn is_new(&self) -> bool {
        self.is_new
    }

    /// Consume the `GetPackage` and return the inner `Package`
    #[must_use]
    pub fn into_package(self) -> Package {
        self.package
    }
}

/// A dotfile mapping from repo source to deployment target.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DotfileEntry {
    source: String,
    target: String,
}

impl DotfileEntry {
    /// Create a new dotfile entry with source and target paths.
    pub fn new(source: impl Into<String>, target: impl Into<String>) -> Self {
        Self {
            source: source.into(),
            target: target.into(),
        }
    }

    /// Get the source path (relative path within the dotfiles repository).
    pub fn source(&self) -> &str {
        &self.source
    }

    /// Get the target path (deployment destination, may use `~` for home directory).
    pub fn target(&self) -> &str {
        &self.target
    }
}

/// Core package entity representing a package definition
///
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Package {
    /// Package name
    pub(crate) name: String,

    /// Optional homepage URL
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) homepage: Option<String>,

    /// Optional package description
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) description: Option<String>,

    /// Dotfile mappings (source → target); applies regardless of environment
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) dotfiles: Vec<DotfileEntry>,

    /// Optional note displayed to the user after a fresh install
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) post_install_note: Option<String>,

    /// Map of environment configurations
    #[serde(default)]
    pub(crate) environments: HashMap<String, EnvironmentConfig>,

    /// Path to the package file (not serialized/deserialized)
    #[serde(skip)]
    pub(crate) path: PathBuf,

    /// Raw YAML content for validation (e.g., unknown field detection).
    /// Set after deserialization, not serialized. Same pattern as `path`.
    #[serde(skip)]
    pub(crate) raw_yaml: String,
}

/// Configuration for a specific environment
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EnvironmentConfig {
    /// Command to install the package
    pub(crate) install: String,

    /// Optional command to check if the package is already installed
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) check: Option<String>,

    /// Optional command to audit the package's installation sources
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) audit: Option<String>,

    /// Dependencies that must be installed before this package
    #[serde(default)]
    pub(crate) dependencies: Vec<String>,

    /// Soft dependencies that are installed after this package but don't cascade failure
    ///
    /// Unlike `dependencies`, a failed recommend does not cause the parent package to fail.
    /// Recommends are installed sequentially after the parent succeeds, with individual
    /// success/failure reported via events.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) recommends: Vec<String>,
}

impl EnvironmentConfig {
    /// Create a new environment configuration
    #[must_use]
    pub fn new(
        install: String,
        check: Option<String>,
        audit: Option<String>,
        dependencies: Vec<String>,
        recommends: Vec<String>,
    ) -> Self {
        Self {
            install,
            check,
            audit,
            dependencies,
            recommends,
        }
    }

    /// Get the install command for this environment
    #[must_use]
    pub fn install(&self) -> &str {
        &self.install
    }

    /// Get the optional check command for this environment
    #[must_use]
    pub fn check(&self) -> Option<&str> {
        self.check.as_deref()
    }

    /// Get the optional audit command for this environment
    #[must_use]
    pub fn audit(&self) -> Option<&str> {
        self.audit.as_deref()
    }

    /// Get the list of dependencies for this environment
    #[must_use]
    pub fn dependencies(&self) -> &[String] {
        &self.dependencies
    }

    /// Get the list of recommended (soft) dependencies for this environment
    #[must_use]
    pub fn recommends(&self) -> &[String] {
        &self.recommends
    }
}

impl Package {
    /// Create a new package with the specified attributes. See `PackageBuilder`.
    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        name: String,
        homepage: Option<String>,
        description: Option<String>,
        dotfiles: Vec<DotfileEntry>,
        post_install_note: Option<String>,
        environments: HashMap<String, EnvironmentConfig>,
        path: PathBuf,
    ) -> Self {
        Self {
            name,
            homepage,
            description,
            dotfiles,
            post_install_note,
            environments,
            path,
            raw_yaml: String::new(),
        }
    }

    /// Create a basic package template
    ///
    /// Creates a minimal package template suitable for new packages.
    /// The template includes basic metadata and a placeholder environment.
    ///
    /// # Arguments
    ///
    /// * `name` - The package name
    #[must_use]
    pub fn new_template(name: &str) -> Self {
        let mut environments = HashMap::new();
        environments.insert(
            "default".to_string(),
            EnvironmentConfig {
                install: format!("# TODO: Add install command for {name}"),
                check: Some(format!("# TODO: Add check command for {name}")),
                audit: None,
                dependencies: Vec::new(),
                recommends: Vec::new(),
            },
        );

        Self {
            name: name.to_string(),
            homepage: None,
            description: None,
            dotfiles: Vec::new(),
            post_install_note: None,
            environments,
            path: PathBuf::new(), // Will be set by GetPackage::new
            raw_yaml: String::new(),
        }
    }

    /// Get the package name
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Get the optional homepage URL
    #[must_use]
    pub fn homepage(&self) -> Option<&str> {
        self.homepage.as_deref()
    }

    /// Get the optional package description
    #[must_use]
    pub fn description(&self) -> Option<&str> {
        self.description.as_deref()
    }

    /// Get the list of dotfile mappings for this package
    #[must_use]
    pub fn dotfiles(&self) -> &[DotfileEntry] {
        &self.dotfiles
    }

    /// Add a dotfile mapping to this package.
    ///
    /// Skips the entry if a dotfile with the same target already exists,
    /// preventing duplicate entries from repeated `track-dotfile` calls.
    pub fn add_dotfile(&mut self, entry: DotfileEntry) {
        let already_tracked = self
            .dotfiles
            .iter()
            .any(|existing| existing.target() == entry.target());
        if !already_tracked {
            self.dotfiles.push(entry);
        }
    }

    /// Get the optional post-install note for this package
    #[must_use]
    pub fn post_install_note(&self) -> Option<&str> {
        self.post_install_note.as_deref()
    }

    /// Get the environment configurations
    #[must_use]
    pub fn environments(&self) -> &HashMap<String, EnvironmentConfig> {
        &self.environments
    }

    /// Get the package file path
    #[must_use]
    pub fn path(&self) -> &PathBuf {
        &self.path
    }
}

#[cfg(test)]
mod package_tests {
    use std::path::PathBuf;

    use builder::PackageBuilder;

    use crate::package::port::PackageError;

    use super::*;

    #[test]
    fn test_create_package_node() {
        let package = PackageBuilder::default()
            .name("test-package")
            .environment("test-env", |b| b.install("test install"))
            .build();

        assert_eq!(package.name, "test-package");
        assert_eq!(package.environments.len(), 1);
        assert_eq!(
            package.environments.get("test-env").unwrap().install,
            "test install"
        );
    }

    #[test]
    fn test_create_package_with_metadata() {
        let package = PackageBuilder::default()
            .name("test-package")
            .homepage("https://example.com")
            .description("Test package description")
            .environment("test-env", |b| b.install("test install"))
            .build();

        assert_eq!(package.name, "test-package");
        assert_eq!(package.homepage, Some("https://example.com".to_string()));
        assert_eq!(
            package.description,
            Some("Test package description".to_string())
        );
        assert_eq!(package.environments.len(), 1);
    }

    #[test]
    fn test_package_not_found_error_contains_context() {
        let error = PackageError::PackageNotFound {
            name: "test-package".to_string(),
            packages_path: PathBuf::from("/home/user/.config/selfie/packages"),
            files_examined: 15,
            search_patterns: vec![
                "test-package.yml".to_string(),
                "test-package.yaml".to_string(),
            ],
        };

        // Test that the error message contains the package name and path
        let error_message = error.to_string();
        assert!(error_message.contains("test-package"));
        assert!(error_message.contains("/home/user/.config/selfie/packages"));

        // Test that context fields are accessible for debugging
        match error {
            PackageError::PackageNotFound {
                name,
                packages_path,
                files_examined,
                search_patterns,
            } => {
                assert_eq!(name, "test-package");
                assert_eq!(
                    packages_path,
                    PathBuf::from("/home/user/.config/selfie/packages")
                );
                assert_eq!(files_examined, 15);
                assert_eq!(
                    search_patterns,
                    vec!["test-package.yml", "test-package.yaml"]
                );
            }
            _ => panic!("Expected PackageNotFound error"),
        }
    }

    #[test]
    fn test_multiple_packages_found_error_contains_conflicting_paths() {
        let conflicting_paths = vec![
            PathBuf::from("/packages/test-package.yml"),
            PathBuf::from("/packages/test-package.yaml"),
        ];

        let error = PackageError::MultiplePackagesFound {
            name: "test-package".to_string(),
            packages_path: PathBuf::from("/packages"),
            conflicting_paths: conflicting_paths.clone(),
            files_examined: 10,
            search_patterns: vec![
                "test-package.yml".to_string(),
                "test-package.yaml".to_string(),
            ],
        };

        // Test that context information is preserved
        match error {
            PackageError::MultiplePackagesFound {
                name,
                conflicting_paths: paths,
                files_examined,
                search_patterns,
                ..
            } => {
                assert_eq!(name, "test-package");
                assert_eq!(paths, conflicting_paths);
                assert_eq!(paths.len(), 2);
                assert_eq!(files_examined, 10);
                assert_eq!(search_patterns.len(), 2);
            }
            _ => panic!("Expected MultiplePackagesFound error"),
        }
    }

    #[test]
    fn test_environment_not_found_error_provides_suggestions() {
        let available_environments = vec![
            "macos".to_string(),
            "linux".to_string(),
            "windows".to_string(),
        ];

        let error = PackageError::EnvironmentNotFound {
            package_name: "test-package".to_string(),
            environment: "freebsd".to_string(),
            available_environments: available_environments.clone(),
            package_file: PathBuf::from("/packages/test-package.yml"),
        };

        // Test error message content
        let error_message = error.to_string();
        assert!(error_message.contains("freebsd"));
        assert!(error_message.contains("test-package"));

        // Test that available environments are accessible for user suggestions
        match error {
            PackageError::EnvironmentNotFound {
                package_name,
                environment,
                available_environments: envs,
                package_file,
            } => {
                assert_eq!(package_name, "test-package");
                assert_eq!(environment, "freebsd");
                assert_eq!(envs, available_environments);
                assert!(envs.contains(&"macos".to_string()));
                assert!(envs.contains(&"linux".to_string()));
                assert!(envs.contains(&"windows".to_string()));
                assert_eq!(package_file, PathBuf::from("/packages/test-package.yml"));
            }
            _ => panic!("Expected EnvironmentNotFound error"),
        }
    }

    #[test]
    fn test_no_check_command_error_shows_alternatives() {
        let other_envs_with_check = vec!["linux".to_string(), "windows".to_string()];

        let error = PackageError::NoCheckCommand {
            package_name: "test-package".to_string(),
            environment: "macos".to_string(),
            package_file: PathBuf::from("/packages/test-package.yml"),
            other_envs_with_check: other_envs_with_check.clone(),
        };

        // Test error message
        let error_message = error.to_string();
        assert!(error_message.contains("macos"));
        assert!(error_message.contains("test-package"));

        // Test that alternative environments are available for suggestions
        match error {
            PackageError::NoCheckCommand {
                package_name,
                environment,
                package_file,
                other_envs_with_check: envs,
            } => {
                assert_eq!(package_name, "test-package");
                assert_eq!(environment, "macos");
                assert_eq!(package_file, PathBuf::from("/packages/test-package.yml"));
                assert_eq!(envs, other_envs_with_check);
                assert!(envs.contains(&"linux".to_string()));
                assert!(envs.contains(&"windows".to_string()));
            }
            _ => panic!("Expected NoCheckCommand error"),
        }
    }

    #[test]
    fn test_parse_error_contains_file_metadata() {
        use crate::package::port::PackageParseError;
        use std::sync::Arc;

        // Create a realistic parse error
        let parse_error = PackageParseError::YamlParse {
            package_path: PathBuf::from("/packages/broken.yml"),
            source: Arc::new(
                serde_saphyr::from_str::<serde_json::Value>("invalid: yaml: [unclosed")
                    .unwrap_err(),
            ),
        };

        let error = PackageError::ParseError {
            name: "broken-package".to_string(),
            packages_path: PathBuf::from("/packages"),
            failed_file: PathBuf::from("/packages/broken.yml"),
            file_size_bytes: 1024,
            source: parse_error,
        };

        // Test that file context is available
        match error {
            PackageError::ParseError {
                name,
                packages_path,
                failed_file,
                file_size_bytes,
                source,
            } => {
                assert_eq!(name, "broken-package");
                assert_eq!(packages_path, PathBuf::from("/packages"));
                assert_eq!(failed_file, PathBuf::from("/packages/broken.yml"));
                assert_eq!(file_size_bytes, 1024);

                // Verify the parse error is preserved
                match source {
                    PackageParseError::YamlParse { package_path, .. } => {
                        assert_eq!(package_path, PathBuf::from("/packages/broken.yml"));
                    }
                    _ => panic!("Expected YamlParse error"),
                }
            }
            _ => panic!("Expected ParseError"),
        }
    }

    #[test]
    fn test_error_context_can_be_extracted_for_debugging() {
        let error = PackageError::PackageNotFound {
            name: "debug-package".to_string(),
            packages_path: PathBuf::from("/debug/packages"),
            files_examined: 42,
            search_patterns: vec!["debug-package.yml".to_string()],
        };

        // Demonstrate how to extract context for debugging/logging
        let debug_info = match &error {
            PackageError::PackageNotFound {
                name,
                packages_path,
                files_examined,
                search_patterns,
            } => {
                format!(
                    "Package '{}' not found in '{}' after examining {} files with patterns: {:?}",
                    name,
                    packages_path.display(),
                    files_examined,
                    search_patterns
                )
            }
            _ => panic!("Expected PackageNotFound error"),
        };

        assert!(debug_info.contains("debug-package"));
        assert!(debug_info.contains("42 files"));
        assert!(debug_info.contains("debug-package.yml"));
    }

    #[test]
    fn test_error_debug_output_includes_all_context() {
        let error = PackageError::MultiplePackagesFound {
            name: "test-multi".to_string(),
            packages_path: PathBuf::from("/test"),
            conflicting_paths: vec![
                PathBuf::from("/test/test-multi.yml"),
                PathBuf::from("/test/test-multi.yaml"),
            ],
            files_examined: 5,
            search_patterns: vec!["test-multi.yml".to_string(), "test-multi.yaml".to_string()],
        };

        let debug_output = format!("{error:?}");

        // Verify that all context fields appear in debug output
        assert!(debug_output.contains("test-multi"));
        assert!(debug_output.contains("files_examined: 5"));
        assert!(debug_output.contains("search_patterns"));
        assert!(debug_output.contains("conflicting_paths"));
    }
}
