//! Port for file system operations, and the errors they report.

use std::{
    io,
    path::{Path, PathBuf},
    sync::Arc,
};

use thiserror::Error;

use crate::fs::target::TargetPath;

/// What is at a directory path.
///
/// One type for the question selfie asks of every directory it reads — the package
/// directory, the dotfiles directory, the state directory — so no two consumers can
/// answer it differently. See ADR-0005 decision 1.
///
/// [`Unlistable`](DirectoryState::Unlistable) and [`Unknown`](DirectoryState::Unknown)
/// stay separate variants rather than one error a caller inspects, because they mean
/// different things: the first is a directory that may be hiding entries, the second
/// is a path nothing is known about. Conflating them is what let "could not look" read
/// as "nothing there".
#[derive(Debug, Clone)]
pub enum DirectoryState {
    /// A directory. Its entries may still fail to read; that is
    /// [`Unlistable`](DirectoryState::Unlistable).
    Directory,
    /// No directory is at the path, and why not.
    Absent(AbsentReason),
    /// A directory whose entries could not be read, so it may be hiding entries.
    Unlistable(Arc<io::Error>),
    /// The check itself failed. Nothing is known about the path.
    Unknown(Arc<io::Error>),
}

/// Why no directory is at a path.
///
/// Carried because the remedy is not shared: only [`Empty`](AbsentReason::Empty) is
/// fixed by creating the directory. `mkdir -p` fails with "File exists" against a
/// plain file and "No such file or directory" against a dangling link, so a sentence
/// offering it for those is worse than no sentence.
#[derive(Debug, Clone)]
pub enum AbsentReason {
    /// Nothing is at the path. The one reason `mkdir -p` answers.
    Empty,
    /// Something that is not a directory is at the path.
    Occupied {
        /// What is there, for the sentence: `regular file`, `named pipe (fifo)`.
        kind: &'static str,
    },
    /// The final component is a symlink whose destination is not there.
    DanglingSymlink {
        /// Where the link points, when the link itself could be read.
        points_to: Option<PathBuf>,
    },
    /// A component of the path is not a directory: a file, or a symlink whose
    /// destination is not there.
    ParentNotADirectory {
        /// The first such component, found by walking the path's ancestors. The
        /// errno says only that some component is not a directory, never which.
        parent: PathBuf,
    },
}

impl AbsentReason {
    /// What is at the path instead of a directory, as a clause that follows the
    /// directory's name: "does not exist", "is a regular file".
    ///
    /// Shared so a refusal and a warning about the same directory describe it the
    /// same way, and so no caller has to match on the variants to word one.
    #[must_use]
    pub fn clause(&self) -> String {
        match self {
            Self::Empty => "does not exist".to_string(),
            Self::Occupied { kind } => format!("is not a directory, it is a {kind}"),
            Self::DanglingSymlink { points_to } => match points_to {
                Some(destination) => {
                    format!(
                        "is a symlink to nothing: it points at {}",
                        destination.display()
                    )
                }
                // The link read once and would not read again, so the sentence names
                // what is known rather than guessing a destination.
                None => "is a symlink to nothing".to_string(),
            },
            Self::ParentNotADirectory { parent } => {
                format!("is below {}, which is not a directory", parent.display())
            }
        }
    }

    /// The command that would create the directory at `path`, or `None` when no
    /// single command is the remedy.
    ///
    /// Only [`Empty`](Self::Empty) has one. `mkdir -p` fails with "File exists"
    /// against a plain file and "No such file or directory" against a dangling
    /// symlink, so offering it for those sends the user to a command that cannot
    /// work. The path is shell-quoted, because the sentence exists to be pasted.
    #[must_use]
    pub fn remedy(&self, path: &Path) -> Option<String> {
        // `--` ends the option list. Without it a dotfiles directory named `-p` or
        // `-foo` is read by mkdir as options, so the command fails or creates
        // something the user did not ask for.
        match self {
            Self::Empty => Some(format!("Create it with: mkdir -p -- {}", shell_quote(path))),
            Self::Occupied { .. }
            | Self::DanglingSymlink { .. }
            | Self::ParentNotADirectory { .. } => None,
        }
    }
}

