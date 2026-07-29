//! File system abstraction layer
//!
//! This module provides a trait-based abstraction for file system operations
//! to enable testing and different implementations. It follows the Hexagonal
//! Architecture pattern by defining a port for file system interactions.

use std::{
    io,
    path::{Path, PathBuf},
    sync::Arc,
};

use thiserror::Error;

/// Port for file system operations (Hexagonal Architecture)
///
/// This trait abstracts file system operations to allow for different implementations
/// (real file system, in-memory for testing, etc.) and to enable comprehensive testing
/// through mocking. All file system interactions in the selfie library go through
/// this abstraction.
#[cfg_attr(feature = "with_mocks", mockall::automock)]
pub trait FileSystem: Send + Sync {
    /// Read a file and return its contents as a string
    ///
    /// Reads the entire file content and returns it as a UTF-8 string.
    /// The file is read synchronously and the entire content is loaded into memory.
    ///
    /// # Arguments
    ///
    /// * `path` - Path to the file to read
    ///
    /// # Errors
    ///
    /// Returns [`FileSystemError`] if:
    /// - The file does not exist
    /// - Permission is denied to read the file
    /// - The file content is not valid UTF-8
    /// - Any other IO error occurs during reading
    fn read_file(&self, path: &Path) -> Result<String, FileSystemError>;

    /// Read a file and return its raw bytes
    ///
    /// Unlike [`read_file`](FileSystem::read_file), imposes no encoding
    /// requirement. Use this wherever the content is compared or written rather
    /// than displayed — secret-bearing dotfile content is not guaranteed to be
    /// UTF-8, and decoding it lossily before a comparison would report two
    /// different files as identical.
    ///
    /// # Arguments
    ///
    /// * `path` - Path to the file to read
    ///
    /// # Errors
    ///
    /// Returns [`FileSystemError`] if:
    /// - The file does not exist
    /// - Permission is denied to read the file
    /// - Any other IO error occurs during reading
    fn read_file_bytes(&self, path: &Path) -> Result<Vec<u8>, FileSystemError>;

    /// Write data to a file
    ///
    /// Writes the provided data to the specified file path, creating the file
    /// if it doesn't exist or overwriting it if it does. Creates any necessary
    /// parent directories.
    ///
    /// # Arguments
    ///
    /// * `path` - Path where the file should be written
    /// * `data` - Data to write to the file
    ///
    /// # Errors
    ///
    /// Returns [`FileSystemError`] if:
    /// - Permission is denied to write to the file or directory
    /// - The parent directory cannot be created
    /// - Any other IO error occurs during writing
    fn write_file(&self, path: &Path, data: &[u8]) -> Result<(), FileSystemError>;

