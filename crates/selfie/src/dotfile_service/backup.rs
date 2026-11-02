//! Copies of the content a target held before an apply overwrote it.
//!
//! One copy per target, under `backups/` in the state directory, replacing any
//! earlier copy of the same target.

use std::fmt;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use crate::dotfile_service::deploy::compute_checksum;
use crate::fs::{
    filesystem::{FileSystem, FileSystemError, IfPresent},
    target::repository_path,
};
use crate::package::event::{EntryOperation, Failure};

/// How many characters of the target's own file name the directory carries.
const READABLE_LEN: usize = 40;

/// How many hex characters of a fresh uuid follow the timestamp.
const SUFFIX_LEN: usize = 6;

/// A copy of a target's former content, on disk and not yet the only one.
pub(super) struct Kept {
    path: PathBuf,
    directory: PathBuf,
}

impl Kept {
    /// Where the copy is.
    pub(super) fn path(&self) -> &Path {
        &self.path
    }

    /// Remove every earlier copy of this target, leaving this one.
    ///
    /// Call it only once the overwrite this copy protects has actually landed.
    /// Returns a sentence to report when an earlier copy could not be removed,
    /// which is untidy rather than unsafe: this copy is on disk either way.
    pub(super) fn prune_earlier<F: FileSystem>(&self, filesystem: &F) -> Option<String> {
        // Selects by name inequality rather than by picking the newest, so it does
        // not depend on enumeration order -- `readdir` is sorted-ish on APFS and
        // hash-ordered on ext4.
        //
        // Two applies running at once are unsupported, and this is one of the
        // places that shows: the other run's fresh copy is not this one, so it goes.
        let entries = match filesystem.list_directory(&self.directory) {
            Ok(entries) => entries,
            Err(e) => {
                return Some(format!(
                    "Kept a copy of the previous content, but cannot list '{}' to remove \
                     earlier copies: {e}",
                    self.directory.display()
                ));
            }
        };

        let failed: Vec<String> = entries
            .iter()
            .filter(|entry| *entry != &self.path)
            .filter_map(|entry| {
                filesystem
                    .remove_file(entry)
                    .err()
                    .map(|e| format!("'{}': {e}", entry.display()))
            })
            .collect();

        (!failed.is_empty()).then(|| {
            format!(
                "Kept a copy of the previous content, but cannot remove an earlier copy \
                 beside it: {}",
                failed.join("; ")
            )
        })
    }
}

/// Copy `current` aside before `target`, keyed as `target_key`, is overwritten.
///
/// `root` is the `backups` directory; it and the per-target directory beneath it
/// are created as needed. Earlier copies of the same target survive until the
/// caller calls [`Kept::prune_earlier`].
///
/// # Errors
///
/// The entry's [`Failure`] if the copy cannot be written, naming `target`, the
/// path that failed, and the directory to fix. The caller must then leave the
/// target alone: there would be nothing to recover it from.
pub(super) fn keep<F: FileSystem>(
    filesystem: &F,
    root: &Path,
    target_key: &str,
    target: &Path,
    current: &[u8],
) -> Result<Kept, Failure> {
    keep_at(filesystem, root, target_key, current, &stamp()).map_err(|error| {
        failure(
            filesystem,
            root,
            &directory(root, target_key),
            target,
            &error,
        )
    })
}

// `keep` with the timestamp supplied, so a test can hand two calls the same one.
// Private to this module: production has exactly one clock.
fn keep_at<F: FileSystem>(
    filesystem: &F,
    root: &Path,
    target_key: &str,
    current: &[u8],
    stamp: &str,
) -> Result<Kept, FileSystemError> {
    let per_target = directory(root, target_key);
    // The timestamp alone is not a name. Two applies inside one millisecond would
    // produce the same one, and the second `write_file_private` would replace the
    // first's copy -- before the second's target write had landed, so a failure
    // there would leave neither the older content nor a copy of it.
    let path = per_target.join(format!("{stamp}-{}", suffix()));

    // `write_file_private`, so the copy is owner-only and lands atomically. It
    // holds whatever was at the target, which may be a credential even for a
    // repository-file entry -- the entry says where selfie writes, not what the
    // user had there first. Only the file is owner-only; `create_dir_all` gives
    // the directories `0o777 & !umask`.
    filesystem
        .write_file_private(&repository_path(&path), current, IfPresent::Replace)
        .map(drop)?;

    // Nothing is removed here. An earlier run's copy may be the only record of
    // content that is nowhere else, while this one holds what is still at the
    // target -- so pruning before the overwrite lands trades the irreplaceable
    // for a duplicate whenever the write then fails.
    Ok(Kept {
        path,
        directory: per_target,
    })
}

