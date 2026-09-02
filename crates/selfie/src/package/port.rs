//! Package repository port and error types
//!
//! This module defines the core port for package repository operations in the
//! hexagonal architecture. The `PackageRepository` trait abstracts package
//! storage and retrieval, allowing different implementations (YAML files,
//! databases, remote repositories, etc.) while maintaining a consistent interface.

use std::{
    path::{Path, PathBuf},
    sync::Arc,
};
use thiserror::Error;

use crate::{
    fs::filesystem::FileSystemError,
    package::{GetPackage, Package},
};

/// Port for package storage: discovering, loading, saving and removing package
/// definitions.
#[cfg_attr(feature = "with_mocks", mockall::automock)]
pub trait PackageRepository: Send + Sync {
    /// Load a package by name, with every environment configuration, dependency
    /// and piece of metadata it declares.
    ///
    /// The name corresponds to the package file's name without its extension.
    ///
    /// # Errors
    ///
    /// [`PackageRepoError`] if no package by that name exists, if several do, if
    /// the definition is malformed, or if file system access fails.
    fn get_package(&self, name: &str) -> Result<GetPackage, PackageRepoError>;

    /// Whether anything already occupies `path`.
    ///
    /// Asked of the path, not of a package name: a caller about to write to
    /// `path` needs to know whether that write would land on something, and
    /// [`Self::get_package`] cannot answer it, because a name and a path do not
    /// have to agree about what is stored.
    fn path_is_occupied(&self, path: &Path) -> bool;

    /// Read a file a package refers to but does not contain, such as a dotfile
    /// template whose contents validation inspects.
    ///
    /// `relative_path` resolves against `package_path`'s parent directory — the
    /// same base `dotfiles` sources resolve against.
    ///
    /// It lives on the repository because the repository owns access to package
    /// storage; validation works on an already-loaded [`Package`] and has no file
    /// system of its own.
    ///
    /// # Errors
    ///
    /// Returns [`FileSystemError`] if the file does not exist, cannot be read, or
    /// is not valid UTF-8.
    fn read_referenced_file(
        &self,
        package_path: &Path,
        relative_path: &str,
    ) -> Result<String, FileSystemError>;

    /// Load every package definition in the configured directory, returning the
    /// ones that parsed **and** the errors from the ones that did not, so a
    /// caller can carry on past a single bad file.
    ///
    /// # Errors
    ///
    /// [`PackageListError`] if the package directory cannot be accessed or
    /// listed.
    fn list_packages(&self) -> Result<ListPackagesOutput, PackageListError>;

    /// The names of the packages that parse, for callers that do not need the
    /// definitions themselves.
    ///
    /// # Errors
    ///
    /// [`PackageListError`] if the underlying listing fails.
    fn available_packages(&self) -> Result<Vec<String>, PackageListError> {
        let list_packages_output = self.list_packages()?;

        Ok(list_packages_output
            .valid_packages()
            .map(|package| package.name().to_string())
            .collect())
    }

    /// Every package file matching `name`. More than one is how an ambiguous
    /// package name is detected.
    ///
    /// # Errors
    ///
    /// [`PackageListError`] if the package directory cannot be accessed.
    fn find_package_files(&self, name: &str) -> Result<Vec<PathBuf>, PackageListError>;

    /// Serialize a package and write it to `path`.
    ///
    /// # Errors
    ///
    /// [`PackageRepoError`] if the package cannot be serialized to YAML, or if
    /// the target directory does not exist or cannot be written to.
    fn save_package(&self, package: &Package, path: &Path) -> Result<(), PackageRepoError>;

    /// Delete a package's definition file. Irreversible, so confirm with the user
    /// before calling it.
    ///
    /// # Errors
    ///
    /// [`PackageRepoError`] if the package does not exist, permission is denied,
    /// or the deletion otherwise fails.
    fn remove_package(&self, name: &str) -> Result<(), PackageRepoError>;

    /// Every package listing `target_package` as a dependency in any of its
    /// environments, so a caller can say what a removal would break.
    ///
    /// # Errors
    ///
    /// [`PackageRepoError`] if the package listing fails.
    fn find_dependent_packages(
        &self,
        target_package: &str,
    ) -> Result<Vec<Package>, PackageRepoError>;
}

