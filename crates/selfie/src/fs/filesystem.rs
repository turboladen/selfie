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

use crate::fs::target::TargetPath;

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
    /// This applies to `path` **as given**: a resolved path names the link's
    /// destination, so the link is never seen and the guarantee is gone. That is what
    /// the [`TargetPath`] parameter is for -- nothing can resolve one.
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
    fn write_file_private(&self, path: &TargetPath, data: &[u8]) -> Result<(), FileSystemError>;

    /// Write data to a file, refusing a symlink at the final component
    ///
    /// Behaves like [`write_file`](FileSystem::write_file) for an ordinary target --
    /// parent directories are created, an existing file is truncated and overwritten,
    /// and its mode is left alone -- except that a symlink at the **final component**
    /// of `path` is refused with [`FileSystemError::SymlinkedTarget`] rather than
    /// written through. Nothing is written in that case: neither the link nor what it
    /// points at is modified, and a dangling link's destination is not created.
    ///
    /// Intended for deploy targets. A target names a path the user asked selfie to
    /// manage; writing through a link there sends the content wherever the link
    /// points, which may be somewhere chosen by whoever planted it.
    ///
    /// This is deliberately **not** [`write_file_private`](FileSystem::write_file_private),
    /// and the two differ in more than one way. Neither writes *through* a link, but
    /// that method **replaces** the link and succeeds, where this one **refuses** and
    /// returns an error the caller has to handle. It also makes the file owner-only,
    /// which is right for a credential and wrong for an ordinary dotfile.
    ///
    /// # Symlinks
    ///
    /// The refusal applies to `path` **as given**. A resolved path names the link's
    /// destination, so this method would be handed an ordinary file and write to it;
    /// [`TargetPath`] is what keeps one from arriving here.
    ///
    /// Symlinked **parent** directories are still followed, so a planted directory
    /// symlink can still redirect where the file lands. Same limitation as
    /// [`write_file_private`](FileSystem::write_file_private).
    ///
    /// # Strength of the guarantee
    ///
    /// On Unix a link planted concurrently is refused just the same — there is no
    /// interval between deciding and writing. On other platforms the check precedes
    /// the write, so a concurrent planter can win: a mitigation there, not a
    /// guarantee.
    ///
    /// # Errors
    ///
    /// Returns [`FileSystemError::SymlinkedTarget`] if the final component is a
    /// symlink, or [`FileSystemError`] if:
    /// - The parent directory cannot be created
    /// - Permission is denied to write to the file or directory
    /// - Any other IO error occurs during writing
    ///
    /// The refusal is exact; its *classification* is not quite. A link deleted
    /// between the failed write and the report surfaces as an `IoError` rather than
    /// `SymlinkedTarget`. Nothing was written either way — only the wording differs.
    fn write_file_no_follow(&self, path: &TargetPath, data: &[u8]) -> Result<(), FileSystemError>;

    /// The refusal [`write_file_no_follow`](FileSystem::write_file_no_follow) would
    /// give for `path`, if it would refuse
    ///
    /// `Some` exactly when the final component is a symlink, carrying the same
    /// [`SymlinkedTarget`](FileSystemError::SymlinkedTarget) that method would return
    /// -- including its `None` destination when the link itself cannot be read, which
    /// is why this answers with the error rather than with the destination. A caller
    /// asking "would this be refused, and what do I tell the user" gets one answer to
    /// both questions, worded identically to the real thing.
    ///
    /// For **describing** a path without writing to it: previewing what an apply
    /// would refuse, or reporting it before asking the user a question that could not
    /// be honored. Advisory and inherently racy -- the answer can be stale by the
    /// time a caller acts on it.
    ///
    /// Never use it to decide whether a write is safe.
    /// [`write_file_no_follow`](FileSystem::write_file_no_follow) refuses on its own,
    /// and checking here as well would reintroduce the race that method avoids. This
    /// may move *when the user is told* and *whether a deployment is recorded*, never
    /// whether the write happens — a stale answer omits something the next run
    /// re-evaluates.
    fn symlink_refusal(&self, path: &TargetPath) -> Option<FileSystemError>;

    /// [`FileSystemError::IrregularTarget`] if `path` resolves to something that
    /// is neither absent nor a regular file
    ///
    /// A fifo, socket, device node or directory. `None` for a regular file, for a
    /// path that does not exist, and for a **symlink to a regular file** — a
    /// symlink is [`symlink_refusal`](FileSystem::symlink_refusal)'s question, and
    /// answering it here too would report one thing two ways.
    ///
    /// Must answer for what an `open` of `path` would land on, so a symlink to a
    /// fifo is a fifo.
    ///
    /// **Call this before every read of a path selfie does not control.** A write
    /// is protected either way — [`write_file_no_follow`](FileSystem::write_file_no_follow)
    /// re-checks the descriptor it opened — but nothing does that for a read, and
    /// opening a fifo to read blocks exactly as opening it to write does. Removing
    /// this from a read path restores an indefinite hang.
    ///
    /// Advisory for writes: stat-then-act is racy, so a fifo swapped in afterwards
    /// is caught by the writer rather than here.
    fn irregular_target_refusal(&self, path: &TargetPath) -> Option<FileSystemError>;

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
    /// Symlinks are followed, so this reports on the file the path resolves to --
    /// which is why it takes a [`TargetPath`]. Handed an already-resolved path it
    /// would report on a link's destination and call the target private, and the
    /// caller would then skip the write that replaces the link.
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
    fn is_owner_only(&self, path: &TargetPath) -> Result<bool, FileSystemError>;

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

    /// A write was refused because the final path component is a symlink
    ///
    /// Kept distinct from [`IoError`](FileSystemError::IoError) because it is the
    /// one outcome here that is a deliberate refusal rather than something going
    /// wrong. Callers report it differently, and having it as a variant means they
    /// do not have to inspect an errno -- which would put a platform detail in a
    /// layer this port exists to keep it out of.
    ///
    /// `points_to` is optional because reading the link is a second syscall that
    /// can fail on its own; the refusal still stands when it does.
    #[error(
        "{}: target is a symlink{} and selfie will not write through it",
        .path.display(),
        .points_to.as_deref().map_or(String::new(), |dest| format!(" to '{}'", dest.display()))
    )]
    SymlinkedTarget {
        path: PathBuf,
        points_to: Option<PathBuf>,
    },

    /// A target that is neither absent nor a regular file
    ///
    /// A fifo, socket, device node or directory. Refused rather than written to,
    /// and refused before it is *read*: opening a fifo blocks until the other end
    /// is opened, so `selfie apply` hung indefinitely on one and `command_timeout`
    /// did not bound it — that governs provider commands, not filesystem calls
    ///
    /// `kind` names what was found, because the remedy differs: a leftover socket
    /// is deleted, a device node in `/dev` means the target path is wrong.
    ///
    /// Says "resolves to" rather than "is": this is answered with a *following*
    /// stat, so it covers a symlink pointing at a fifo as well as a bare one. A
    /// plain symlink is reported as [`SymlinkedTarget`](Self::SymlinkedTarget)
    /// instead — that question is asked with a non-following stat, and the two
    /// must not be conflated.
    #[error("{}: target resolves to a {kind} and selfie will not write to it", .path.display())]
    IrregularTarget { path: PathBuf, kind: &'static str },
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

    /// Set up a mock for writing a file selfie owns
    ///
    /// Configures the mock to succeed when writing to the specified path.
    ///
    /// Matches on the path *inside* the [`TargetPath`] rather than on the
    /// wrapper, because a caller has to mint one and the mint is not the thing
    /// under test.
    ///
    /// # Arguments
    ///
    /// * `path` - Path where the write should succeed
    pub fn mock_write_file_no_follow<P>(&mut self, path: P)
    where
        PathBuf: From<P>,
    {
        let path_buf = PathBuf::from(path);
        self.expect_write_file_no_follow()
            .withf(move |target, _| target.path() == path_buf)
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
