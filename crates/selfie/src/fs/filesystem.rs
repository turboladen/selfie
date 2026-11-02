//! Port for file system operations, and the errors they report.

use std::{
    borrow::Cow,
    io,
    path::{Path, PathBuf},
    sync::Arc,
};

use thiserror::Error;

use crate::fs::target::TargetPath;
use crate::parts::{Composed, Parts};

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

/// What a read of a deploy target found there.
///
/// Returned by [`FileSystem::read_file_no_follow`], so no caller classifies a
/// target by decoding an error.
#[derive(Debug, Clone)]
pub enum TargetRead {
    /// A regular file, and its bytes.
    Bytes(Vec<u8>),
    /// Nothing is at the path.
    Absent,
    /// A component above the path is not a directory, so nothing is at the path
    /// and nothing can be written there: a file, or a symlink whose destination is
    /// not there.
    BelowNonDirectory {
        /// The first such component, when it could be found.
        parent: Option<PathBuf>,
    },
    /// A directory, whether or not selfie may open it.
    Directory,
    /// A symlink at the final component, dangling or not. Not followed.
    Link {
        /// Where the link points, when the link itself could be read.
        points_to: Option<PathBuf>,
    },
    /// A fifo, socket or device node, never read from.
    Irregular {
        /// What is there, for the sentence: `named pipe (fifo)`, `socket`.
        kind: &'static str,
    },
    /// A regular file is there, and selfie may not open or read it.
    Unreadable(Arc<io::Error>),
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

// A directory state's sentence is a clause that follows the path: "does not
// exist", "could not be listed: …". Shared so every sentence about one directory
// describes it in the same words.
crate::parts::display_from_parts!(DirectoryState);

impl Composed for DirectoryState {
    fn parts(&self) -> Parts<'_> {
        let parts = Parts::new();
        match self {
            Self::Directory => parts.text("is a directory"),
            Self::Absent(reason) => reason.parts(),
            Self::Unlistable(error) => parts.text("could not be listed: ").cause(error),
            Self::Unknown(error) => parts.text("could not be checked: ").cause(error),
        }
    }
}

crate::parts::display_from_parts!(AbsentReason);

impl Composed for AbsentReason {
    fn parts(&self) -> Parts<'_> {
        let parts = Parts::new();
        match self {
            Self::Empty => parts.text("does not exist"),
            Self::Occupied { kind } => parts.text(format!("is not a directory, it is a {kind}")),
            Self::DanglingSymlink {
                points_to: Some(destination),
            } => parts
                .text("is a symlink to nothing: it points at ")
                .path(destination),
            // The link read once and would not read again, so the sentence names
            // what is known rather than guessing a destination.
            Self::DanglingSymlink { points_to: None } => parts.text("is a symlink to nothing"),
            Self::ParentNotADirectory { parent } => parts
                .text("is below ")
                .path(parent)
                .text(", which is not a directory"),
        }
    }
}

impl AbsentReason {
    /// Reports whether `mkdir -p` creates the directory, which it does only when
    /// nothing is at the path.
    #[must_use]
    pub fn creatable(&self) -> bool {
        match self {
            Self::Empty => true,
            Self::Occupied { .. }
            | Self::DanglingSymlink { .. }
            | Self::ParentNotADirectory { .. } => false,
        }
    }
}

/// Quotes `path` as shell words that expand to it, for a command the user will
/// paste, or returns `None` when it holds a NUL byte, which no shell word can carry.
///
/// A leading `~/` is left outside the quotes, so the shell still expands it. Pair it
/// with a `--` before the path, which this does not add: a leading dash is not a
/// character quoting protects, so only the option separator ends it. Quote a name
/// with [`shell_word`] instead, so a name starting with `~` is quoted whole.
#[must_use]
pub fn shell_quote(path: &Path) -> Option<String> {
    let rendered = path.display().to_string();
    // A tilde arrives here only when the config loader could not expand it, which
    // needs a home directory selfie cannot determine. Quoting it whole offers a
    // command that creates a directory named `~` in the working directory. Only a
    // bare `~` and a leading `~/` are held out, because those are the two the loader
    // expands; `~user` stays quoted, since a command reaching further than selfie
    // does would name a directory selfie will not then use.
    if rendered == "~" {
        return Some(rendered);
    }
    match rendered.strip_prefix("~/") {
        Some(rest) => shell_word(rest).map(|rest| format!("~/{rest}")),
        None => shell_word(&rendered).map(Cow::into_owned),
    }
}