/// Errors that can occur during package repository operations
///
/// This enum represents all possible failures when interacting with the
/// package repository, providing detailed context for debugging and
/// error handling.
#[derive(Error, Debug, Clone)]
pub enum PackageRepoError {
    /// Package-specific error (not found, parse error, etc.)
    #[error(transparent)]
    PackageError(Box<PackageError>),

    /// Package listing operation failed
    #[error(transparent)]
    PackageListError(#[from] PackageListError),

    /// IO error during repository operation
    #[error("IO error: {0}")]
    IoError(#[from] Arc<std::io::Error>),

    /// File system error during repository operation
    #[error("File system error: {0}")]
    FileSystemError(#[from] FileSystemError),

    /// Refused to rewrite a package whose dotfile entries carry unrecognized keys.
    ///
    /// Saving re-serializes from the struct, so a key the struct does not model
    /// is dropped. For a dotfile entry that key is exactly what makes
    /// [`content_source`](crate::package::DotfileEntry::content_source) refuse
    /// the entry, so the rewrite would turn an entry `selfie apply` skips into
    /// one it deploys.
    // The remedy names editing the file rather than a selfie command on purpose:
    // every command that could rewrite the file is refused by this same guard, so
    // pointing at one would send the user back to a tool that just refused them.
    #[error(
        "refusing to rewrite {path}: unrecognized {fields}. \
         Saving would delete the key and silently make the entry deployable. \
         Edit {path} directly to correct or remove the key."
    )]
    UnknownDotfileFields {
        /// The file that would have been rewritten.
        path: PathBuf,
        /// The offending field paths, e.g. `dotfiles[0].var`.
        fields: String,
    },

    /// Refused to rewrite a package file because its top level carries a key the
    /// struct does not model.
    ///
    /// `_dotfiles:` read as an anchor, or a plain misspelling like `configs:`,
    /// is dropped by a rewrite -- taking every entry under it, which is the
    /// largest thing any of these refusals protects.
    #[error(
        "refusing to rewrite {path}: unrecognized {fields}. \
         Saving would delete the key and everything under it. \
         Edit {path} directly to correct or remove the key."
    )]
    UnknownTopLevelFields {
        /// The file that would have been rewritten.
        path: PathBuf,
        /// The offending field paths, e.g. `_dotfiles`.
        fields: String,
    },

    /// Refused to rewrite a package file because its top level could not be read
    /// back, so selfie cannot say whether it carries keys the struct would drop.
    ///
    /// Distinct from [`UnknownTopLevelFields`](Self::UnknownTopLevelFields),
    /// which names keys it found. Here nothing is known, and a rewrite serializes
    /// from the struct regardless -- so the safe direction is to decline the
    /// write, which costs the user a retry, rather than to delete silently.
    // The parse failure ends the message, because it is the part a reader can act
    // on last: the remedy is the same whatever the file turned out to be wrong
    // about. The apply-path warning orders the same string the same way.
    #[error(
        "refusing to rewrite {path}: its top level could not be read back, so any \
         key selfie does not model would be dropped without warning. \
         Edit {path} directly, or simplify it until selfie can read it. \
         The read failed with: {error}"
    )]
    UncheckedTopLevel {
        /// The file that would have been rewritten.
        path: PathBuf,
        /// Why the re-read failed.
        error: String,
    },

    /// Refused to rewrite a package file because an environment carries a key
    /// the struct does not model.
    ///
    /// Separate from [`UnknownDotfileFields`](Self::UnknownDotfileFields)
    /// because the harm differs. No entry becomes deployable here: the key is
    /// a setting of the environment, so re-serializing deletes whatever the user
    /// wrote for it -- `audt: "brew audit myapp"` takes the command with it.
    // Same reasoning as the sibling for naming the file rather than a command:
    // every writer is refused by this guard.
    #[error(
        "refusing to rewrite {path}: unrecognized {fields}. \
         Saving would delete the key and whatever it was set to. \
         Edit {path} directly to correct or remove the key."
    )]
    UnknownEnvironmentFields {
        /// The file that would have been rewritten.
        path: PathBuf,
        /// The offending field paths, e.g. `environments.work.audt`.
        fields: String,
    },

    /// Refused to write a package file because of what is at its path.
    ///
    /// Carries its own wording, which **must never say "target"**. Anything that
    /// is not a refusal keeps the plain
    /// [`FileSystemError`](Self::FileSystemError) passthrough.
    // Both `FileSystemError` refusal variants say "target" in their `Display` —
    // "target is a symlink", "target resolves to a …" — because both were written
    // for a dotfile target, the path selfie deploys out to. A package file is the
    // opposite direction: selfie writing into its own repository. Telling a user
    // their "target" is a symlink when they ran `selfie spec update` names a
    // thing that is not in the sentence they typed.
    // `a_refused_package_path_does_not_call_it_a_target` holds this.
    #[error(
        "refusing to write {path}: {reason}. \
         Remove what is at that path, or choose another name."
    )]
    UnwritablePath {
        /// The package file selfie declined to write.
        path: PathBuf,
        /// What was found there, phrased to follow a colon.
        reason: String,
    },
}