/// `path` as shell words that expand to it, for a remedy the user will paste.
///
/// A leading `~/` is left outside the quotes, so the shell still expands it. Pair it
/// with a `--` before the path, which this does not add: a leading dash is not a
/// character quoting protects, so only the option separator ends it.
#[must_use]
pub fn shell_quote(path: &Path) -> String {
    let rendered = path.display().to_string();
    // A tilde arrives here only when the config loader could not expand it, which
    // needs a home directory selfie cannot determine. Quoting it whole offers a
    // command that creates a directory named `~` in the working directory. Only a
    // bare `~` and a leading `~/` are held out, because those are the two the loader
    // expands; `~user` stays quoted, since a command reaching further than selfie
    // does would name a directory selfie will not then use.
    if rendered == "~" {
        return rendered;
    }
    match rendered.strip_prefix("~/") {
        Some(rest) => format!("~/{}", quote_word(rest)),
        None => quote_word(&rendered),
    }
}

/// One shell word, quoted unless every character is one the shell leaves alone.
///
/// Single quotes, with the shell's own escape for an embedded single quote, which is
/// the one character single quotes do not cover.
fn quote_word(word: &str) -> String {
    if !word.is_empty()
        && word
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '.' | '_' | '-'))
    {
        return word.to_string();
    }
    format!("'{}'", word.replace('\'', r"'\''"))
}

impl DirectoryState {
    /// The state a failed listing implies, for a caller that has already listed.
    ///
    /// The repositories list to read their specs, so asking them to classify first
    /// would read the directory twice. This turns the listing they already did into
    /// the same states [`FileSystem::directory_state`] returns.
    ///
    /// One classifier, not two: anything that is not a listing failure is handed to
    /// [`FileSystem::directory_state`], so the two entry points cannot disagree
    /// about one path.
    pub fn from_listing<F: FileSystem + ?Sized>(
        filesystem: &F,
        path: &Path,
        error: &io::Error,
    ) -> Self {
        match error.kind() {
            // A directory that is there and will not open its entries. The one state
            // only a listing can discover.
            io::ErrorKind::PermissionDenied => Self::Unlistable(Arc::new(clone_io_error(error))),
            _ => filesystem.directory_state(path),
        }
    }
}