/// Quotes `word` as one shell word, unless the shell leaves every character in it
/// alone, or returns `None` when it holds a NUL byte, which no shell word can carry.
///
/// Quoting keeps every other character as it is, control characters included, so
/// a quoted word is not safe to print as it is.
#[must_use]
pub fn shell_word(word: &str) -> Option<Cow<'_, str>> {
    shlex::try_quote(word).ok()
}

/// `name` as a command's operand: quoted as one shell word, after `--` when it
/// starts with a dash, so the command does not read it as an option. Returns
/// `None` when it holds a NUL byte, which no shell word can carry.
#[must_use]
pub fn shell_operand(name: &str) -> Option<String> {
    let word = shell_word(name)?;
    Some(if name.starts_with('-') {
        format!("-- {word}")
    } else {
        word.into_owned()
    })
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

/// What a write does with anything already at its target when the rename that
/// puts the new file in place lands.
#[derive(Clone, PartialEq, Eq)]
pub enum IfPresent {
    /// Replace it.
    Replace,
    /// Replace it only if it is still a regular file holding exactly these
    /// bytes, as it did when the caller read it. Otherwise nothing is written
    /// and the write answers [`Placed::Kept`]: whatever is there is left in
    /// place, and where nothing is, nothing is created. A symlink is not the
    /// expected file: the private writer keeps one, and the no-follow writer
    /// refuses it as it does under every variant.
    ///
    /// A mitigation, not a guarantee: a change made at the same instant as the
    /// write may still be replaced.
    ReplaceIfHolding(Arc<[u8]>),
    /// Leave it in place, write nothing, and answer [`Placed::Kept`]. For a
    /// target the caller last found absent, so that something appearing there
    /// since is never replaced unseen.
    ///
    /// A guarantee where the volume supports an exclusive rename, and on Linux
    /// also where it lacks one but supports hard links. Elsewhere it is a
    /// mitigation, not a guarantee: the writer looks at the target once more,
    /// without following a link, immediately before an ordinary rename, and
    /// something that appears between that look and the rename is replaced. A
    /// write to an absent target on such a volume still succeeds. A sandbox that
    /// refuses the exclusive rename itself with `EPERM` gets the same mitigation.
    Keep,
}

// Written by hand, so the expected bytes, which may be a file's whole content,
// never reach a log line.
impl std::fmt::Debug for IfPresent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Replace => f.write_str("Replace"),
            Self::ReplaceIfHolding(bytes) => {
                write!(f, "ReplaceIfHolding({} bytes)", bytes.len())
            }
            Self::Keep => f.write_str("Keep"),
        }
    }
}

/// What a write did at its target.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[must_use = "a write that kept what was at its target wrote nothing"]
pub enum Placed {
    /// The new file is at the target.
    Written,
    /// Under [`IfPresent::Keep`], something was at the target when the rename
    /// landed; under [`IfPresent::ReplaceIfHolding`], the target no longer held
    /// the expected bytes. It is left there, and nothing was written.
    Kept,
}

/// What [`FileSystem::remove_file_if_holding`] did at its path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[must_use = "a removal that kept what was at its path removed nothing"]
pub enum Removal {
    /// The file is gone.
    Removed,
    /// What was there was not the expected file, or nothing was. It is left as it
    /// is, and nothing was removed.
    Kept,
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
/// Under [`IfPresent::ReplaceIfHolding`] the private writer keeps a link rather
/// than replacing it, since a link is not the file the caller read.
///
/// Secret-bearing content takes the first; everything else takes the second.
/// Each takes an [`IfPresent`] saying whether the rename may replace what is at
/// the target, and answers with the [`Placed`] that says whether it did.
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

    /// Read a file from the user's repository as UTF-8: a dotfile's source or
    /// template. Follows symlinks, as a repository file may be one.
    ///
    /// # Errors
    ///
    /// [`RepositoryRead`], saying what the failed read found at the path.
    fn read_repository_file(&self, path: &Path) -> Result<String, RepositoryRead>;