impl PackageRepoError {
    /// Whether this means no package file exists at that name.
    ///
    /// Two answers do: the name matched nothing, and the package directory is
    /// not there at all. Every other error means selfie found something and
    /// could not use it -- a file that will not parse, one it refused to read,
    /// two files claiming the same name -- and each is a file that creating or
    /// templating over would destroy.
    // Lives on the error rather than beside one caller because two commands ask
    // this and a third will. A rule stated twice is a rule that can drift, which
    // is what selfie-vhw4 was about.
    #[must_use]
    pub fn means_no_such_package(&self) -> bool {
        match self {
            Self::PackageError(e) => matches!(**e, PackageError::PackageNotFound { .. }),
            // A missing package directory holds no file to lose, and the write
            // creates it -- `write_file_no_follow` runs `create_dir_all` first.
            Self::PackageListError(PackageListError::PackageDirectoryNotFound(_)) => true,
            _ => false,
        }
    }
}

impl From<PackageError> for PackageRepoError {
    fn from(err: PackageError) -> Self {
        Self::PackageError(Box::new(err))
    }
}

/// Errors that can occur when listing packages
///
/// Represents failures specific to package discovery and directory
/// operations during package listing.
#[derive(Error, Debug, Clone)]
pub enum PackageListError {
    /// IO error occurred while reading the package directory
    #[error("IO error reading package list: {0}")]
    IoError(#[from] Arc<std::io::Error>),

    /// The configured package directory does not exist
    #[error("Directory does not exist: {}", _0.display())]
    PackageDirectoryNotFound(PathBuf),
}

// File names alone -- the directory is already named earlier in the message, so
// repeating the full path for every conflict buries the part that differs.
fn format_conflicting_paths(paths: &[PathBuf]) -> String {
    paths
        .iter()
        .map(|p| {
            p.file_name()
                .unwrap_or(p.as_os_str())
                .to_string_lossy()
                .into_owned()
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// Errors that can occur with specific package operations
///
/// Represents detailed failures when working with individual packages,
/// providing rich context for debugging and user-friendly error messages.
#[derive(Error, Debug, Clone)]
pub enum PackageError {
    /// No package with the specified name could be found
    #[error("Package `{name}` not found in path {}", packages_path.display())]
    #[allow(clippy::doc_link_with_quotes)]
    PackageNotFound {
        name: String,
        packages_path: PathBuf,
        /// Number of files examined during search
        files_examined: usize,
        /// Search patterns used (e.g., ["package.yml", "package.yaml"])
        search_patterns: Vec<String>,
    },

    /// Multiple package files found with the same name, creating ambiguity
    #[error(
        "Multiple packages found with name `{name}` in path {}: {}. Names are compared ignoring \
         case, and `.yml` and `.yaml` name the same package, so these files all claim one name. \
         Rename or remove all but one.",
        packages_path.display(),
        format_conflicting_paths(conflicting_paths)
    )]
    MultiplePackagesFound {
        name: String,
        packages_path: PathBuf,
        /// The conflicting file paths found
        conflicting_paths: Vec<PathBuf>,
        files_examined: usize,
        search_patterns: Vec<String>,
    },

    /// Package definition file exists but could not be parsed
    #[error("Parse error in package `{name}` from {}: {source}", packages_path.display())]
    ParseError {
        name: String,
        packages_path: PathBuf,
        /// The specific file that failed to parse
        failed_file: PathBuf,
        #[source]
        source: PackageParseError,
    },

    /// Package definition file exists but selfie refused to read it
    ///
    /// Separate from [`ParseError`](Self::ParseError) because nothing was
    /// parsed: the file was never opened. Saying "parse error" for a fifo sends
    /// the reader to inspect YAML syntax in a file that has none.
    #[error("Cannot read package `{name}` from {}: {source}", packages_path.display())]
    UnreadableFile {
        name: String,
        packages_path: PathBuf,
        /// The specific file that could not be read
        failed_file: PathBuf,
        #[source]
        source: PackageParseError,
    },

    /// The requested environment is not configured for this package
    #[error("Environment `{environment}` not found in package `{package_name}`")]
    EnvironmentNotFound {
        package_name: String,
        environment: String,
        /// Available environments for suggestions
        available_environments: Vec<String>,
        package_file: PathBuf,
    },

    /// Package environment exists but has no check command configured
    #[error("No check command defined for package `{package_name}` in environment `{environment}`")]
    NoCheckCommand {
        package_name: String,
        environment: String,
        package_file: PathBuf,
        /// Whether other environments have check commands (for suggestions)
        other_envs_with_check: Vec<String>,
    },

    /// A package with the specified name already exists
    #[error("Package `{name}` already exists at {}", file_path.display())]
    PackageAlreadyExists { name: String, file_path: PathBuf },

    /// The write path for a new package is occupied, though the name is free
    #[error(
        "Cannot create `{name}`: {} already exists, though no package answers to that name. \
         Creating would replace that file.",
        file_path.display()
    )]
    PackagePathOccupied { name: String, file_path: PathBuf },

