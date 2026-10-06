//! What is at a deploy target, and how a refused deploy is worded.
//!
//! The secret-bearing path, the repository-file path, drift and track all guard
//! and classify a target through here and refuse it in the same words, so none of
//! them can describe one refusal differently from the others.

use std::{
    io,
    path::{Path, PathBuf},
    sync::Arc,
};

use crate::{
    fs::{
        filesystem::{
            AbsentReason, DirectoryState, FileSystem, FileSystemError, OpenRefusal, RepositoryRead,
            TargetRead,
        },
        target::{TargetPath, TargetRejection},
    },
    package::event::{Condition, Location, Refusal},
};

/// A symlink at a target's final component.
#[derive(Clone)]
pub(super) struct Link {
    path: PathBuf,
    points_to: Option<PathBuf>,
}

impl Link {
    /// Where the link points, when the link itself could be read. The raw link
    /// text, relative whenever the user wrote a relative link: for wording only,
    /// never to be resolved.
    pub(super) fn destination(&self) -> Option<&Path> {
        self.points_to.as_deref()
    }

    /// The refusal a write that will not follow the link gives for it.
    pub(super) fn refusal(&self) -> FileSystemError {
        FileSystemError::SymlinkedTarget {
            path: self.path.clone(),
            points_to: self.points_to.clone(),
        }
    }
}

/// What the two questions asked before anything reads a target found there.
pub(super) enum TargetGuard {
    /// Neither question found anything. The target may be a regular file, a
    /// directory, or nothing at all.
    Clear,
    /// The final component is a symlink. `behind` is the refusal for what the link
    /// resolves to, when that is a fifo, socket or device node.
    Link {
        link: Link,
        behind: Option<FileSystemError>,
    },
    /// Not a symlink, but a fifo, socket or device node; or a refusal the guard
    /// does not recognize, which refuses rather than being read as "nothing here".
    Refused(FileSystemError),
}

/// Ask both questions every path reaching a target asks before it reads one: is
/// the final component a symlink, and does the path resolve to a fifo, socket or
/// device node. For a path that treats a link by what it points at.
///
/// A path that refuses every link asks [`guard_refusal`] instead, which does not
/// look behind one.
// The symlink question takes a non-following stat, because it is about the name;
// the irregular one a following stat, because the hazard is what an `open` lands
// on. They stay two calls with two syscalls. The secret path needs both answers for
// a link: it replaces one, and refuses one whose destination is a fifo.
pub(super) fn guard_target<F: FileSystem>(filesystem: &F, target: &TargetPath) -> TargetGuard {
    match link_at(filesystem, target) {
        Err(unrecognized) => TargetGuard::Refused(unrecognized),
        Ok(Some(link)) => TargetGuard::Link {
            link,
            behind: filesystem.irregular_target_refusal(target),
        },
        Ok(None) => filesystem
            .irregular_target_refusal(target)
            .map_or(TargetGuard::Clear, TargetGuard::Refused),
    }
}

/// The refusal for a target on a path that refuses every symlink: the link when
/// there is one, whatever it points at, otherwise a fifo, socket or device node.
///
/// Asks the same questions as [`guard_target`], in the same order.
// Skips the following stat when the link has already answered. That stat reaches
// the link's destination, which may sit on a hung mount; a path that refuses the
// link either way has no reason to wait on it.
pub(super) fn guard_refusal<F: FileSystem>(
    filesystem: &F,
    target: &TargetPath,
) -> Option<FileSystemError> {
    match link_at(filesystem, target) {
        Err(unrecognized) => Some(unrecognized),
        Ok(Some(link)) => Some(link.refusal()),
        Ok(None) => filesystem.irregular_target_refusal(target),
    }
}

/// Whether `target`'s final component is a symlink, asking only that.
///
/// # Errors
///
/// A refusal other than [`FileSystemError::SymlinkedTarget`], which the caller
/// must refuse the entry on.
pub(super) fn link_at<F: FileSystem>(
    filesystem: &F,
    target: &TargetPath,
) -> Result<Option<Link>, FileSystemError> {
    classify_link(filesystem.symlink_refusal(target))
}

/// A link for a symlink refusal, `None` for no refusal.
///
/// # Errors
///
/// Any refusal other than [`FileSystemError::SymlinkedTarget`], carried so the
/// caller refuses on it.
// Fails **closed**, and deliberately not a `_ => Ok(None)` that would treat an
// unrecognized refusal as "no link". `symlink_refusal` returns only
// `SymlinkedTarget` today, so nothing reaches that arm; a fallback would silently
// send a future variant down a path that reads the target, which for a secret
// entry hands the bytes to a resolver.
pub(super) fn classify_link(
    refusal: Option<FileSystemError>,
) -> Result<Option<Link>, FileSystemError> {
    match refusal {
        None => Ok(None),
        Some(FileSystemError::SymlinkedTarget { path, points_to }) => {
            Ok(Some(Link { path, points_to }))
        }
        Some(other) => Err(other),
    }
}