    /// Read a target without following a symlink at its final component and
    /// without blocking on a fifo. The reader for deploy targets.
    ///
    /// Imposes no encoding requirement: secret-bearing content is not guaranteed
    /// to be UTF-8, and decoding it lossily before a comparison would report two
    /// different files as identical. Symlinked **parent** directories are still
    /// followed, as they are by both writers.
    ///
    /// What is at the path is decided when it is opened, so a link or fifo planted
    /// after any earlier check is reported as one rather than read.
    ///
    /// # Errors
    ///
    /// [`FileSystemError::IoError`] when what is there is unknown: a parent selfie
    /// may not search, a loop above the target, or a regular file that failed for a
    /// reason other than permission, such as too many open files. A regular file
    /// selfie may not open or read is [`TargetRead::Unreadable`], not an error.
    fn read_file_no_follow(&self, path: &TargetPath) -> Result<TargetRead, FileSystemError>;

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
    /// # Durability
    ///
    /// As [`write_file_no_follow`](FileSystem::write_file_no_follow)'s: returns
    /// only once the data has been flushed to disk, with a best-effort attempt at
    /// the parent directory.
    ///
    /// # Strength of the guarantee
    ///
    /// The mode is `0o600` masked by the umask — never more permissive. The
    /// durability is as strong as `write_file_no_follow`'s, and no stronger.
    ///
    /// # Errors
    ///
    /// Returns [`FileSystemError::IrregularTarget`] if the path resolves to a
    /// fifo, socket or device node, which is left as it is rather than renamed
    /// over. Under [`IfPresent::ReplaceIfHolding`] such a node is not an error:
    /// it is kept, and the write answers [`Placed::Kept`].
    ///
    /// Returns another [`FileSystemError`], naming the target path, if the
    /// parent directory cannot be created, the temporary file cannot be created
    /// or written, the target cannot be read back under
    /// [`IfPresent::ReplaceIfHolding`], the rename into place fails, or flushing
    /// to disk fails. Flushing can fail after the write itself succeeded, and
    /// `ENOSPC` surfacing only at flush time is the usual case.
    ///
    /// Like [`write_file_no_follow`](FileSystem::write_file_no_follow), this
    /// cannot succeed on an existing file inside a directory the caller cannot
    /// write to: an atomic replace must create a sibling first.
    fn write_file_private(
        &self,
        path: &TargetPath,
        data: &[u8],
        if_present: IfPresent,
    ) -> Result<Placed, FileSystemError>;

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
    /// planted after that is never followed or opened: under
    /// [`IfPresent::Replace`] the rename replaces it and the write succeeds, and
    /// under [`IfPresent::Keep`] and [`IfPresent::ReplaceIfHolding`] it is left
    /// in place, within that variant's guarantee.
    ///
    /// # Errors
    ///
    /// Returns [`FileSystemError::SymlinkedTarget`] if the final component is a
    /// symlink. Returns [`FileSystemError::IrregularTarget`] if it resolves to a
    /// fifo, socket or device node, except under [`IfPresent::ReplaceIfHolding`],
    /// which keeps such a node and answers [`Placed::Kept`].
    ///
    /// Returns another [`FileSystemError`], naming the target, if the parent
    /// directory cannot be created, the temporary file cannot be created,
    /// written or flushed, the target cannot be read back under
    /// [`IfPresent::ReplaceIfHolding`], or the rename fails. Replacing a file
    /// needs write permission on its directory, not on the file: a read-only
    /// file is replaced, and a file in a directory the caller cannot write to is
    /// not.
    /// In a directory with the sticky bit set, a file owned by another user is
    /// not replaced either, because rename there requires owning the file.
    fn write_file_no_follow(
        &self,
        path: &TargetPath,
        data: &[u8],
        if_present: IfPresent,
    ) -> Result<Placed, FileSystemError>;

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