    /// Package environment exists but has no install command configured
    #[error(
        "No install command defined for package `{package_name}` in environment `{environment}`"
    )]
    NoInstallCommand {
        package_name: String,
        environment: String,
        package_file: PathBuf,
        /// Whether other environments have install commands (for suggestions)
        other_envs_with_install: Vec<String>,
    },
}

/// Output from listing packages in the repository
///
/// Contains the results of attempting to load all packages from the repository.
/// This includes both successfully loaded packages and any parse errors that
/// occurred, allowing callers to handle partial failures gracefully.
#[derive(Debug)]
pub struct ListPackagesOutput(pub(crate) Vec<Result<Package, PackageParseError>>);

impl ListPackagesOutput {
    /// Get the total number of packages found (both valid and invalid)
    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Check if no packages were found
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Create a `ListPackagesOutput` from a list of valid packages.
    ///
    /// Useful in tests where you want to construct output without going
    /// through a real repository.
    #[cfg(any(test, feature = "with_mocks"))]
    #[must_use]
    pub fn from_packages(packages: Vec<Package>) -> Self {
        Self(packages.into_iter().map(Ok).collect())
    }

    /// Create a `ListPackagesOutput` from a mix of loaded packages and failures.
    // The field is `pub(crate)`, so without this a test outside the library can
    // only build an output in which nothing failed -- which is exactly the case
    // that cannot catch a caller dropping the failures silently.
    #[cfg(any(test, feature = "with_mocks"))]
    #[must_use]
    pub fn from_results(results: Vec<Result<Package, PackageParseError>>) -> Self {
        Self(results)
    }

    /// Get all package loading results (both successes and failures)
    ///
    /// Returns a slice containing the result of attempting to load each
    /// package file found in the repository.
    pub fn all_results(&self) -> &[Result<Package, PackageParseError>] {
        &self.0
    }

    /// Get an iterator over successfully loaded packages
    ///
    /// Filters out any packages that failed to parse and returns only
    /// the valid package definitions.
    pub fn valid_packages(&self) -> impl Iterator<Item = &Package> {
        self.0.iter().filter_map(|maybe_p| maybe_p.as_ref().ok())
    }

    /// Find a package by name among the ones that parsed.
    ///
    /// `None` covers both "no such package" and "it failed to parse".
    #[must_use]
    pub fn get(&self, package_name: &str) -> Option<&Package> {
        self.0.iter().find_map(|maybe_p| match maybe_p {
            Ok(p) => {
                if p.name() == package_name {
                    Some(p)
                } else {
                    None
                }
            }
            Err(_) => None,
        })
    }

    /// Get an iterator over packages that failed to parse
    ///
    /// Returns parse errors for packages that could not be loaded successfully.
    /// This is useful for debugging configuration issues.
    pub fn invalid_packages(&self) -> impl Iterator<Item = &PackageParseError> {
        self.0.iter().filter_map(|maybe_p| maybe_p.as_ref().err())
    }
}