// The failure of an entry whose target could not be copied before it was
// overwritten. The sentence names the target, `error` names the path under the
// state directory that failed, and the remedy names the directory to fix and
// how, as far as the filesystem can tell. `directory` is the one the copy went in.
fn failure<F: FileSystem>(
    filesystem: &F,
    root: &Path,
    directory: &Path,
    target: &Path,
    error: &FileSystemError,
) -> Failure {
    let remedy = remedy(filesystem, root, directory, error);
    Failure::new(
        EntryOperation::Backup,
        format_args!(
            "Failed to keep a copy of '{}' before overwriting it",
            target.display()
        ),
        error,
    )
    .with_remedy(&format!(
        "The target is unchanged. {remedy} To keep copies elsewhere instead, point \
         state_directory at a writable directory. Then run apply again."
    ))
}

/// What to do about a copy that could not be made. Its `Display` is one complete
/// sentence, an instruction naming the path to act on.
enum Remedy {
    /// A directory that refuses a new entry, or that may not be searched.
    Denied(PathBuf),
    /// A path on a read-only file system.
    ReadOnly(PathBuf),
    /// Something where selfie has to create a directory or its copy.
    InTheWay(PathBuf),
    /// A path on a file system that is full, or over quota.
    Full(PathBuf),
    /// A directory selfie could not write to, for no reason it could find.
    Other(PathBuf),
}

impl fmt::Display for Remedy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Denied(path) => write!(
                f,
                "Give your user write and search access to '{}'.",
                path.display()
            ),
            Self::ReadOnly(path) => write!(
                f,
                "Remount the file system holding '{}' read-write.",
                path.display()
            ),
            Self::InTheWay(path) => write!(f, "Move '{}' out of the way.", path.display()),
            Self::Full(path) => write!(
                f,
                "Free space on the file system holding '{}'.",
                path.display()
            ),
            Self::Other(path) => write!(f, "Check that selfie can write to '{}'.", path.display()),
        }
    }
}

// The write's own error decides wherever it names its cause and its path. The
// rest, a permission refusal above all, names the directory selfie tried to
// create or the copy, never the directory that refused, so the permissions are
// asked which one that is. Asked only for the wording: the copy was not made and
// the target is not written, whatever this answers.
fn remedy<F: FileSystem>(
    filesystem: &F,
    root: &Path,
    directory: &Path,
    error: &FileSystemError,
) -> Remedy {
    match error {
        FileSystemError::BelowNonDirectory {
            parent: Some(parent),
            ..
        } => return Remedy::InTheWay(parent.clone()),
        FileSystemError::DirectoryTarget { path, .. } => return Remedy::InTheWay(path.clone()),
        FileSystemError::IoError(failure) => {
            // A remedy about the file system holding a path names one that is
            // there: the write names a directory it could not create or a copy it
            // could not make.
            let held = || existing(filesystem, failure.path.as_deref().unwrap_or(root));
            match failure.error.kind() {
                ErrorKind::StorageFull | ErrorKind::QuotaExceeded => return Remedy::Full(held()),
                ErrorKind::ReadOnlyFilesystem => return Remedy::ReadOnly(held()),
                _ => {}
            }
        }
        _ => {}
    }
    // The remedy names the path selfie composed, never a link's destination.
    match filesystem.file_creation_refusal(directory) {
        Some(FileSystemError::CannotCreateIn { path, cause }) => match cause.kind() {
            ErrorKind::PermissionDenied => Remedy::Denied(path),
            ErrorKind::ReadOnlyFilesystem => Remedy::ReadOnly(path),
            ErrorKind::NotADirectory | ErrorKind::AlreadyExists => Remedy::InTheWay(path),
            _ => Remedy::Other(path),
        },
        Some(_) | None => Remedy::Other(directory.to_path_buf()),
    }
}

// The nearest of `path` and its ancestors that exists, or `path` when none does.
fn existing<F: FileSystem>(filesystem: &F, path: &Path) -> PathBuf {
    path.ancestors()
        .find(|ancestor| filesystem.path_exists(ancestor))
        .unwrap_or(path)
        .to_path_buf()
}