    /// What a read of `path` would meet when it opens it, or `None` when the open
    /// succeeds, nothing is there, the final component is a symlink, or what is
    /// there is not a regular file: a directory, fifo, socket or device node.
    ///
    /// Opens and closes without reading a byte, never follows a symlink at the final
    /// component, and never waits on a fifo. Answers for the effective user, as the
    /// read itself would.
    ///
    /// Advisory: a target can change between this and the read, so a caller that
    /// reads must still handle the read's own error.
    fn open_for_read_refusal(&self, path: &TargetPath) -> Option<OpenRefusal>;

    /// The refusal that keeps this process from creating an entry in the
    /// existing directory `directory`, or `None` when its permissions allow one.
    ///
    /// Asks about `directory` alone; [`file_creation_refusal`](Self::file_creation_refusal)
    /// is the question about a path that may not exist yet.
    fn access_refusal(&self, directory: &Path) -> Option<FileSystemError>;

    /// The refusal that keeps a new file from being created in `directory`, or
    /// `None` when its permissions allow one. An absent `directory` is judged by
    /// whether it could be created: the nearest ancestor that exists must be a
    /// directory that allows a new entry, with nothing else in the way.
    ///
    /// A permission check, not a write: a later write can still fail, for example
    /// on a full disk or where an existing file at the name refuses replacement.
    /// Answers for this process's user.
    fn file_creation_refusal(&self, directory: &Path) -> Option<FileSystemError> {
        let refusal = |path: &Path, kind: io::ErrorKind| {
            Some(FileSystemError::CannotCreateIn {
                path: path.to_path_buf(),
                cause: Arc::new(io::Error::from(kind)),
            })
        };
        // Walked through `directory_state`, so a file, a dangling link or an
        // unknown path in the way is refused as what it is. `create_dir_all` makes
        // every missing level from the nearest existing ancestor down, so that
        // ancestor's permissions decide whether the rest can be created.
        let mut candidate = if directory.as_os_str().is_empty() {
            PathBuf::from(".")
        } else {
            directory.to_path_buf()
        };
        loop {
            match self.directory_state(&candidate) {
                DirectoryState::Directory => return self.access_refusal(&candidate),
                DirectoryState::Absent(AbsentReason::Empty) => {
                    // A walk that cannot climb further, at the root or at `.`,
                    // has found nothing to create the directory in.
                    let parent = parent_dir(&candidate);
                    if candidate.parent().is_none() || parent == candidate {
                        return refusal(&candidate, io::ErrorKind::NotFound);
                    }
                    candidate = parent.to_path_buf();
                }
                DirectoryState::Absent(AbsentReason::ParentNotADirectory { parent }) => {
                    return refusal(&parent, io::ErrorKind::NotADirectory);
                }
                DirectoryState::Absent(AbsentReason::Occupied { .. }) => {
                    return refusal(&candidate, io::ErrorKind::NotADirectory);
                }
                // `create_dir_all` meets the link itself and fails with EEXIST.
                DirectoryState::Absent(AbsentReason::DanglingSymlink { .. }) => {
                    return refusal(&candidate, io::ErrorKind::AlreadyExists);
                }
                DirectoryState::Unlistable(error) | DirectoryState::Unknown(error) => {
                    return Some(FileSystemError::CannotCreateIn {
                        path: candidate,
                        cause: Arc::new(io::Error::new(error.kind(), error.to_string())),
                    });
                }
            }
        }
    }

    /// Whether a file is readable only by its owner
    ///
    /// Companion to [`write_file_private`](FileSystem::write_file_private), for
    /// deciding whether an existing file already meets that standard. Content and
    /// permissions are independent: a target whose bytes already match may still
    /// be world-readable.
    ///
    /// True when no group or other permission bit is set. Answers for what is at
    /// the path itself: a symlink there is refused, never judged by its
    /// destination's mode.
    ///
    /// # Errors
    ///
    /// [`FileSystemError::SymlinkedTarget`] if the final component is a symlink.
    /// [`FileSystemError`] if the metadata cannot be read.
    fn is_owner_only(&self, path: &TargetPath) -> Result<bool, FileSystemError>;

    /// Remove a file. Irreversible.
    ///
    /// # Errors
    ///
    /// [`FileSystemError`] if the file does not exist, permission is denied, the
    /// path is a directory rather than a file, or any other IO error occurs.
    fn remove_file(&self, path: &Path) -> Result<(), FileSystemError>;

