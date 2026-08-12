//! Port for file system operations, and the errors they report.

use std::{
    io,
    path::{Path, PathBuf},
    sync::Arc,
};

use thiserror::Error;

use crate::fs::target::TargetPath;

/// Port for file system operations. Every file system interaction in the selfie
/// library goes through it.
///
/// It offers two writers and deliberately no third that follows symlinks, so
/// "selfie never writes through a symlink" is a property of the port rather than
/// of its call sites:
///
/// | | link at final component | mode | atomic |
/// |---|---|---|---|
/// | [`write_file_private`](FileSystem::write_file_private) | replaced | owner-only | yes |
/// | [`write_file_no_follow`](FileSystem::write_file_no_follow) | refused, as an error | left alone | no |
///
/// Secret-bearing content takes the first; everything else takes the second.
/// Neither follows a link at the final component, and both still follow
/// symlinked **parent** directories — a planted directory symlink can redirect
/// where a file lands either way.
#[cfg_attr(feature = "with_mocks", mockall::automock)]
pub trait FileSystem: Send + Sync {
    /// Read a file's contents as a UTF-8 string, all of it into memory.
    ///
    /// # Errors
    ///
    /// [`FileSystemError`] if the file does not exist, permission is denied, the
    /// content is not valid UTF-8, or any other IO error occurs.
    fn read_file(&self, path: &Path) -> Result<String, FileSystemError>;

    /// Read a file's raw bytes, imposing no encoding requirement.
    ///
    /// Use this wherever the content is compared or written rather than
    /// displayed. Secret-bearing dotfile content is not guaranteed to be UTF-8,
    /// and decoding it lossily before a comparison would report two different
    /// files as identical.
    ///
    /// # Errors
    ///
    /// [`FileSystemError`] if the file does not exist, permission is denied, or
    /// any other IO error occurs.
    fn read_file_bytes(&self, path: &Path) -> Result<Vec<u8>, FileSystemError>;

    /// Write a file readable only by its owner, replacing it atomically. The
    /// writer for secret-bearing content.
    ///
    /// Owner-only from the outset and put in place with a rename, so there is no
    /// window in which the content is world-readable, no partial write if
    /// interrupted, and no inheriting of a laxer mode from an existing file. A
    /// symlink at the final component is replaced rather than written through.
    ///
    /// Parent directories are created as needed, but only the *file* is
    /// owner-only: created directories get the usual `0o777 & !umask`. The
    /// content is protected; the fact that it exists is not.
    ///
    /// Because the file is replaced rather than modified, it does not inherit the
    /// old one's extended attributes, POSIX ACLs, or SELinux label.
    ///
    /// # Strength of the guarantee
    ///
    /// On Unix the mode is `0o600` masked by the umask — never more permissive.
    /// On Windows the atomic replace holds but owner-only is best-effort, the
    /// file inheriting the parent directory's ACL, and the replace also fails if
    /// the target is open in another process. On any other platform there is **no
    /// owner-only guarantee at all**: the call may succeed and create the file
    /// with default permissions. Do not treat a non-Unix, non-Windows target as
    /// fail-safe.
    ///
    /// # Errors
    ///
    /// [`FileSystemError`] if the parent directory cannot be created, the
    /// temporary file cannot be created or written, the rename into place fails,
    /// or flushing to disk fails — which can happen after the write itself
    /// succeeded, `ENOSPC` surfacing only at flush time being the usual case.
    ///
    /// Unlike [`write_file_no_follow`](FileSystem::write_file_no_follow), this
    /// cannot succeed on an existing file inside a read-only directory: an atomic
    /// replace must create a sibling first.
    fn write_file_private(&self, path: &TargetPath, data: &[u8]) -> Result<(), FileSystemError>;

