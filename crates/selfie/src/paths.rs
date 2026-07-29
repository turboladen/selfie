//! Path containment checks shared across subsystems.
//!
//! A package file names the files it refers to with paths relative to its own
//! directory. Nothing stops a package from writing `../../../etc/passwd`, so
//! every read of a package-relative path has to be checked before it happens —
//! not only the ones that go through the dotfile deploy path.

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
