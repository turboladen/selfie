//! Path containment checks shared across subsystems.
//!
//! A package file names the files it refers to with paths relative to its own
//! directory. Nothing stops a package from writing `../../../etc/passwd`, so
//! every read of a package-relative path has to be checked before it happens —
//! not only the ones that go through the dotfile deploy path.
//!
//! The check is **lexical**: it resolves `.` and `..` textually and does not follow
//! symlinks, so it catches a written traversal and not a planted link. See
//! [`is_within`] for what that does and does not rule out.

use std::path::{Component, Path, PathBuf};

/// Normalize a path by resolving `.` and `..` components without touching the
/// file system.
///
/// Unlike `canonicalize`, this works on paths that do not exist yet. It
/// processes components left to right, popping on `..` and skipping `.`.
#[must_use]
pub(crate) fn normalize_path(path: &Path) -> PathBuf {
    let mut parts: Vec<Component<'_>> = Vec::new();

    for component in path.components() {
        match component {
            Component::ParentDir => {
                // Only pop Normal components; never pop past root or a prefix.
                if matches!(parts.last(), Some(Component::Normal(_))) {
                    parts.pop();
                }
            }
            Component::CurDir => {}
            other => parts.push(other),
        }
    }

    parts.iter().collect()
}

/// Whether `path` stays inside `base_dir` once `.` and `..` are resolved.
///
/// A plain `starts_with` is not sufficient: `base/../../etc/passwd` starts with
/// `base` as a string but escapes it. Both sides are made absolute and normalized
/// first, which is what makes the comparison meaningful.
///
/// # Lexical, and a symlink defeats it
///
/// A link inside `base_dir` passes while pointing outside, including the
/// linked-*directory* form: with `packages/myapp -> /elsewhere`,
/// `symlink_metadata` on the source path reports a regular file. Being lexical
/// is also what lets this work on paths that do not exist yet.
// Both forms are pinned by `a_symlinked_source_escapes_the_containment_guard`,
// which asserts the escape succeeds — strengthening the guard means deleting
// that test and this paragraph together.
//
// The gap is accepted: escaping needs a hostile package repository, which can
// already run arbitrary commands through a dotfile's `command:`.
//
// Do not call `std::fs` from here. That takes the library around its own port,
// and a `symlink_metadata` walk adds a check-then-read race. The non-racy fix is
// `openat2`'s `RESOLVE_BENEATH`, kernel-enforced, and Linux-only.
#[must_use]
pub(crate) fn is_within(path: &Path, base_dir: &Path) -> bool {
    match (std::path::absolute(path), std::path::absolute(base_dir)) {
        (Ok(abs_path), Ok(abs_base)) => {
            normalize_path(&abs_path).starts_with(normalize_path(&abs_base))
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_path_inside_the_base_is_allowed() {
        assert!(is_within(
            Path::new("/configs/app/file.toml"),
            Path::new("/configs")
        ));
    }

    #[test]
    fn a_traversal_out_of_the_base_is_rejected() {
        assert!(!is_within(
            Path::new("/configs/../etc/passwd"),
            Path::new("/configs")
        ));
    }

    #[test]
    fn a_deeply_nested_path_inside_the_base_is_allowed() {
        assert!(is_within(
            Path::new("/configs/deep/nested/file"),
            Path::new("/configs")
        ));
    }

    #[test]
    fn a_traversal_that_returns_inside_the_base_is_allowed() {
        assert!(is_within(
            Path::new("/configs/sub/../file.toml"),
            Path::new("/configs")
        ));
    }

    #[test]
    fn repeated_traversal_cannot_climb_past_the_root() {
        assert!(!is_within(
            Path::new("/configs/../../../etc/passwd"),
            Path::new("/configs")
        ));
    }

    #[test]
    fn normalize_resolves_dot_and_parent_components() {
        assert_eq!(
            normalize_path(Path::new("/a/./b/../c")),
            PathBuf::from("/a/c")
        );
    }
}