/// What is at an entry's target when apply, drift or track reaches it.
///
/// Kept distinct from `Option<Vec<u8>>` because "absent" and "present but
/// unreadable" call for opposite handling: the first is safe to write, the second
/// must never be written over as though nothing were there.
pub(super) enum TargetState {
    /// Nothing is at the path.
    Absent,
    /// A component above the target is not a directory, so nothing is there and
    /// nothing can be written there. `parent` is the first such component.
    BelowNonDirectory {
        parent: Option<PathBuf>,
    },
    Readable(Vec<u8>),
    /// A directory, which a file cannot replace.
    Directory,
    /// A symlink, found by the read after every look missed it.
    Link(Link),
    /// A fifo, socket or device node, found by the read after every look missed it.
    /// Carries the refusal a writer gives for it.
    Irregular(FileSystemError),
    /// A regular file is there, and it could not be read.
    Unreadable(Arc<io::Error>),
    /// What is there could not be found out.
    Undetermined(FileSystemError),
}

/// What is at `target`, from one read of it.
///
/// Read as raw bytes, so two different files are never reported identical after
/// a lossy decode. The read never follows a link at the target or waits on a fifo,
/// but ask the guard first all the same, which keeps a device node from being
/// opened at all: [`guard_refusal`] on a path that refuses every link, and
/// [`guard_target`] on the secret path, which replaces one.
// One read rather than an existence probe and then a read, so a file deleted
// between the two deploys instead of refusing, and a directory is named rather than
// reported as a read failure. The port says what is there, including why a read
// failed, so nothing here looks at the path again. A target that cannot be told is
// never written over: apply refuses it on both paths, and drift refuses it too.
pub(super) fn read_target_state<F: FileSystem>(filesystem: &F, target: &TargetPath) -> TargetState {
    match filesystem.read_file_no_follow(target) {
        Ok(TargetRead::Bytes(bytes)) => TargetState::Readable(bytes),
        Ok(TargetRead::Absent) => TargetState::Absent,
        Ok(TargetRead::BelowNonDirectory { parent }) => TargetState::BelowNonDirectory { parent },
        Ok(TargetRead::Directory) => TargetState::Directory,
        Ok(TargetRead::Link { points_to }) => TargetState::Link(Link {
            path: target.path().to_path_buf(),
            points_to,
        }),
        Ok(TargetRead::Irregular { kind }) => {
            TargetState::Irregular(FileSystemError::IrregularTarget {
                path: target.path().to_path_buf(),
                kind,
            })
        }
        Ok(TargetRead::Unreadable(error)) => TargetState::Unreadable(error),
        Err(error) => TargetState::Undetermined(error),
    }
}

// The mappings below decide every condition a check of a path names, so one state
// on disk is one condition whichever check meets it. Each lists every variant of a
// type selfie owns, with no `_` arm, so a variant added to the port has to be
// placed before anything compiles. An `io::Error`'s kind is never matched here:
// its enum is non-exhaustive, and the port already says what an errno means.

/// The condition a guard's or a writer's error names.
pub(super) fn guarded(error: &FileSystemError) -> Condition {
    match error {
        FileSystemError::SymlinkedTarget { .. } => Condition::Symlink,
        FileSystemError::IrregularTarget { .. } => Condition::Irregular,
        FileSystemError::BelowNonDirectory { .. } => Condition::NotADirectory,
        FileSystemError::DirectoryTarget { .. } => Condition::Directory,
        FileSystemError::IoError(_) | FileSystemError::HomeDirNotFound => Condition::Undetermined,
    }
}

/// The condition a failed read of a repository file names. A path only read
/// cannot be below a non-directory, since nothing can be there: that is `Absent`.
pub(super) fn repository_condition(read: &RepositoryRead) -> Condition {
    match read {
        RepositoryRead::Absent(_) => Condition::Absent,
        RepositoryRead::Directory(_) => Condition::Directory,
        RepositoryRead::Unreadable(_) => Condition::Unreadable,
        RepositoryRead::Undetermined(_) => Condition::Undetermined,
    }
}