    /// Write a file, refusing a symlink at the final component. The ordinary
    /// writer, and the only one for content that is not a credential.
    ///
    /// Parent directories are created, an existing file is truncated and
    /// overwritten, and its mode is left alone. A symlink at the final component
    /// is refused with [`FileSystemError::SymlinkedTarget`]: nothing is written,
    /// neither the link nor what it points at is modified, and a dangling link's
    /// destination is not created.
    ///
    /// For deploy targets and for paths selfie composes inside its own
    /// directories. A target names a path the user asked selfie to manage, so
    /// writing through a link there sends the content wherever the link points —
    /// possibly somewhere chosen by whoever planted it.
    ///
    /// # Durability
    ///
    /// Returns only once the data has been flushed to disk, with a best-effort
    /// attempt at the parent directory. Callers record a successful write as a
    /// deployment, and a record outliving the write it describes turns a lost
    /// deploy into a conflict blamed on the user, so the flush is ordered before
    /// the record rather than left to the state layer.
    ///
    /// # Strength of the guarantee
    ///
    /// Durability orders against a process or kernel crash everywhere, but
    /// against power loss only where the filesystem honors `fsync` — macOS does
    /// not. The directory flush covers only the immediate parent.
    ///
    /// On Unix a link planted concurrently is refused just the same, there being
    /// no interval between deciding and writing. On other platforms the check
    /// precedes the write, so a concurrent planter can win: a mitigation there,
    /// not a guarantee.
    ///
    /// # Errors
    ///
    /// [`FileSystemError::SymlinkedTarget`] if the final component is a symlink,
    /// or [`FileSystemError`] if the parent directory cannot be created,
    /// permission is denied, or any other IO error occurs.
    ///
    /// The refusal is exact; its *classification* is not quite. A link deleted
    /// between the failed write and the report surfaces as an `IoError` rather
    /// than `SymlinkedTarget`. Nothing was written either way.
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

    /// Remove a file. Irreversible.
    ///
    /// # Errors
    ///
    /// [`FileSystemError`] if the file does not exist, permission is denied, the
    /// path is a directory rather than a file, or any other IO error occurs.
    fn remove_file(&self, path: &Path) -> Result<(), FileSystemError>;

    /// Whether `path` exists, as either a file or a directory.
    fn path_exists(&self, path: &Path) -> bool;

    /// Expand `~` and environment variables in a path, for user-provided paths
    /// out of configuration files.
    ///
    /// # Errors
    ///
    /// [`FileSystemError`] if the home directory cannot be determined for `~`,
    /// an environment variable cannot be expanded, or the result contains
    /// invalid characters.
    fn expand_path(&self, path: &Path) -> Result<PathBuf, FileSystemError>;

    /// Every entry in a directory, files and subdirectories alike, as absolute
    /// paths.
    ///
    /// Enumeration order is the platform's and is not sorted.
    ///
    /// # Errors
    ///
    /// [`FileSystemError`] if the directory does not exist, permission is
    /// denied, the path is not a directory, or any other IO error occurs.
    fn list_directory(&self, path: &Path) -> Result<Vec<PathBuf>, FileSystemError>;

    /// Resolve a path to its canonical form: absolute, with symlinks and `.`/`..`
    /// resolved.
    ///
    /// **Never call this on a deploy target.** Resolving a link hands the caller
    /// the destination, so an onward writer sees an ordinary file and every
    /// symlink guarantee in this port is forfeited — and forfeited precisely when
    /// the call *succeeds*. Targets travel as [`TargetPath`], which cannot be
    /// resolved, for that reason.
    ///
    /// # Errors
    ///
    /// [`FileSystemError`] if the path does not exist, permission is denied on
    /// any component, link resolution fails, or any other IO error occurs.
    fn canonicalize(&self, path: &Path) -> Result<PathBuf, FileSystemError>;

    /// The current user's configuration directory, by platform convention —
    /// `~/.config` on Unix-like systems.
    ///
    /// # Errors
    ///
    /// [`FileSystemError`] if the home directory cannot be determined or the
    /// configuration directory cannot be accessed.
    fn config_dir(&self) -> Result<PathBuf, FileSystemError>;
}