// `io::Error` is not `Clone`, and both carrying variants need to be. Keeps the kind
// and the message, which is all any sentence renders.
fn clone_io_error(error: &io::Error) -> io::Error {
    io::Error::new(error.kind(), error.to_string())
}

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
/// | [`write_file_no_follow`](FileSystem::write_file_no_follow) | refused, as an error | kept | yes |
///
/// Secret-bearing content takes the first; everything else takes the second.
/// Neither follows a link at the final component, and neither renames over a
/// fifo, socket or device node, including one a link resolves to. Both still
/// follow symlinked **parent** directories — a planted directory symlink can
/// redirect where a file lands either way.
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
    /// old one's extended attributes, POSIX ACLs, SELinux label, or ownership.
    ///
    /// # Strength of the guarantee
    ///
    /// The mode is `0o600` masked by the umask — never more permissive.
    ///
    /// # Errors
    ///
    /// [`FileSystemError::IrregularTarget`] if the path resolves to a fifo,
    /// socket or device node, which is left as it is rather than renamed over.
    /// Otherwise [`FileSystemError`] if the parent directory cannot be created,
    /// the temporary file cannot be created or written, the rename into place
    /// fails, or flushing to disk fails — which can happen after the write
    /// itself succeeded, `ENOSPC` surfacing only at flush time being the usual
    /// case. Every such error names the target path.
    ///
    /// Like [`write_file_no_follow`](FileSystem::write_file_no_follow), this
    /// cannot succeed on an existing file inside a directory the caller cannot
    /// write to: an atomic replace must create a sibling first.
    fn write_file_private(&self, path: &TargetPath, data: &[u8]) -> Result<(), FileSystemError>;

    /// Write a file, refusing a symlink at the final component. The ordinary
    /// writer, and the only one for content that is not a credential.
    ///
    /// Parent directories are created. The content is written to a temporary
    /// file beside the target and renamed into place, so a reader sees either
    /// the old file or the complete new one, and an interrupted write leaves
    /// the old one intact. An existing regular file's permission bits are
    /// carried over; a new file gets `0o666 & !umask`, as an ordinary write
    /// would. A symlink at the final component is refused with
    /// [`FileSystemError::SymlinkedTarget`]: nothing is written, neither the
    /// link nor what it points at is modified, and a dangling link's destination
    /// is not created. A fifo, socket or device node is refused with
    /// [`FileSystemError::IrregularTarget`] and never opened.
    ///
    /// For deploy targets and for paths selfie composes inside its own
    /// directories. A target names a path the user asked selfie to manage, so
    /// writing through a link there sends the content wherever the link points —
    /// possibly somewhere chosen by whoever planted it.
    ///
    /// Because the file is replaced rather than modified, other hard links to it
    /// keep the old content, and it does not inherit the old one's extended
    /// attributes, POSIX ACLs, or SELinux label. The replacement carries the
    /// writing user's ownership, not the old file's: a target owned by another
    /// user, or given another group, does not keep that.
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
    /// against power loss only where the filesystem honors the flush. The
    /// directory flush covers only the immediate parent.
    ///
    /// The refusals are decided when the write is checked. A symlink or fifo
    /// planted after that is replaced by the rename, never followed or opened,
    /// and the write succeeds.
    ///
    /// # Errors
    ///
    /// [`FileSystemError::SymlinkedTarget`] if the final component is a symlink,
    /// [`FileSystemError::IrregularTarget`] if it resolves to a fifo, socket or
    /// device node, or [`FileSystemError`] naming the target if the parent
    /// directory cannot be created, the temporary file cannot be created,
    /// written or flushed, or the rename fails. Replacing a file needs write
    /// permission on its directory, not on the file: a read-only file is
    /// replaced, and a file in a directory the caller cannot write to is not.
    /// In a directory with the sticky bit set, a file owned by another user is
    /// not replaced either, because rename there requires owning the file.
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
    /// [`write_file_no_follow`](FileSystem::write_file_no_follow) checks again
    /// itself before it creates anything, and replaces rather than follows a link
    /// planted after that. This may move *when the user is told* and *whether a
    /// deployment is recorded*, never whether content goes through a link — a
    /// stale answer omits something the next run re-evaluates.
    fn symlink_refusal(&self, path: &TargetPath) -> Option<FileSystemError>;

    /// [`FileSystemError::IrregularTarget`] if `path` resolves to something that
    /// is neither absent nor a regular file
    ///
    /// A fifo, socket or device node. **Not a directory:** opening one never blocks
    /// and writing to one fails without touching anything, so a directory is left to
    /// the read that precedes a deploy. `None` also for a regular file, for a path
    /// that does not exist, and for a symlink to a regular file, which is
    /// [`symlink_refusal`](FileSystem::symlink_refusal)'s question.
    ///
    /// Answers for what an `open` of `path` would land on, so a symlink to a fifo
    /// is a fifo.
    ///
    /// Advisory for writes: the writer checks again immediately before writing,
    /// and a fifo planted after that check is replaced by the rename, never
    /// opened.
    fn irregular_target_refusal(&self, path: &TargetPath) -> Option<FileSystemError>;

    /// Whether a directory is at `path`.
    ///
    /// Symlinks are followed, so it reports on what the path resolves to. `false`
    /// when nothing is there, so an absent target is not an error.
    ///
    /// Answered with a stat rather than a listing, and a directory never blocks the
    /// way a fifo does.
    ///
    /// # Errors
    ///
    /// Returns [`FileSystemError`] when the stat fails for any reason other than
    /// the path not existing — a parent that cannot be traversed, most often. A
    /// caller cannot treat that as "no directory": nothing is known about the
    /// path, and a write there may still land on one.
    fn is_directory(&self, path: &TargetPath) -> Result<bool, FileSystemError>;

    /// Whether a file is readable only by its owner
    ///
    /// Companion to [`write_file_private`](FileSystem::write_file_private), for
    /// deciding whether an existing file already meets that standard. Content and
    /// permissions are independent: a target whose bytes already match may still
    /// be world-readable.
    ///
    /// True when no group or other permission bit is set. Symlinks are
    /// followed, so it reports on the file the path resolves to.
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

    /// What is at a directory path: a directory, absent with a reason, unlistable, or
    /// unknown.
    ///
    /// A dangling symlink is absent with a reason of its own rather than as an empty
    /// path, and a symlink loop is unknown.
    ///
    /// Never returns [`Unlistable`](DirectoryState::Unlistable): discovering that needs
    /// a listing, and a caller that has listed gets there through
    /// [`DirectoryState::from_listing`].
    fn directory_state(&self, path: &Path) -> DirectoryState;

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

    /// A target that is neither absent nor a regular file: a fifo, socket or device
    /// node. A directory is not one of these.
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