    /// Write data to a file readable only by its owner, replacing it atomically
    ///
    /// Unlike [`write_file`](FileSystem::write_file), this creates the file with
    /// owner-only permissions from the outset (on Unix -- see the platform notes
    /// below) and puts it in place with a rename, so there is no window in which the
    /// content is world-readable, no partial write if interrupted, and no inheriting
    /// of a laxer mode from an existing file.
    ///
    /// Intended for secret-bearing content.
    ///
    /// Creates parent directories as needed, but only the *file* is owner-only:
    /// created directories get the usual `0o777 & !umask`, so a directory made by this
    /// call is typically world-listable. The content is protected; the fact that the
    /// file exists is not.
    ///
    /// # Symlinks
    ///
    /// A symlink at the **final component** of `path` is replaced rather than written
    /// through. Symlinked **parent** directories are still followed, so a planted
    /// directory symlink can still redirect where the file lands.
    ///
    /// This applies to `path` **as given**, so a caller that resolves the path first
    /// can forfeit it. The precise rule, for callers going through
    /// [`expand_path`](FileSystem::expand_path): the guarantee survives exactly when
    /// `expand_path` on the *full* path fails.
    ///
    /// `expand_path` canonicalizes, which only succeeds for a path that already
    /// exists. So a symlink that **resolves** is followed by the caller, and this
    /// method receives the destination rather than the symlink -- the guarantee is
    /// gone. A **dangling** symlink makes canonicalization fail with `ENOENT`; a
    /// caller that then falls back to resolving only the parent, or to the raw path,
    /// hands over an unresolved final component and the guarantee **holds**.
    ///
    /// Do not read this as "canonicalizing callers always forfeit it" -- that is too
    /// pessimistic -- nor as "it is forfeited whenever a symlink is planted", which is
    /// too optimistic in the other direction. It turns on whether the full path
    /// resolved.
    ///
    /// # Platform and metadata notes
    ///
    /// Because the file is replaced rather than modified, the new file does not
    /// inherit the old one's extended attributes, POSIX ACLs, or SELinux label.
    ///
    /// On Unix the mode is `0o600` masked by the process umask, so it may be more
    /// restrictive but never more permissive. On Windows the atomic replace still
    /// applies, but owner-only permissions are best-effort -- the file inherits the
    /// parent directory's ACL -- and the replace additionally fails if the target is
    /// open in another process. On any other platform there is **no owner-only
    /// guarantee at all**: the call may well succeed and simply create the file with
    /// default permissions. Do not treat a non-Unix, non-Windows target as fail-safe.
    ///
    /// # Arguments
    ///
    /// * `path` - Path where the file should be written
    /// * `data` - Data to write to the file
    ///
    /// # Errors
    ///
    /// Returns [`FileSystemError`] if:
    /// - The parent directory cannot be created
    /// - The temporary file cannot be created or written
    /// - Flushing the temporary file to disk fails, which can happen after the write
    ///   itself succeeded (for example `ENOSPC` surfacing only at flush time)
    /// - The rename into place fails
    ///
    /// Note this differs from [`write_file`](FileSystem::write_file), which can still
    /// succeed on an existing file inside a read-only directory; an atomic replace
    /// cannot, because it must create a sibling first.
    fn write_file_private(&self, path: &Path, data: &[u8]) -> Result<(), FileSystemError>;

    /// Whether a file is readable only by its owner
    ///
    /// Companion to [`write_file_private`](FileSystem::write_file_private), for
    /// deciding whether an existing file already meets the standard that method
    /// establishes. Secret-bearing content that is already correct still has to
    /// be checked: content and permissions are independent, and a target whose
    /// bytes happen to match may still be world-readable.
    ///
    /// # Platform notes
    ///
    /// On Unix this is exact: true when no group or other permission bit is set.
    /// Symlinks are followed, so this reports on the file the path resolves to.
    ///
    /// On every other platform this returns `true`, because there are no Unix
    /// permission bits to inspect and nothing this method could meaningfully
    /// report or a caller meaningfully fix. Callers use it to decide whether to
    /// tighten, so `true` — "nothing to do" — is the correct answer there, not a
    /// claim that the file is private.
    ///
    /// # Errors
    ///
    /// Returns [`FileSystemError`] if the file's metadata cannot be read.
    fn is_owner_only(&self, path: &Path) -> Result<bool, FileSystemError>;

    /// Remove a file from the file system
    ///
    /// Deletes the file at the specified path. This operation is irreversible.
    ///
    /// # Arguments
    ///
    /// * `path` - Path to the file to remove
    ///
    /// # Errors
    ///
    /// Returns [`FileSystemError`] if:
    /// - The file does not exist
    /// - Permission is denied to delete the file
    /// - The path points to a directory instead of a file
    /// - Any other IO error occurs during deletion
    fn remove_file(&self, path: &Path) -> Result<(), FileSystemError>;

    /// Check if a path exists
    ///
    /// Tests whether the specified path exists in the file system.
    /// This works for both files and directories.
    ///
    /// # Arguments
    ///
    /// * `path` - Path to check for existence
    ///
    /// # Returns
    ///
    /// `true` if the path exists, `false` otherwise
    fn path_exists(&self, path: &Path) -> bool;

    /// Expand a path with shell-like expansions
    ///
    /// Performs path expansion including tilde (~) expansion to the user's
    /// home directory and other shell-like expansions. This is useful for
    /// handling user-provided paths in configuration files.
    /// Expand path with tilde (~) and environment variables
    ///
    /// # Arguments
    ///
    /// * `path` - The path to expand
    ///
    /// # Returns
    ///
    /// The expanded path
    ///
    /// # Errors
    ///
    /// Returns [`FileSystemError`] if:
    /// - The home directory cannot be determined for ~ expansion
    /// - Environment variable expansion fails
    /// - The expanded path contains invalid characters
    /// - Path expansion fails for any other reason
    fn expand_path(&self, path: &Path) -> Result<PathBuf, FileSystemError>;