/// Where `condition`, found on the path at `at`, lies: a component that is not a
/// directory lies above the target, not at it.
pub(super) fn located(condition: Condition, at: Location) -> (Condition, Location) {
    match (condition, at) {
        (Condition::NotADirectory, Location::Target) => (condition, Location::AboveTarget),
        (condition, at) => (condition, at),
    }
}

// What stands in the way of a file at a target, as `directory_state` reports it.
enum InTheWay<'a> {
    Directory,
    /// A component above the target is not a directory.
    NotADirectory(&'a AbsentReason),
    /// The check itself failed.
    Undetermined(&'a Arc<io::Error>),
}

impl InTheWay<'_> {
    fn condition(&self) -> Condition {
        match self {
            Self::Directory => Condition::Directory,
            Self::NotADirectory(_) => Condition::NotADirectory,
            Self::Undetermined(_) => Condition::Undetermined,
        }
    }
}

// An unlistable directory is still a directory: no file can replace it.
fn in_the_way(state: &DirectoryState) -> Option<InTheWay<'_>> {
    match state {
        DirectoryState::Directory | DirectoryState::Unlistable(_) => Some(InTheWay::Directory),
        DirectoryState::Absent(reason @ AbsentReason::ParentNotADirectory { .. }) => {
            Some(InTheWay::NotADirectory(reason))
        }
        DirectoryState::Absent(
            AbsentReason::Empty
            | AbsentReason::Occupied { .. }
            | AbsentReason::DanglingSymlink { .. },
        ) => None,
        DirectoryState::Unknown(error) => Some(InTheWay::Undetermined(error)),
    }
}

/// The refusal a failed write to `target` amounts to, or `None` when the write
/// failed. `source` is how the refusal names the entry.
///
/// A refusal is a condition the checks before the write refuse too: a symlink the
/// writer would not write through, a fifo, socket or device node, a component
/// above the target that is not a directory, or a directory at the target.
/// Anything else is a failure of the write.
pub(super) fn classify_write(
    source: &str,
    target: &TargetPath,
    error: &FileSystemError,
) -> Option<Refusal> {
    match error {
        FileSystemError::SymlinkedTarget { .. } | FileSystemError::IrregularTarget { .. } => {
            Some(refusal_for(source, error))
        }
        FileSystemError::BelowNonDirectory { parent, .. } => Some(below_non_directory_refusal(
            source,
            target,
            parent.as_deref(),
        )),
        FileSystemError::DirectoryTarget { .. } => Some(Refusal::found(
            located(guarded(error), Location::Target),
            directory_message(source, target),
        )),
        FileSystemError::IoError(_) | FileSystemError::HomeDirNotFound => None,
    }
}

/// The bytes at a target an entry may go on to compare (`None` for nothing there),
/// or the refusal of the entry for what the read found instead.
///
/// A link or fifo the read found is worded as the guard words one, since the
/// user's remedy is the same whichever check found it.
pub(super) fn readable_or_refusal(
    source: &str,
    target: &TargetPath,
    state: TargetState,
) -> Result<Option<Vec<u8>>, Refusal> {
    let (condition, message) = match state {
        TargetState::Absent => return Ok(None),
        TargetState::Readable(bytes) => return Ok(Some(bytes)),
        TargetState::BelowNonDirectory { parent } => {
            return Err(below_non_directory_refusal(
                source,
                target,
                parent.as_deref(),
            ));
        }
        TargetState::Directory => (Condition::Directory, directory_message(source, target)),
        TargetState::Link(link) => (Condition::Symlink, refusal_message(source, &link.refusal())),
        TargetState::Irregular(refusal) => {
            (Condition::Irregular, refusal_message(source, &refusal))
        }
        TargetState::Unreadable(error) => (
            Condition::Unreadable,
            unreadable_message(source, target, &FileSystemError::IoError(error)),
        ),
        TargetState::Undetermined(error) => (
            Condition::Undetermined,
            unreadable_message(source, target, &error),
        ),
    };
    Err(Refusal::found(
        located(condition, Location::Target),
        message,
    ))
}

/// What a directory at a target means and what to do about it, as one clause, so
/// every path that meets one words it the same way.
pub(super) fn directory_at_target(target: &TargetPath) -> String {
    format!(
        "a directory is at the target '{}', and a file cannot replace a directory. Remove it \
         or point the entry somewhere else.",
        target.display()
    )
}

fn directory_message(source: &str, target: &TargetPath) -> String {
    format!("Skipping '{source}': {}", directory_at_target(target))
}