// Why selfie will not read a file out of its own repository.
//
// Reading a fifo blocks until a writer arrives, so one committed into the
// dotfiles directory hangs `selfie apply` and `dotfiles drift` with no timeout --
// `command_timeout` governs provider commands, not filesystem calls (selfie-lwv5).
//
// Returns the reason only; the three read sites frame it differently.
//
// Worded for a *source*. `IrregularTarget`'s own `Display` describes a deploy
// target, and here the problem is a file in the repository the user syncs.
pub(crate) fn repository_read_refusal(refusal: &FileSystemError) -> String {
    match refusal {
        FileSystemError::IrregularTarget { kind, .. } => {
            format!("the repository file is a {kind} and selfie will not read it")
        }
        // Fails **closed**, and deliberately not a `_ => {}` that would skip the
        // guard. `irregular_target_refusal` returns only `IrregularTarget` today,
        // so nothing reaches this arm; a wildcard would silently let a future
        // variant through and un-guard the read, which is the failure this whole
        // guard exists to prevent. Refuse on anything it reports.
        other => format!("selfie will not read the repository file: {other}"),
    }
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
/// fs.mock_no_irregular_files();
/// fs.mock_write_file_no_follow(&package_path);
/// fs.mock_remove_file(&package_path);
///
/// let repo = YamlPackageRepository::new(
///     fs,
///     PathBuf::from("/test/packages"),
///     selfie::package::SpecOrigin::PackageDirectory,
/// );
/// ```
#[cfg(feature = "with_mocks")]
impl MockFileSystem {
    /// Answer `state` for every directory classified.
    ///
    /// Most tests are not about the directory, and this is what keeps the
    /// classification out of their way. A test whose subject *is* the directory
    /// sets its own expectation per path instead.
    pub fn mock_directory_state(&mut self, state: DirectoryState) {
        self.expect_directory_state()
            .returning(move |_| state.clone());
    }

    /// Answer "a directory" for every directory classified.
    pub fn mock_directories_exist(&mut self) {
        self.mock_directory_state(DirectoryState::Directory);
    }

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

    /// Answer every irregular-file question with `None`, so reads proceed.
    ///
    /// For a fixture in which **no** path is refused.
    ///
    /// **Do not call this in a test where any path must be refused**, and do not
    /// add a specific refusal alongside it. mockall evaluates expectations in
    /// FIFO order and uses the first that matches, so this unlimited catch-all
    /// swallows every later `expect_irregular_target_refusal`: the refusal never
    /// fires, the read proceeds, and the test passes while asserting nothing.
    /// Set the specific expectation on its own instead — one `returning` closure
    /// can answer `Some` for the refused path and `None` for the rest.
    pub fn mock_no_irregular_files(&mut self) {
        self.expect_irregular_target_refusal().returning(|_| None);
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
    ///
    /// The mocked file behaves like a normal file, so the irregular-file guard
    /// refuses nothing. A test that needs the configuration path refused sets
    /// [`expect_irregular_target_refusal`] itself.
    ///
    /// [`expect_irregular_target_refusal`]: MockFileSystem::expect_irregular_target_refusal
    pub fn mock_config_file(&mut self, config_dir: &Path, config_yaml: &str) {
        let config_dir_owned = PathBuf::from(config_dir);
        let config_path = config_dir.join("config.yaml");

        self.expect_config_dir()
            .return_once(|| Ok(config_dir_owned));
        self.mock_path_exists(&config_path, true);
        self.mock_read_file(&config_path, config_yaml);
        // The loader asks this immediately before the read, so a fixture without
        // it fails on an unexpected call rather than on anything it means to test.
        self.expect_irregular_target_refusal().returning(|_| None);

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

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    // The `other` arm fails closed. Nothing returns a non-`IrregularTarget`
    // variant from `irregular_target_refusal` today, so this is the only thing
    // holding the arm: hand it one directly and the read must still be refused
    // with something a user can read. A `_ => {}` that skipped the guard would
    // return an empty string here.
    #[test]
    fn a_read_refusal_that_is_not_an_irregular_file_still_refuses() {
        let message = repository_read_refusal(&FileSystemError::SymlinkedTarget {
            path: PathBuf::from("/pkgs/myapp/config.toml"),
            points_to: None,
        });
        assert!(!message.is_empty(), "the guard fell through silently");
        assert!(message.contains("repository file"), "got: {message}");
    }
}