/// A package file that could not be turned into a package.
///
/// Carries the file it was reading and why. No [`PackageParseKind`]'s own wording
/// names the file, so renderers put the path where their own layout wants it -- a
/// column, a label, a JSON field -- and
/// [`skipped_spec_warning`](crate::package::service::skipped_spec_warning) is the
/// one that puts it in prose.
// Split into a path and a path-free reason so that "the message names the file
// exactly once" holds for every `#[error]` attribute at once rather than for each
// of them separately. The kinds carrying free text -- `Io`, `Unreadable`,
// `Refused` -- can still hold a path at runtime, so nothing building one of them
// may re-tag a failure with the path it was reading.
#[derive(Error, Debug, Clone)]
#[error("{kind}")]
pub struct PackageParseError {
    package_path: PathBuf,
    // `#[source]` as well as interpolated, so a caller walking the chain reaches
    // the `ParseFailure` or the `io::Error` underneath. Every other error in this
    // module carries its payload both ways.
    #[source]
    kind: PackageParseKind,
}

impl PackageParseError {
    /// Report `kind` against the package file at `package_path`.
    #[must_use]
    pub fn new(package_path: impl Into<PathBuf>, kind: PackageParseKind) -> Self {
        Self {
            package_path: package_path.into(),
            kind,
        }
    }

    /// The package file that failed to parse.
    #[must_use]
    pub fn package_path(&self) -> &Path {
        &self.package_path
    }

    /// What went wrong, for a caller that has to tell the cases apart.
    #[must_use]
    pub fn kind(&self) -> &PackageParseKind {
        &self.kind
    }
}

/// Why a package file could not be turned into a package.
///
/// No variant names the file: the path lives on
/// [`PackageParseError`](PackageParseError::package_path), and a renderer that
/// wants it in the sentence asks for it there.
#[derive(Error, Debug, Clone)]
pub enum PackageParseKind {
    /// YAML syntax or structure error in the package file
    #[error("YAML parsing error: {source}")]
    Yaml {
        #[source]
        source: crate::yaml::ParseFailure,
    },

    /// IO error occurred while reading the package file
    #[error("I/O error reading the package file: {source}")]
    Io {
        #[source]
        source: Arc<std::io::Error>,
    },

    /// File system abstraction error during package file access
    #[error("file system error reading the package file: {source}")]
    FileSystem {
        #[source]
        source: Arc<crate::fs::filesystem::FileSystemError>,
    },

    /// The package file is a fifo, socket or device node rather than a regular file
    ///
    /// Reading one blocks until a writer arrives, so it is refused before the
    /// read rather than reported after it.
    #[error(
        "the package file is a {kind}, not a regular file. Replace it with a regular file or remove it from the package directory."
    )]
    IrregularFile { kind: &'static str },

    /// Some other refusal from the filesystem port, worded for a read
    ///
    /// Carries a `reason` rather than the [`FileSystemError`] itself.
    // Every refusal variant's own `Display` names a *target* and says selfie
    // will not **write** through it, having been written for the deploy side.
    // Rendering one here would report a write refusal on a read path.
    //
    // Reached only if `irregular_target_refusal` ever returns something other
    // than `IrregularTarget`. It exists so that growth fails closed with
    // sensible wording rather than falling through to the read.
    #[error("selfie will not read the package file: {reason}")]
    Refused { reason: String },
}

#[cfg(test)]
mod tests {
    use super::*;
    use mockall::predicate::*;