    /// Remove the file at `path` only if what `path` names is a regular file
    /// holding exactly `expected`, as it did when the caller read it.
    ///
    /// A symlink at `path` is judged by the file it names, and only the link is
    /// removed, never what it names. Anything else at `path`, or nothing, is
    /// left in place, and the call answers [`Removal::Kept`]. A mitigation, not
    /// a guarantee: a change made at the same instant as the removal may still
    /// be removed.
    ///
    /// # Errors
    ///
    /// Returns [`FileSystemError`], naming `path`, if what is there cannot be
    /// read back, such as a file selfie may not read, or the removal itself
    /// fails for a reason other than the file being gone.
    fn remove_file_if_holding(
        &self,
        path: &TargetPath,
        expected: &[u8],
    ) -> Result<Removal, FileSystemError>;

    /// Whether `path` exists, as either a file or a directory.
    fn path_exists(&self, path: &Path) -> bool;

    /// What is at a directory path: a directory, absent with a reason, unlistable, or
    /// unknown.
    ///
    /// A dangling symlink is absent with a reason of its own rather than as an empty
    /// path, and a symlink loop is unknown. A trailing slash does not change the
    /// answer.
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

    /// The identity of the file at `path`, following links. Two paths answering
    /// equal identities name one file, however each is spelled or mounted.
    ///
    /// For configured repository directories. **Never call this on a deploy
    /// target**: it follows links, so it answers for whatever a link points to.
    ///
    /// On a file system that does not report stable inode numbers, such as some
    /// FUSE, virtiofs and NFS mounts, two different files can share an identity
    /// and be taken as one.
    ///
    /// # Errors
    ///
    /// [`FileSystemError`] if the path does not exist, permission is denied on
    /// any component, or any other IO error occurs.
    fn identity(&self, path: &Path) -> Result<FileIdentity, FileSystemError>;

    /// The current user's configuration directory, by platform convention —
    /// `~/.config` on Unix-like systems.
    ///
    /// # Errors
    ///
    /// [`FileSystemError`] if the home directory cannot be determined or the
    /// configuration directory cannot be accessed.
    fn config_dir(&self) -> Result<PathBuf, FileSystemError>;
}

/// What makes two paths one file. Equal identities name one file; nothing else
/// about the value means anything.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FileIdentity {
    device: u64,
    inode: u64,
}

impl FileIdentity {
    /// The identity of the file numbered `inode` on the device numbered `device`,
    /// for a test double of [`FileSystem`].
    #[cfg(any(test, feature = "with_mocks"))]
    #[must_use]
    pub const fn new(device: u64, inode: u64) -> Self {
        Self::of(device, inode)
    }

    /// The identity of the file numbered `inode` on the device numbered `device`.
    pub(crate) const fn of(device: u64, inode: u64) -> Self {
        Self { device, inode }
    }
}

/// Holds an IO error and, when the operation that failed names one, the path
/// the error is about.
#[derive(Debug, Clone)]
pub struct IoFailure {
    /// Names the path the error is about, or is `None` when the operation
    /// names no path.
    pub path: Option<PathBuf>,
    /// Holds the error the operation returned.
    pub error: Arc<io::Error>,
}

impl IoFailure {
    /// Wraps `error` together with the `path` it is about.
    pub fn at(path: &Path, error: impl Into<Arc<io::Error>>) -> Self {
        Self {
            path: Some(path.to_path_buf()),
            error: error.into(),
        }
    }
}

crate::parts::display_from_parts!(IoFailure);

impl Composed for IoFailure {
    fn parts(&self) -> Parts<'_> {
        io_parts(self.path.as_deref(), &self.error)
    }
}

impl From<io::Error> for IoFailure {
    fn from(error: io::Error) -> Self {
        Arc::new(error).into()
    }
}

impl From<Arc<io::Error>> for IoFailure {
    fn from(error: Arc<io::Error>) -> Self {
        Self { path: None, error }
    }
}

/// Every way a file system operation here can fail.
// No variant has a source: a CLI error with a chain longer than one prints the
// chain, so a source here would print the cause twice.
#[derive(Debug, Clone)]
pub enum FileSystemError {
    /// General IO error occurred during file system operation
    IoError(IoFailure),