// The directory one target's copies go in. `keep_at` writes there and `remedy`
// asks about it, so the two cannot disagree.
fn directory(root: &Path, target_key: &str) -> PathBuf {
    root.join(component(target_key))
}

// The directory holding one target's copy.
//
// The checksum carries the whole of the uniqueness; the readable half carries none
// of it and only helps a person read the listing. Two targets collide on a SHA-256
// collision alone, and one copy would then replace the other's.
//
// Whatever the target looks like, the result is a usable directory name: both
// forms end in 64 hex characters, so neither is `.`, `..` or empty, and neither
// the hex nor a sanitized name holds a separator. That has to hold here --
// `repository_path` normalizes nothing and hands the writer exactly this.
fn component(target: &str) -> String {
    let checksum = compute_checksum(target.as_bytes());
    let readable = readable_part(target);
    if readable.is_empty() {
        checksum
    } else {
        format!("{readable}-{checksum}")
    }
}

// The target's own file name, reduced to characters that are safe in a path
// component. Empty when the target has no file name.
//
// Sanitize first, then shorten. Each rejected character becomes exactly one `_`,
// so what comes back is ASCII and shortening it cannot split a character. Doing
// it the other way round would cut a multi-byte character in half.
fn readable_part(target: &str) -> String {
    let Some(name) = Path::new(target).file_name() else {
        return String::new();
    };

    name.to_string_lossy()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .take(READABLE_LEN)
        .collect()
}

// When the copy was made, as the leading half of its name. Sorts by time and
// carries no `:` to confuse a file browser. Uniqueness is `suffix`'s job, not
// this one's -- two calls in the same millisecond get the same stamp.
fn stamp() -> String {
    chrono::Utc::now().format("%Y%m%dT%H%M%S%.3fZ").to_string()
}

