//! Copies of the content a target held before an apply overwrote it.
//!
//! One copy per target, under `backups/` in the state directory, replacing any
//! earlier copy of the same target.

use std::path::{Path, PathBuf};

use crate::dotfile_service::deploy::compute_checksum;
use crate::fs::{
    filesystem::{FileSystem, FileSystemError},
    target::repository_path,
};

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

/// Copy `current` aside before `target_key`'s target is overwritten.
///
/// `root` is the `backups` directory; it and the per-target directory beneath it
/// are created as needed. Earlier copies of the same target survive until the
/// caller calls [`Kept::prune_earlier`].
///
/// # Errors
///
/// [`FileSystemError`] naming a path under `root` if the copy cannot be written.
/// The caller must then leave the target alone: there would be nothing to
/// recover it from.
pub(super) fn keep<F: FileSystem>(
    filesystem: &F,
    root: &Path,
    target_key: &str,
    current: &[u8],
) -> Result<Kept, FileSystemError> {
    keep_at(filesystem, root, target_key, current, &stamp())
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
    let directory = root.join(component(target_key));
    // The timestamp alone is not a name. Two applies inside one millisecond would
    // produce the same one, and the second `write_file_private` would replace the
    // first's copy -- before the second's target write had landed, so a failure
    // there would leave neither the older content nor a copy of it.
    let path = directory.join(format!("{stamp}-{}", suffix()));

    // `write_file_private`, so the copy is owner-only and lands atomically. It
    // holds whatever was at the target, which may be a credential even for a
    // repository-file entry -- the entry says where selfie writes, not what the
    // user had there first. Only the file is owner-only; `create_dir_all` gives
    // the directories `0o777 & !umask`.
    filesystem.write_file_private(&repository_path(&path), current)?;

    // Nothing is removed here. An earlier run's copy may be the only record of
    // content that is nowhere else, while this one holds what is still at the
    // target -- so pruning before the overwrite lands trades the irreplaceable
    // for a duplicate whenever the write then fails.
    Ok(Kept { path, directory })
}

/// Why an entry was not overwritten, for the apply loop's `Skipping` line.
///
/// Names the target; `error` names the path under the state directory that
/// failed, which is the backups directory when it could not be created and the
/// copy itself otherwise.
pub(super) fn refusal(source: &str, target: &Path, error: &FileSystemError) -> String {
    format!(
        "Skipping '{source}': cannot keep a copy of '{}' before overwriting it: {error}. \
         The target is unchanged. Free space under the state directory, or point \
         --state-directory somewhere writable, then run apply again.",
        target.display()
    )
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
        fs.expect_write_file_private().returning(|_, _| Ok(()));
        // No `list_directory` and no `remove_file` expectation, so reaching either
        // panics rather than passing quietly.

        let kept = keep(&fs, Path::new("/state/backups"), "/home/u/.npmrc", b"old").expect("kept");

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
        fs.expect_write_file_private().returning(|_, _| Ok(()));
        // Neither name can be the timestamped file just written, so both are
        // offered to `remove_file`.
        fs.expect_list_directory()
            .returning(|dir| Ok(vec![dir.join("earlier"), dir.join("earlier-still")]));
        fs.expect_remove_file().returning(|_| {
            Err(FileSystemError::IoError(std::sync::Arc::new(
                std::io::Error::other("Permission denied"),
            )))
        });

        let kept = keep(&fs, Path::new("/state/backups"), "/home/u/.npmrc", b"old").expect("kept");
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
        fs.expect_write_file_private().returning(|_, _| Ok(()));
        let removed = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen = std::sync::Arc::clone(&removed);
        fs.expect_remove_file().returning(move |path| {
            seen.lock().unwrap().push(path.to_path_buf());
            Ok(())
        });

        let kept = keep(&fs, Path::new("/state/backups"), "/home/u/.npmrc", b"old").expect("kept");
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
        fs.expect_write_file_private().returning(move |path, _| {
            seen.lock().unwrap().push(path.path().to_path_buf());
            Ok(())
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
        fs.expect_write_file_private().returning(|_, _| Ok(()));

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

    // The refusal has to send the user somewhere they can act, and must not read
    // as the target write having failed -- the target is untouched.
    #[test]
    fn the_refusal_names_the_target_and_an_escape() {
        let error = FileSystemError::IoError(std::sync::Arc::new(std::io::Error::other(
            "/state/backups: No space left on device",
        )));
        let message = refusal(
            "app/config.toml",
            Path::new("/home/u/.config/app.toml"),
            &error,
        );

        assert!(
            message.contains("/home/u/.config/app.toml")
                && message.contains("--state-directory")
                && message.contains("The target is unchanged"),
            "{message}"
        );
        assert!(
            !message.contains("Failed to write"),
            "a copy that could not be made is not a failed target write: {message}"
        );
    }
}