    /// Home directory could not be determined (needed for path expansion)
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
    IrregularTarget { path: PathBuf, kind: &'static str },

    /// A new file cannot be created under `path`: the directory it was asked
    /// about, or the nearest ancestor of it that exists, refuses one.
    CannotCreateIn {
        path: PathBuf,
        cause: Arc<io::Error>,
    },

    /// A write could not create the target because a component above it is not a
    /// directory. `parent` is the first such component, when it could be found.
    BelowNonDirectory {
        path: PathBuf,
        parent: Option<PathBuf>,
        cause: Arc<io::Error>,
    },

    /// A write could not replace the target because a directory is there.
    DirectoryTarget {
        path: PathBuf,
        cause: Arc<io::Error>,
    },
}

crate::parts::display_from_parts!(error FileSystemError);

impl Composed for FileSystemError {
    fn parts(&self) -> Parts<'_> {
        match self {
            Self::IoError(failure) => Parts::new().text("IO error: ").append(failure.parts()),
            Self::HomeDirNotFound => Parts::new().text("Home directory not found"),
            Self::SymlinkedTarget { path, points_to } => {
                let to = points_to.as_deref().map_or_else(Parts::new, |destination| {
                    Parts::new().text(" to '").path(destination).text("'")
                });
                Parts::new()
                    .path(path)
                    .text(": target is a symlink")
                    .append(to)
                    .text(" and selfie will not write through it")
            }
            Self::IrregularTarget { path, kind } => Parts::new().path(path).text(format!(
                ": target resolves to a {kind} and selfie will not write to it"
            )),
            Self::CannotCreateIn { path, cause } => {
                Parts::new().path(path).text(": ").cause(&**cause)
            }
            // The message names the directory the write could not create.
            Self::BelowNonDirectory { path, cause, .. } => Parts::new()
                .text("IO error: ")
                .append(io_parts(Some(parent_dir(path)), cause)),
            Self::DirectoryTarget { path, cause } => Parts::new()
                .text("IO error: ")
                .append(io_parts(Some(path), cause)),
        }
    }
}

// The parts of an IO error after "IO error: ": the path it is about, when it
// has one, then the error.
fn io_parts<'a>(path: Option<&'a Path>, error: &io::Error) -> Parts<'a> {
    let parts = match path {
        Some(path) => Parts::new().path(path).text(": "),
        None => Parts::new(),
    };
    parts.cause(error)
}

/// What a failed read of a repository file found at the path.
///
/// Each carries the read's error, which its `Display` renders.
#[derive(Error, Debug, Clone)]
pub enum RepositoryRead {
    /// Nothing is there, or a component above the path is not a directory.
    #[error("IO error: {0}")]
    Absent(Arc<io::Error>),
    /// A directory is there.
    #[error("IO error: {0}")]
    Directory(Arc<io::Error>),
    /// A file is there, and it could not be read: denied, or not UTF-8.
    #[error("IO error: {0}")]
    Unreadable(Arc<io::Error>),
    /// What is there could not be found out.
    #[error("IO error: {0}")]
    Undetermined(Arc<io::Error>),
}