// What makes the name unique, whatever the clock says. Hex, so it cannot
// introduce a separator or a character a file browser dislikes.
fn suffix() -> String {
    uuid::Uuid::new_v4().simple().to_string()[..SUFFIX_LEN].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fs::MockFileSystem;

    const CHECKSUM_LEN: usize = 64;

    // A component's checksum half, which is everything after the last `-`.
    fn checksum_of(component: &str) -> &str {
        component.rsplit('-').next().expect("a non-empty component")
    }

    // A single path component may be at most 255 bytes on APFS and on ext4, and a
    // target's file name can be longer than that on its own.
    //
    // The bound is written out rather than derived from `READABLE_LEN`. Computed
    // from the constant, it would rise with any value that constant took and the
    // assertion would hold for all of them.
    #[test]
    fn a_long_file_name_stays_inside_one_path_component() {
        let long = "x".repeat(400);
        let component = component(&format!("/home/u/{long}"));

        assert!(
            component.len() <= 105,
            "a component must stay inside a filesystem's per-component limit, got {} bytes",
            component.len()
        );
        assert_eq!(
            READABLE_LEN + 1 + CHECKSUM_LEN,
            105,
            "the bound above is the arithmetic of these two, spelled out"
        );
        assert!(!component.contains('/'), "{component}");
    }

    // The property the sanitize-then-shorten order has: one `_` per rejected
    // character, so a four-byte character costs one byte and not four. Sanitizing
    // per byte instead would make this component longer than the arithmetic here
    // allows.
    #[test]
    fn sanitizing_a_file_name_leaves_one_ascii_path_component() {
        let component = component("/home/u/a b:\u{1F600}c.toml");
        let readable = component
            .strip_suffix(&format!("-{}", checksum_of(&component)))
            .expect("a readable half and a checksum");

        assert!(component.is_ascii(), "{component}");
        assert!(!component.contains('/'), "{component}");
        assert_eq!(
            readable, "a_b__c.toml",
            "each rejected character must cost exactly one byte"
        );
    }

    #[test]
    fn two_targets_differing_only_outside_the_file_name_get_different_components() {
        let a = component("/home/u/a/.npmrc");
        let b = component("/home/u/b/.npmrc");

        assert_ne!(a, b);
        assert!(
            a.starts_with(".npmrc-") && b.starts_with(".npmrc-"),
            "both carry the same readable half: {a}, {b}"
        );
    }

    #[test]
    fn a_target_with_no_file_name_still_gets_a_component() {
        let component = component("/");

        assert_eq!(component.len(), CHECKSUM_LEN, "{component}");
        assert!(
            component.chars().all(|c| c.is_ascii_hexdigit()),
            "{component}"
        );
    }

    // Whatever the target, the component is usable as one directory name. A `.`
    // or `..` would name the backups directory itself or its parent, and the copy
    // would land outside the tree selfie manages.
    #[test]
    fn a_component_is_never_a_relative_directory_name() {
        for target in ["/", "/.", "/..", "/a/..", "/a/.", "/a/b"] {
            let component = component(target);
            assert!(
                component != "." && component != ".." && !component.is_empty(),
                "{target} produced {component}"
            );
            assert!(!component.contains('/'), "{target} produced {component}");
        }
    }

    // Keeping a copy touches nothing that is already there. Whether an earlier copy
    // is still needed is the caller's question, and it can only be answered once the
    // overwrite has landed.
    #[test]
    fn keeping_a_copy_removes_nothing() {
        let mut fs = MockFileSystem::default();
        fs.expect_write_file_private()
            .returning(|_, _, _| Ok(crate::fs::Placed::Written));
        // No `list_directory` and no `remove_file` expectation, so reaching either
        // panics rather than passing quietly.

        let kept = keep(
            &fs,
            Path::new("/state/backups"),
            "/home/u/.npmrc",
            Path::new("/home/u/.npmrc"),
            b"old",
        )
        .expect("kept");

        assert_eq!(
            kept.path().parent().expect("a per-target directory"),
            Path::new("/state/backups").join(component("/home/u/.npmrc"))
        );
    }

    // An earlier copy that cannot be removed is reported and is not a failure: this
    // copy is on disk, and `perform_deploy` refuses only on `keep`'s `Err`, which
    // this path does not reach.
    #[test]
    fn a_prune_failure_is_reported_and_is_not_a_refusal() {
        let mut fs = MockFileSystem::default();
        fs.expect_write_file_private()
            .returning(|_, _, _| Ok(crate::fs::Placed::Written));
        // Neither name can be the timestamped file just written, so both are
        // offered to `remove_file`.
        fs.expect_list_directory()
            .returning(|dir| Ok(vec![dir.join("earlier"), dir.join("earlier-still")]));
        fs.expect_remove_file().returning(|_| {
            Err(FileSystemError::IoError(
                std::io::Error::other("Permission denied").into(),
            ))
        });

        let kept = keep(
            &fs,
            Path::new("/state/backups"),
            "/home/u/.npmrc",
            Path::new("/home/u/.npmrc"),
            b"old",
        )
        .expect("kept");
        let stale = kept
            .prune_earlier(&fs)
            .expect("the failure must be reported");

        assert!(
            stale.contains("earlier")
                && stale.contains("Permission denied")
                && stale.contains("Kept a copy"),
            "{stale}"
        );
    }

    // The copy just written is never its own casualty.
    #[test]
    fn pruning_keeps_the_copy_it_was_called_on() {
        let mut fs = MockFileSystem::default();
        fs.expect_write_file_private()
            .returning(|_, _, _| Ok(crate::fs::Placed::Written));
        let removed = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen = std::sync::Arc::clone(&removed);
        fs.expect_remove_file().returning(move |path| {
            seen.lock().unwrap().push(path.to_path_buf());
            Ok(())
        });

        let kept = keep(
            &fs,
            Path::new("/state/backups"),
            "/home/u/.npmrc",
            Path::new("/home/u/.npmrc"),
            b"old",
        )
        .expect("kept");
        let mine = kept.path().to_path_buf();
        let earlier = mine.parent().expect("a directory").join("earlier");
        let listing = vec![mine.clone(), earlier.clone()];
        fs.expect_list_directory()
            .returning(move |_| Ok(listing.clone()));

        assert!(kept.prune_earlier(&fs).is_none());
        assert_eq!(
            *removed.lock().unwrap(),
            vec![earlier],
            "only the earlier copy may be removed"
        );
    }

    // Two copies made inside one millisecond are two files. The timestamp is only
    // the sortable half of the name: if it were the whole name, the second write
    // would replace the first's copy, and a failure in the second's target write
    // would then leave neither the older content nor a copy of it.
    #[test]
    fn two_copies_sharing_a_timestamp_are_two_files() {
        let written = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen = std::sync::Arc::clone(&written);
        let mut fs = MockFileSystem::default();
        fs.expect_write_file_private().returning(move |path, _, _| {
            seen.lock().unwrap().push(path.path().to_path_buf());
            Ok(crate::fs::Placed::Written)
        });

        let root = Path::new("/state/backups");
        let first = keep_at(&fs, root, "/home/u/.npmrc", b"one", "20260920T143005.117Z")
            .expect("first copy");
        let second = keep_at(&fs, root, "/home/u/.npmrc", b"two", "20260920T143005.117Z")
            .expect("second copy");

        assert_ne!(
            first.path(),
            second.path(),
            "the same timestamp must not name the same file"
        );
        let written = written.lock().unwrap();
        assert_eq!(written.len(), 2, "{written:?}");
        assert_ne!(
            written[0], written[1],
            "the second write replaced the first"
        );
        // Same target, so the same directory: only the file name distinguishes them.
        assert_eq!(written[0].parent(), written[1].parent());
    }

    // Both halves are visible in the name: the timestamp leads so a listing sorts
    // by time, and the suffix follows so the name is unique.
    #[test]
    fn a_copy_is_named_timestamp_then_suffix() {
        let mut fs = MockFileSystem::default();
        fs.expect_write_file_private()
            .returning(|_, _, _| Ok(crate::fs::Placed::Written));

        let stamp = "20260920T143005.117Z";
        let kept = keep_at(
            &fs,
            Path::new("/state/backups"),
            "/home/u/.npmrc",
            b"x",
            stamp,
        )
        .expect("kept");
        let name = kept
            .path()
            .file_name()
            .expect("a file name")
            .to_string_lossy()
            .into_owned();

        let rest = name
            .strip_prefix(&format!("{stamp}-"))
            .unwrap_or_else(|| panic!("name must lead with the timestamp: {name}"));
        assert_eq!(rest.len(), SUFFIX_LEN, "{name}");
        assert!(
            rest.chars().all(|c| c.is_ascii_hexdigit()),
            "the suffix must be hex, so it cannot hold a separator: {name}"
        );
    }

    // `stamp` makes three claims the file name depends on: it sorts by time, it
    // carries no `:`, and it names milliseconds. `deployed_at` in the deploy state
    // is RFC 3339, and copying that format here for consistency would break all
    // three at once -- `:` in every name, `+00:00` on the end, second resolution --
    // while every other test in this module keeps passing, because they all reach
    // the copy through a directory listing and never read its name.
    #[test]
    fn a_stamp_sorts_by_time_and_is_safe_in_a_file_name() {
        let first = stamp();
        std::thread::sleep(std::time::Duration::from_millis(2));
        let second = stamp();

        assert!(
            !first.contains(':'),
            "a `:` confuses a file browser: {first}"
        );
        assert!(!first.contains('+'), "{first}");
        assert!(first.ends_with('Z'), "{first}");
        assert!(
            first < second,
            "names must sort by time: {first} then {second}"
        );
        assert_ne!(
            first, second,
            "two stamps 2ms apart must differ, which second resolution cannot do"
        );
    }

    const ROOT: &str = "/state/backups";
    const KEY: &str = "/home/u/.config/app.toml";

    // The directory the copy of `KEY` goes in.
    fn copy_directory() -> PathBuf {
        directory(Path::new(ROOT), KEY)
    }

    // A probe answer: `path` refuses a new file with `kind`.
    fn refused(path: &Path, kind: ErrorKind) -> Option<FileSystemError> {
        Some(FileSystemError::CannotCreateIn {
            path: path.to_path_buf(),
            cause: std::sync::Arc::new(std::io::Error::from(kind)),
        })
    }

    // The write's own error, of `kind`, naming the copy's directory as the
    // writer names a directory it could not create.
    fn write_error(kind: ErrorKind) -> FileSystemError {
        FileSystemError::IoError(crate::fs::IoFailure::at(
            &copy_directory(),
            std::io::Error::from(kind),
        ))
    }

    // `failure`'s message when the probe answers `refusal` and the write failed
    // with `error`. Only `/state` and the paths above it exist.
    fn message_for(refusal: Option<FileSystemError>, error: &FileSystemError) -> String {
        let mut fs = MockFileSystem::default();
        fs.expect_file_creation_refusal()
            .return_once(move |_| refusal);
        fs.expect_path_exists()
            .returning(|path| Path::new("/state").starts_with(path));
        failure(
            &fs,
            Path::new(ROOT),
            &copy_directory(),
            Path::new(KEY),
            error,
        )
        .message
    }

    // Each cause names its own path and fix. A blocked `/state` is neither the
    // backups root nor the copy's directory, so naming either in its place fails,
    // and every path is matched with its closing quote. Where the write's own
    // error decides, the probe answers `/elsewhere`, so asking it fails too, and
    // a remedy about a file system names `/state`, the nearest path that exists.
    #[test]
    fn each_cause_names_the_path_to_fix_and_the_fix() {
        let copy = copy_directory();
        let elsewhere = || refused(Path::new("/elsewhere"), ErrorKind::PermissionDenied);
        let cases = [
            (
                refused(Path::new("/state"), ErrorKind::PermissionDenied),
                write_error(ErrorKind::PermissionDenied),
                "Give your user write and search access to '/state'.".to_string(),
            ),
            (
                refused(Path::new("/state"), ErrorKind::ReadOnlyFilesystem),
                write_error(ErrorKind::Other),
                "Remount the file system holding '/state' read-write.".to_string(),
            ),
            (
                refused(Path::new(ROOT), ErrorKind::NotADirectory),
                write_error(ErrorKind::Other),
                "Move '/state/backups' out of the way.".to_string(),
            ),
            (
                refused(Path::new(ROOT), ErrorKind::AlreadyExists),
                write_error(ErrorKind::Other),
                "Move '/state/backups' out of the way.".to_string(),
            ),
            (
                refused(Path::new("/state"), ErrorKind::Other),
                write_error(ErrorKind::Other),
                "Check that selfie can write to '/state'.".to_string(),
            ),
            (
                None,
                write_error(ErrorKind::Other),
                format!("Check that selfie can write to '{}'.", copy.display()),
            ),
            (
                elsewhere(),
                write_error(ErrorKind::StorageFull),
                "Free space on the file system holding '/state'.".to_string(),
            ),
            (
                elsewhere(),
                write_error(ErrorKind::QuotaExceeded),
                "Free space on the file system holding '/state'.".to_string(),
            ),
            (
                elsewhere(),
                write_error(ErrorKind::ReadOnlyFilesystem),
                "Remount the file system holding '/state' read-write.".to_string(),
            ),
            (
                elsewhere(),
                FileSystemError::BelowNonDirectory {
                    path: copy.join("copy"),
                    parent: Some(PathBuf::from("/state/file")),
                    cause: std::sync::Arc::new(std::io::Error::from(ErrorKind::NotADirectory)),
                },
                "Move '/state/file' out of the way.".to_string(),
            ),
            (
                elsewhere(),
                FileSystemError::DirectoryTarget {
                    path: copy.join("copy"),
                    cause: std::sync::Arc::new(std::io::Error::from(ErrorKind::IsADirectory)),
                },
                format!("Move '{}' out of the way.", copy.join("copy").display()),
            ),
        ];

        for (refusal, error, remedy) in cases {
            let message = message_for(refusal, &error);

            // Whatever the cause, the target is named and untouched, another state
            // directory is offered, and nothing reads as a failed target write.
            assert_eq!(
                message,
                format!(
                    "Failed to keep a copy of '{KEY}' before overwriting it: {error}. The \
                     target is unchanged. {remedy} To keep copies elsewhere instead, point \
                     state_directory at a writable directory. Then run apply again."
                ),
            );
        }
    }

    // The question is about the directory the write used, asked once, and its
    // answer is the path the remedy names.
    #[test]
    fn the_probe_asks_about_the_directory_the_copy_went_to() {
        let mut fs = MockFileSystem::default();
        let expected = copy_directory();
        fs.expect_file_creation_refusal()
            .withf(move |path| path == expected)
            .times(1)
            .returning(|_| refused(Path::new("/elsewhere"), ErrorKind::PermissionDenied));
        fs.expect_write_file_private()
            .returning(|_, _, _| Err(write_error(ErrorKind::PermissionDenied)));

        let failure = keep(&fs, Path::new(ROOT), KEY, Path::new(KEY), b"old")
            .err()
            .expect("the copy failed");

        assert_eq!(failure.operation, EntryOperation::Backup);
        assert!(
            failure
                .message
                .contains("Give your user write and search access to '/elsewhere'."),
            "{}",
            failure.message
        );
    }
}
