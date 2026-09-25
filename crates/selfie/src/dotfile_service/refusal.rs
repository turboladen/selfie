//! What is at a deploy target, and how a refused deploy is worded.
//!
//! The secret-bearing path, the repository-file path, drift and track all guard
//! and classify a target through here and refuse it in the same words, so none of
//! them can describe one refusal differently from the others.

use std::{
    io,
    path::{Path, PathBuf},
};

use crate::fs::{
    filesystem::{FileSystem, FileSystemError},
    target::{TargetPath, TargetRejection},
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
fn link_at<F: FileSystem>(
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
    /// Nothing is there: the path does not exist, or a component of it is not a
    /// directory, where nothing can be and a write fails on its own.
    Absent,
    Readable(Vec<u8>),
    /// A directory, which a file cannot replace.
    Directory,
    /// Something is there, or may be, and it could not be read.
    Unreadable(FileSystemError),
}

/// What is at `target`, from one read of it.
///
/// Read as raw bytes, so two different files are never reported identical after
/// a lossy decode. Ask [`guard_target`] first: this reads, and a read follows what
/// the guard refuses.
// One read rather than an existence probe and then a read, so a file deleted
// between the two deploys instead of refusing, and a directory is named rather than
// reported as a read failure. Only the two errnos that prove nothing is at the path
// are absent. Anything else -- a parent that denies access, a loop above the
// target -- leaves what is there unknown, and an unknown target is never written
// over: the secret path would put it to a resolver, and the repository-file path and
// drift refuse it outright.
pub(super) fn read_target_state<F: FileSystem>(filesystem: &F, target: &TargetPath) -> TargetState {
    let error = match filesystem.read_file_bytes(target.path()) {
        Ok(bytes) => return TargetState::Readable(bytes),
        Err(error) => error,
    };

    let kind = match &error {
        FileSystemError::IoError(io) => Some(io.kind()),
        _ => None,
    };
    match kind {
        Some(io::ErrorKind::NotFound | io::ErrorKind::NotADirectory) => TargetState::Absent,
        Some(io::ErrorKind::IsADirectory) => TargetState::Directory,
        _ => TargetState::Unreadable(error),
    }
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

/// Why an entry whose target is a directory is refused.
pub(super) fn directory_target_refusal(source: &str, target: &TargetPath) -> String {
    format!("Skipping '{source}': {}", directory_at_target(target))
}

// Every site that words a refused deploy shares this, so apply, drift and the
// writer cannot describe the same refusal differently. Format it here rather than
// at a call site — no test pins this wrapper at the write site, so a copy there
// could drift unnoticed.
//
// Named as a property rather than counted. The count was "three", and was correct
// until the same change that wrote it added three more call sites — a number in a
// comment is a claim that goes stale on the next edit, in a file whose whole
// subject is claims going stale.
pub(super) fn refusal_warning(source: &str, refusal: &FileSystemError) -> String {
    format!("Skipping '{source}': {refusal}")
}

// Why an entry whose target could not be read is refused, worded the
// same by apply and drift. A symlink whose destination cannot be read is a
// symlinked target first, which is the refusal every command already shares;
// only a plain file gets the read failure.
pub(super) fn unreadable_target_refusal<F: FileSystem>(
    filesystem: &F,
    source: &str,
    target: &TargetPath,
    error: &FileSystemError,
) -> String {
    match filesystem.symlink_refusal(target) {
        Some(refusal) => refusal_warning(source, &refusal),
        None => format!(
            "Skipping '{source}': target '{}' could not be read: {error}",
            target.display()
        ),
    }
}

// The target's bytes if the entry can go on to a decision: `None` for an absent
// target, `Err(warning)` for a directory or for one that could not be read. Apply
// and drift both classify through here, so they cannot answer differently about
// one file.
pub(super) fn readable_target<F: FileSystem>(
    filesystem: &F,
    source: &str,
    target: &TargetPath,
) -> Result<Option<Vec<u8>>, String> {
    match read_target_state(filesystem, target) {
        TargetState::Absent => Ok(None),
        TargetState::Readable(bytes) => Ok(Some(bytes)),
        TargetState::Directory => Err(directory_target_refusal(source, target)),
        TargetState::Unreadable(e) => {
            Err(unreadable_target_refusal(filesystem, source, target, &e))
        }
    }
}

// The three deploy-side sites that refuse a target by the rule: apply's
// secret-bearing path, apply's repository-file path, and drift. `TargetRejection`
// supplies the words so all three say the same thing; this supplies the frame.
pub(super) fn target_refusal(target: &str, rejection: TargetRejection) -> String {
    format!("Skipping '{target}': {}", rejection.message())
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