// The refusal of a target below a component that is not a directory, from a read or
// a write that met one. Names the component when the port found it.
fn below_non_directory_refusal(
    source: &str,
    target: &TargetPath,
    parent: Option<&Path>,
) -> Refusal {
    let clause = match parent {
        Some(parent) => AbsentReason::ParentNotADirectory {
            parent: parent.to_path_buf(),
        }
        .clause(),
        None => "is below a component that is not a directory".to_string(),
    };
    Refusal::found(
        located(Condition::NotADirectory, Location::Target),
        format!(
            "Skipping '{source}': the target '{}' {clause}.",
            target.display()
        ),
    )
}

/// What a refusal before any command adds to its sentence, when there was a
/// command or a credential to not run or write.
#[derive(Clone, Copy)]
pub(super) struct Unrun {
    /// A clause after the condition, such as ", so selfie will not write a
    /// credential there".
    pub(super) unwritten: &'static str,
    /// A sentence after it, such as " No command was run.".
    pub(super) unrun: &'static str,
}

impl Unrun {
    /// Nothing to add.
    pub(super) const NOTHING: Self = Self {
        unwritten: "",
        unrun: "",
    };
}

// The refusal for what stands in the way of a file at `target`. The condition comes
// from `found`, the one mapping every path shares; only the sentence is built here.
fn path_refusal(source: &str, target: &TargetPath, found: &InTheWay<'_>, extra: Unrun) -> Refusal {
    let Unrun { unwritten, unrun } = extra;
    let message = match found {
        InTheWay::Directory => format!("{}{unrun}", directory_message(source, target)),
        InTheWay::NotADirectory(reason) => format!(
            "Skipping '{source}': the target '{}' {}{unwritten}.{unrun}",
            target.display(),
            reason.clause()
        ),
        InTheWay::Undetermined(error) => format!(
            "Skipping '{source}': selfie could not determine what is at the target{unwritten}.\
             {unrun} The check failed with: {}",
            FileSystemError::IoError(Arc::clone(error))
        ),
    };
    Refusal::found(located(found.condition(), Location::Target), message)
}

/// The refusal for what `directory_state` reports at `target`, worded with
/// `extra`, or `None` when nothing there bars a file.
pub(super) fn path_state_refusal(
    source: &str,
    target: &TargetPath,
    state: &DirectoryState,
    extra: Unrun,
) -> Option<Refusal> {
    in_the_way(state).map(|found| path_refusal(source, target, &found, extra))
}

// Every site that words a refused deploy shares this, so apply, drift and the
// writer cannot describe the same refusal differently. Format it here rather than
// at a call site — no test pins this wrapper at the write site, so a copy there
// could drift unnoticed.
fn refusal_message(source: &str, refusal: &FileSystemError) -> String {
    format!("Skipping '{source}': {refusal}")
}

/// The refusal of the entry `source` for what a guard found at its target.
pub(super) fn refusal_for(source: &str, refusal: &FileSystemError) -> Refusal {
    Refusal::found(
        located(guarded(refusal), Location::Target),
        refusal_message(source, refusal),
    )
}

fn unreadable_message(source: &str, target: &TargetPath, error: &dyn std::fmt::Display) -> String {
    format!(
        "Skipping '{source}': target '{}' could not be read: {error}",
        target.display()
    )
}

// Why an entry whose target would not open is refused, worded the same by apply
// and drift.
pub(super) fn unreadable_target_refusal(
    source: &str,
    target: &TargetPath,
    refusal: &OpenRefusal,
) -> Refusal {
    let condition = match refusal {
        OpenRefusal::Unreadable(_) => Condition::Unreadable,
        OpenRefusal::Undetermined(_) => Condition::Undetermined,
    };
    Refusal::found(
        located(condition, Location::Target),
        unreadable_message(source, target, refusal),
    )
}

// The target's bytes if the entry can go on to a decision: `None` for an absent
// target, `Err(refusal)` for anything else. Apply and drift both classify through
// here, so they cannot answer differently about one file.
pub(super) fn readable_target<F: FileSystem>(
    filesystem: &F,
    source: &str,
    target: &TargetPath,
) -> Result<Option<Vec<u8>>, Refusal> {
    readable_or_refusal(source, target, read_target_state(filesystem, target))
}