    /// List the contents of a directory
    ///
    /// Returns a list of all entries (files and subdirectories) in the specified
    /// directory. The paths returned are absolute paths.
    ///
    /// # Arguments
    ///
    /// * `path` - Directory path to list
    ///
    /// # Errors
    ///
    /// Returns [`FileSystemError`] if:
    /// - The directory does not exist
    /// - Permission is denied to read the directory
    /// - The path is not a directory
    /// - Any other IO error occurs during directory reading
    fn list_directory(&self, path: &Path) -> Result<Vec<PathBuf>, FileSystemError>;

    /// Get the canonical (absolute, resolved) path
    ///
    /// Resolves the path to its canonical form by resolving symbolic links
    /// and relative path components (. and ..). The result is always an
    /// absolute path.
    ///
    /// # Arguments
    ///
    /// * `path` - Path to canonicalize
    ///
    /// # Errors
    ///
    /// Returns [`FileSystemError`] if:
    /// - The path does not exist
    /// - Permission is denied to access path components
    /// - Symbolic link resolution fails
    /// - Any other IO error occurs during canonicalization
    fn canonicalize(&self, path: &Path) -> Result<PathBuf, FileSystemError>;

    /// Get the user's configuration directory
    ///
    /// Returns the standard configuration directory for the current user.
    /// This follows platform conventions (e.g., ~/.config on Unix-like systems).
    ///
    /// # Errors
    ///
    /// Returns [`FileSystemError`] if:
    /// - The user's home directory cannot be determined
    /// - The configuration directory cannot be accessed
    fn config_dir(&self) -> Result<PathBuf, FileSystemError>;
}

/// Errors that can occur during file system operations
///
/// Represents all possible failure modes when interacting with the file system,
/// providing detailed context for debugging and error handling.
#[derive(Error, Debug, Clone)]
pub enum FileSystemError {
    /// General IO error occurred during file system operation
    #[error("IO error: {0}")]
    IoError(Arc<io::Error>),

    /// Home directory could not be determined (needed for path expansion)
    #[error("Home directory not found")]
    HomeDirNotFound,
}

#[cfg(feature = "with_mocks")]
impl MockFileSystem {
    /// # Example Usage in CLI Tests
    ///
    /// The mock methods are now public to enable comprehensive testing
    /// in the CLI layer while avoiding real filesystem operations:
    ///
    /// ```rust
    /// use selfie::fs::MockFileSystem;
    /// use selfie::package::repository::yaml::YamlPackageRepository;
    /// use std::path::PathBuf;
    ///
    /// let mut fs = MockFileSystem::default();
    /// let package_path = PathBuf::from("/test/packages/test-package.yml");
    ///
    /// // Mock successful save operation
    /// fs.mock_write_file(&package_path);
    ///
    /// // Mock successful remove operation
    /// fs.mock_remove_file(&package_path);
    ///
    /// let repo = YamlPackageRepository::new(fs, PathBuf::from("/test/packages"));
    /// // Now test save/remove operations without touching real filesystem
    /// ```
    /// Set up a mock for reading a file with specific content
    ///
    /// Configures the mock to return the specified content when the given
    /// path is read. This is useful for testing configuration loading and
    /// package file parsing.
    ///
    /// # Arguments
    ///
    /// * `path` - Path that should trigger this mock response
    /// * `content` - Content to return when the path is read
    pub fn mock_read_file<P, S>(&mut self, path: P, content: S)
    where
        PathBuf: From<P>,
        S: AsRef<str>,
    {
        let path_buf = PathBuf::from(path);
        let content_string = content.as_ref().to_string();
        self.expect_read_file()
            .with(mockall::predicate::eq(path_buf.clone()))
            .returning(move |_| Ok(content_string.clone()));
    }