/// What an open of a target for reading met, from
/// [`FileSystem::open_for_read_refusal`].
#[derive(Error, Debug, Clone)]
pub enum OpenRefusal {
    /// A regular file is there, and selfie may not open it.
    #[error("IO error: {0}")]
    Unreadable(Arc<io::Error>),
    /// What is there could not be found out.
    #[error("IO error: {0}")]
    Undetermined(Arc<io::Error>),
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

/// The directory `path` lives in, as one the filesystem will actually open: its
/// parent, or `.` for a bare name.
///
/// `Path::parent` gives `Some("")` for a bare name like `config.toml`, and
/// `File::open("")` is `ENOENT`. `create_dir_all` and `tempfile_in` both cope
/// with the empty path; the directory fsync does not.
pub(crate) fn parent_dir(path: &Path) -> &Path {
    path.parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."))
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
/// // A spec removal goes through the compare-and-remove, never `remove_file`.
/// fs.expect_remove_file_if_holding()
///     .returning(|_, _| Ok(selfie::fs::Removal::Removed));
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

    /// Allow a new file in every directory asked about.
    pub fn mock_creatable_directories(&mut self) {
        self.expect_file_creation_refusal().returning(|_| None);
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
            .withf(move |target, _, _| target.path() == path_buf)
            .returning(|_, _, _| Ok(Placed::Written));
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

    // A file-system error names its cause once and does not also chain it, so a
    // consumer walking the chain does not print the cause twice.
    #[test]
    fn every_file_system_error_names_its_cause_once() {
        use std::error::Error as _;

        let error = FileSystemError::CannotCreateIn {
            path: PathBuf::from("/state"),
            cause: Arc::new(io::Error::other("the cause")),
        };

        let message = error.to_string();
        assert_eq!(message.matches("the cause").count(), 1, "{message}");
        assert!(error.source().is_none(), "{message} also chains its cause");

        let error = FileSystemError::IoError(IoFailure::at(
            Path::new("/state/file"),
            io::Error::other("the cause"),
        ));
        assert!(error.source().is_none(), "{error} also chains its cause");
    }

    // An IO failure's message is its path, then its error, as the variants that
    // carry one show it.
    #[test]
    fn an_io_failure_names_its_path_before_its_error() {
        let error = || io::Error::other("denied");
        let path = Path::new("/state/deploy-state.yml");

        assert_eq!(
            FileSystemError::IoError(IoFailure::at(path, error())).to_string(),
            "IO error: /state/deploy-state.yml: denied"
        );
        assert_eq!(
            FileSystemError::IoError(error().into()).to_string(),
            "IO error: denied"
        );
        assert_eq!(
            FileSystemError::BelowNonDirectory {
                path: path.to_path_buf(),
                parent: None,
                cause: Arc::new(error()),
            }
            .to_string(),
            "IO error: /state: denied"
        );
        assert_eq!(
            FileSystemError::DirectoryTarget {
                path: path.to_path_buf(),
                cause: Arc::new(error()),
            }
            .to_string(),
            "IO error: /state/deploy-state.yml: denied"
        );
    }

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

    // The remedy is pasted into a shell, so whatever the path holds, the shell must
    // read it back as exactly one word naming that path. A single quote is the case
    // single-quoting alone cannot carry.
    #[test]
    fn a_quoted_path_reads_back_as_one_word() {
        for path in [
            "/home/me/dotfiles",
            "/home/me/my dotfiles",
            "/home/me/it's here",
            "/home/me/a\"b$c`d",
            "/home/me/café",
            "-foo",
        ] {
            let quoted = super::shell_quote(std::path::Path::new(path)).expect("no NUL");
            assert_eq!(
                shlex::split(&quoted),
                Some(vec![path.to_string()]),
                "{path} quoted as {quoted}"
            );
        }
    }

    // A readback cannot tell `~/x` from `'~/x'`, since splitting does no tilde
    // expansion, so the exact text is what shows the tilde stays outside the
    // quotes, where the shell expands it.
    #[test]
    fn shell_quote_holds_a_leading_tilde_outside_the_quotes() {
        let quote = |path: &str| super::shell_quote(std::path::Path::new(path));
        assert_eq!(quote("~/my dir").as_deref(), Some("~/'my dir'"));
        assert_eq!(quote("~/plain").as_deref(), Some("~/plain"));
        assert_eq!(quote("~").as_deref(), Some("~"));
        assert_eq!(quote("~user/x").as_deref(), Some("'~user/x'"));
        assert_eq!(quote("/a\0b"), None);
    }

    // A word is quoted whole, a leading `~` included, and reads back as itself; a
    // NUL byte cannot be carried by any shell word.
    #[test]
    fn a_word_reads_back_as_one_word() {
        for word in ["~x", "~/x", "-x", "café", "it's", "a b", ""] {
            let quoted = super::shell_word(word).expect("no NUL");
            assert_eq!(
                shlex::split(&quoted),
                Some(vec![word.to_string()]),
                "{quoted}"
            );
        }
        assert_eq!(super::shell_word("~/x").as_deref(), Some("'~/x'"));
        assert_eq!(super::shell_word("a\0b"), None);
    }
}