    #[test]
    fn test_mock_find_dependent_packages() {
        // Test that the new find_dependent_packages method can be mocked
        let mut mock_repo = MockPackageRepository::new();

        // Set up expectations
        mock_repo
            .expect_find_dependent_packages()
            .with(eq("target-package"))
            .times(1)
            .returning(|_| {
                use crate::package::Package;

                use std::path::PathBuf;

                let mut env1 = std::collections::HashMap::new();
                env1.insert(
                    "test".to_string(),
                    crate::package::EnvironmentConfig::new(
                        "echo install".to_string(),
                        None,
                        None,
                        vec!["target-package".to_string()],
                        Vec::new(),
                    ),
                );

                let mut env2 = std::collections::HashMap::new();
                env2.insert(
                    "test".to_string(),
                    crate::package::EnvironmentConfig::new(
                        "echo install".to_string(),
                        None,
                        None,
                        vec!["target-package".to_string()],
                        Vec::new(),
                    ),
                );

                Ok(vec![
                    Package::new(
                        "dependent1".to_string(),
                        None,
                        None,
                        Vec::new(),
                        None,
                        env1,
                        PathBuf::from("/test/dependent1.yml"),
                    ),
                    Package::new(
                        "dependent2".to_string(),
                        None,
                        None,
                        Vec::new(),
                        None,
                        env2,
                        PathBuf::from("/test/dependent2.yml"),
                    ),
                ])
            });

        // Call the mocked method
        let result = mock_repo.find_dependent_packages("target-package");

        // Verify the result
        assert!(result.is_ok());
        let dependents = result.unwrap();
        assert_eq!(dependents.len(), 2);

        let names: Vec<String> = dependents.iter().map(|p| p.name().to_string()).collect();
        assert!(names.contains(&"dependent1".to_string()));
        assert!(names.contains(&"dependent2".to_string()));
    }

    #[test]
    fn test_mock_find_dependent_packages_empty() {
        // Test mocking when no dependents are found
        let mut mock_repo = MockPackageRepository::new();

        mock_repo
            .expect_find_dependent_packages()
            .with(eq("standalone-package"))
            .times(1)
            .returning(|_| Ok(vec![]));

        let result = mock_repo.find_dependent_packages("standalone-package");

        assert!(result.is_ok());
        let dependents = result.unwrap();
        assert!(dependents.is_empty());
    }

    #[test]
    fn test_mock_find_dependent_packages_error() {
        // Test mocking error conditions
        let mut mock_repo = MockPackageRepository::new();

        mock_repo
            .expect_find_dependent_packages()
            .with(eq("error-package"))
            .times(1)
            .returning(|_| {
                Err(PackageRepoError::PackageListError(
                    PackageListError::PackageDirectoryNotFound(PathBuf::from("/nonexistent")),
                ))
            });

        let result = mock_repo.find_dependent_packages("error-package");

        assert!(result.is_err());
        match result.unwrap_err() {
            PackageRepoError::PackageListError(PackageListError::PackageDirectoryNotFound(_)) => {
                // Expected error type
            }
            _ => panic!("Expected PackageDirectoryNotFound error"),
        }
    }
}

#[cfg(test)]
mod conflicting_path_tests {
    use super::*;

    // The conflicting file names are the only part of this error a user can act
    // on, and they were captured but never rendered. Asserting the rendered
    // string is what keeps them there.
    #[test]
    fn the_ambiguity_error_names_the_files_and_the_remedy() {
        let rendered = PackageError::MultiplePackagesFound {
            name: "neovim".to_string(),
            packages_path: PathBuf::from("/packages"),
            conflicting_paths: vec![
                PathBuf::from("/packages/Neovim.yml"),
                PathBuf::from("/packages/neovim.yml"),
            ],
            files_examined: 2,
            search_patterns: vec![],
        }
        .to_string();

        assert!(rendered.contains("Neovim.yml"), "got: {rendered}");
        assert!(rendered.contains("neovim.yml"), "got: {rendered}");
        assert!(rendered.contains("Rename"), "no remedy: {rendered}");
        // The directory is stated once; repeating it per file hides the
        // difference between them, which is the whole point of listing them.
        assert_eq!(rendered.matches("/packages/").count(), 0, "got: {rendered}");
    }

    // One error covers two different collisions: names that differ only by
    // case, and `.yml` against `.yaml`. Wording that blames only capitalization
    // sends the reader of this pair looking for a difference that is not there.
    #[test]
    fn the_ambiguity_error_covers_an_extension_collision_too() {
        let rendered = PackageError::MultiplePackagesFound {
            name: "neovim".to_string(),
            packages_path: PathBuf::from("/packages"),
            conflicting_paths: vec![
                PathBuf::from("/packages/neovim.yaml"),
                PathBuf::from("/packages/neovim.yml"),
            ],
            files_examined: 2,
            search_patterns: vec![],
        }
        .to_string();

        assert!(rendered.contains("neovim.yaml"), "got: {rendered}");
        assert!(rendered.contains("neovim.yml"), "got: {rendered}");
        assert!(rendered.contains("`.yml` and `.yaml`"), "got: {rendered}");
    }
}