    /// Set up a mock for listing directory contents
    ///
    /// Configures the mock to return the specified list of entries when
    /// the given directory is listed. Useful for testing package discovery.
    ///
    /// # Arguments
    ///
    /// * `path` - Directory path that should trigger this mock response
    /// * `entries` - List of entries to return for the directory
    pub fn mock_list_directory<P>(&mut self, path: P, entries: &[P])
    where
        PathBuf: From<P>,
        P: Clone + Sync,
    {
        let dir = PathBuf::from(path);
        let paths: Vec<_> = entries.iter().cloned().map(|e| PathBuf::from(e)).collect();

        self.expect_list_directory()
            .with(mockall::predicate::eq(dir.clone()))
            .returning(move |_| Ok(paths.clone()));
    }

    /// Set up a mock for path existence checking
    ///
    /// Configures the mock to return a specific existence result for
    /// the given path. Useful for testing configuration file discovery.
    ///
    /// # Arguments
    ///
    /// * `path` - Path to mock existence for
    /// * `exists` - Whether the path should be reported as existing
    pub fn mock_path_exists<P>(&mut self, path: P, exists: bool)
    where
        PathBuf: From<P>,
    {
        self.expect_path_exists()
            .with(mockall::predicate::eq(PathBuf::from(path)))
            .returning(move |_| exists);
    }

    /// Set up a mock for getting the configuration directory
    ///
    /// Configures the mock to return a specific configuration directory path.
    /// This is useful for testing configuration loading in different environments.
    ///
    /// # Arguments
    ///
    /// * `path` - Configuration directory path to return
    pub fn mock_config_dir_ok<P>(&mut self, path: P)
    where
        PathBuf: From<P>,
    {
        let p = PathBuf::from(path);
        self.expect_config_dir().return_once(|| Ok(p));
    }

    /// Set up a complete mock configuration file scenario
    ///
    /// Configures the mock to simulate finding and reading a configuration file
    /// in a specific directory. This sets up multiple related mocks for a complete
    /// configuration loading test scenario.
    ///
    /// # Arguments
    ///
    /// * `config_dir` - Directory where the config file should be found
    /// * `config_yaml` - YAML content to return when the config file is read
    pub fn mock_config_file(&mut self, config_dir: &Path, config_yaml: &str) {
        let config_dir_owned = PathBuf::from(config_dir);
        let config_path = config_dir.join("config.yaml");

        self.expect_config_dir()
            .return_once(|| Ok(config_dir_owned));
        self.mock_path_exists(&config_path, true);
        self.mock_read_file(&config_path, config_yaml);

        self.mock_path_exists(&config_dir.join("config.yml"), false);
    }

    /// Set up a mock for writing a file
    ///
    /// Configures the mock to succeed when writing to the specified path.
    /// This is useful for testing package saving operations.
    ///
    /// # Arguments
    ///
    /// * `path` - Path where the write should succeed
    pub fn mock_write_file<P>(&mut self, path: P)
    where
        PathBuf: From<P>,
    {
        let path_buf = PathBuf::from(path);
        self.expect_write_file()
            .with(
                mockall::predicate::eq(path_buf),
                mockall::predicate::always(),
            )
            .returning(|_, _| Ok(()));
    }

    /// Set up a mock for removing a file
    ///
    /// Configures the mock to succeed when removing the specified file.
    /// This is useful for testing package removal operations.
    ///
    /// # Arguments
    ///
    /// * `path` - Path where the file removal should succeed
    pub fn mock_remove_file<P>(&mut self, path: P)
    where
        PathBuf: From<P>,
    {
        let path_buf = PathBuf::from(path);
        self.expect_remove_file()
            .with(mockall::predicate::eq(path_buf))
            .returning(|_| Ok(()));
    }

    /// Set up a mock for path expansion
    ///
    /// Configures the mock to return a specific expanded path when
    /// path expansion is requested. Useful for testing tilde expansion
    /// and other path transformations.
    ///
    /// # Arguments
    ///
    /// * `input` - Input path that should trigger expansion
    /// * `output` - Expanded path to return
    pub fn mock_expand_path<P>(&mut self, input: P, output: P)
    where
        PathBuf: From<P>,
    {
        let input = PathBuf::from(input);
        let output = PathBuf::from(output);

        self.expect_expand_path()
            .with(mockall::predicate::eq(input))
            .return_once(|_| Ok(output));
    }
}
