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
/// # This check is lexical, and a symlink defeats it
///
/// It resolves `.` and `..` textually and touches the file system not at all, so a
/// symlink planted *inside* `base_dir` passes while pointing outside it:
/// `packages/creds/link.tpl -> ../outside.tpl` is accepted, and the outside file's
/// contents are then read and rendered into a deployed dotfile. A symlinked
/// *directory* component does the same and is the harder case — with
/// `packages/myapp -> /elsewhere`, `symlink_metadata` on the full path reports a
/// regular file, so a check that only inspected the final component would report
/// containment just as confidently. `a_symlinked_source_escapes_the_containment_
/// guard` in `dotfile_service_tests.rs` records both.
///
/// That is accepted rather than unnoticed. Escaping needs a hostile package
/// repository, and such a repository can already run arbitrary commands through a
/// dotfile's `command:` field, so this guard is not the security boundary in the
/// scenario where it fails. It stops the traversal a *mistake* produces.
///
/// Closing it honestly would mean a `FileSystem` port method resolving each
/// component against the base — `openat2`'s `RESOLVE_BENEATH` or `cap-std`, which
/// are kernel-enforced and so free of the check-then-read race a `symlink_metadata`
/// walk would reintroduce. That is a real fix and deliberately not taken here: this
/// module's value is that it touches no file system at all, and threading a port
/// through for a guard that is not the boundary buys little. Do not "strengthen"
/// this function by calling `std::fs` from it — that trades a limit this comment
/// states for one nothing does, and takes the library around its own port.
///
/// Being lexical is also what lets it work on paths that do not exist yet.
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