// The frame for a target refused by the rule, which `classify_entry` gives for
// apply and drift alike. `TargetRejection` supplies the words; this supplies the
// frame.
pub(super) fn target_refusal(target: &str, rejection: TargetRejection) -> Refusal {
    Refusal::found(
        (Condition::TargetRule, Location::Entry),
        format!("Skipping '{target}': {}", rejection.message()),
    )
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::*;
    use crate::fs::{MockFileSystem, target::repository_path};

    fn target() -> TargetPath {
        repository_path(Path::new("/home/u/.config/app/creds"))
    }

    // A link whose destination is a fifo answers both questions. The link is the
    // answer, with the fifo carried beside it: a repository-file path refuses the
    // link as a link, and the secret path, which replaces links, needs to know the
    // writer would refuse this one.
    #[test]
    fn a_link_to_a_fifo_is_a_link_with_the_fifo_behind_it() {
        let mut fs = MockFileSystem::default();
        fs.expect_symlink_refusal().returning(|path| {
            Some(FileSystemError::SymlinkedTarget {
                path: path.path().to_path_buf(),
                points_to: Some(PathBuf::from("/tmp/pipe")),
            })
        });
        fs.expect_irregular_target_refusal().returning(|path| {
            Some(FileSystemError::IrregularTarget {
                path: path.path().to_path_buf(),
                kind: "named pipe (fifo)",
            })
        });

        let TargetGuard::Link { link, behind } = guard_target(&fs, &target()) else {
            panic!("a symlink must answer as a link, whatever it points at");
        };
        assert_eq!(link.destination(), Some(Path::new("/tmp/pipe")));
        assert!(
            matches!(behind, Some(FileSystemError::IrregularTarget { .. })),
            "the fifo behind the link must be carried"
        );
    }

    // A path that refuses every link does not look behind one: the following stat
    // can reach a hung mount, and its answer would change nothing.
    #[test]
    fn guard_refusal_refuses_a_link_without_asking_what_it_points_at() {
        let mut fs = MockFileSystem::default();
        fs.expect_symlink_refusal().returning(|path| {
            Some(FileSystemError::SymlinkedTarget {
                path: path.path().to_path_buf(),
                points_to: Some(PathBuf::from("/mnt/hung/pipe")),
            })
        });
        fs.expect_irregular_target_refusal().never();

        assert!(matches!(
            guard_refusal(&fs, &target()),
            Some(FileSystemError::SymlinkedTarget { .. })
        ));
    }

    // The control for the test above: nothing at either question is `Clear`, and a
    // fifo that is not behind a link is `Refused`.
    #[test]
    fn a_plain_target_is_clear_and_a_bare_fifo_is_refused() {
        let mut clear = MockFileSystem::default();
        clear.expect_symlink_refusal().returning(|_| None);
        clear.expect_irregular_target_refusal().returning(|_| None);
        assert!(matches!(
            guard_target(&clear, &target()),
            TargetGuard::Clear
        ));

        let mut fifo = MockFileSystem::default();
        fifo.expect_symlink_refusal().returning(|_| None);
        fifo.expect_irregular_target_refusal().returning(|path| {
            Some(FileSystemError::IrregularTarget {
                path: path.path().to_path_buf(),
                kind: "named pipe (fifo)",
            })
        });
        assert!(matches!(
            guard_target(&fifo, &target()),
            TargetGuard::Refused(FileSystemError::IrregularTarget { .. })
        ));
    }

    // `symlink_refusal` answers `None` or `SymlinkedTarget` and nothing else, so
    // this is the only thing holding the fail-closed arm: hand it another variant
    // directly and the entry must still be refused. A fallback to "no link" would
    // return `Ok(None)` here and send the target down a path that reads it.
    #[test]
    fn a_symlink_refusal_that_is_not_a_symlinked_target_still_refuses() {
        let refused = classify_link(Some(FileSystemError::IrregularTarget {
            path: PathBuf::from("/home/u/.config/app/creds"),
            kind: "named pipe (fifo)",
        }));

        let Err(carried) = refused else {
            panic!("an unrecognized refusal must refuse the entry, not report no link");
        };
        assert!(
            matches!(carried, FileSystemError::IrregularTarget { .. }),
            "the refusal must be carried to the caller so it can be reported"
        );
    }

    #[test]
    fn no_refusal_is_no_link_and_a_symlinked_target_carries_its_destination() {
        assert!(matches!(classify_link(None), Ok(None)));

        let link = classify_link(Some(FileSystemError::SymlinkedTarget {
            path: PathBuf::from("/home/u/.config/app/creds"),
            points_to: Some(PathBuf::from("../shared/dir")),
        }));
        let Ok(Some(link)) = link else {
            panic!("a symlinked target must be reported as a link");
        };
        // The raw link text, relative as the user wrote it. It is for the warning's
        // wording and is never resolved.
        assert_eq!(link.destination(), Some(Path::new("../shared/dir")));
        assert!(matches!(
            link.refusal(),
            FileSystemError::SymlinkedTarget { .. }
        ));
    }
}