/// Every way a file system operation here can fail.
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

    /// A target that is neither absent nor a regular file: a fifo, socket, device
    /// node or directory.
    ///
    /// Refused rather than written to, and refused before it is *read* — opening
    /// a fifo blocks until the other end is opened, and `command_timeout` does not
    /// bound that, governing provider commands rather than filesystem calls.
    ///
    /// `kind` names what was found, because the remedy differs: a leftover socket
    /// is deleted, while a device node in `/dev` means the target path is wrong.
    ///
    /// Says "resolves to" rather than "is" because it is answered with a
    /// *following* stat, so it covers a symlink pointing at a fifo as well as a
    /// bare one. A plain symlink is [`SymlinkedTarget`](Self::SymlinkedTarget)
    /// instead, asked with a non-following stat; do not conflate the two.
    #[error("{}: target resolves to a {kind} and selfie will not write to it", .path.display())]
    IrregularTarget { path: PathBuf, kind: &'static str },
}

/// Helpers that set up common expectations, so library and CLI tests can drive
/// real code paths without touching the disk.
///
/// ```rust
/// use selfie::fs::MockFileSystem;
/// use selfie::package::repository::yaml::YamlPackageRepository;
/// use std::path::PathBuf;
///
/// let mut fs = MockFileSystem::default();
/// let package_path = PathBuf::from("/test/packages/test-package.yml");
///
/// fs.mock_write_file_no_follow(&package_path);
/// fs.mock_remove_file(&package_path);
///
/// let repo = YamlPackageRepository::new(fs, PathBuf::from("/test/packages"));
/// ```
#[cfg(feature = "with_mocks")]
impl MockFileSystem {
    /// Return `content` whenever `path` is read.
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

    /// Return `entries` whenever `path` is listed.
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

    /// Report `path` as existing, or not, according to `exists`.
    pub fn mock_path_exists<P>(&mut self, path: P, exists: bool)
    where
        PathBuf: From<P>,
    {
        self.expect_path_exists()
            .with(mockall::predicate::eq(PathBuf::from(path)))
            .returning(move |_| exists);
    }

    /// Return `path` as the configuration directory.
    pub fn mock_config_dir_ok<P>(&mut self, path: P)
    where
        PathBuf: From<P>,
    {
        let p = PathBuf::from(path);
        self.expect_config_dir().return_once(|| Ok(p));
    }

    /// Set up every expectation for loading a config file: the directory, a
    /// `config.yaml` in it holding `config_yaml`, and no `config.yml` beside it.
    pub fn mock_config_file(&mut self, config_dir: &Path, config_yaml: &str) {
        let config_dir_owned = PathBuf::from(config_dir);
        let config_path = config_dir.join("config.yaml");

        self.expect_config_dir()
            .return_once(|| Ok(config_dir_owned));
        self.mock_path_exists(&config_path, true);
        self.mock_read_file(&config_path, config_yaml);

        self.mock_path_exists(&config_dir.join("config.yml"), false);
    }

    /// Succeed when writing to `path`.
    ///
    /// Matches on the path *inside* the [`TargetPath`] rather than on the
    /// wrapper, because a caller has to mint one and the mint is not the thing
    /// under test.
    pub fn mock_write_file_no_follow<P>(&mut self, path: P)
    where
        PathBuf: From<P>,
    {
        let path_buf = PathBuf::from(path);
        self.expect_write_file_no_follow()
            .withf(move |target, _| target.path() == path_buf)
            .returning(|_, _| Ok(()));
    }

    /// Succeed when removing `path`.
    pub fn mock_remove_file<P>(&mut self, path: P)
    where
        PathBuf: From<P>,
    {
        let path_buf = PathBuf::from(path);
        self.expect_remove_file()
            .with(mockall::predicate::eq(path_buf))
            .returning(|_| Ok(()));
    }

    /// Expand `input` to `output`.
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
