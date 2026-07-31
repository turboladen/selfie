//! Paths selfie writes to, and the only two things that produce one.
//!
//! `.claude/rules/secrets.md` carries the argument for why a target must reach a
//! writer unresolved. This module is the mechanism.

use std::path::{Path, PathBuf};

use crate::fs::filesystem::{FileSystem, FileSystemError};
use crate::paths::normalize_path;

/// The user's home directory, and nothing else.
///
/// [`expand_target_path`] takes this rather than a whole [`FileSystem`] so that
/// it *cannot* reach `canonicalize` or `expand_path`. Both resolve a path, and a
/// resolved target forfeits everything [`TargetPath`] exists to protect.
pub trait HomeDir {
    /// The user's home directory.
    ///
    /// # Errors
    ///
    /// Returns [`FileSystemError`] if the home directory cannot be determined.
    fn home(&self) -> Result<PathBuf, FileSystemError>;
}

impl<F: FileSystem + ?Sized> HomeDir for F {
    fn home(&self) -> Result<PathBuf, FileSystemError> {
        self.expand_path(Path::new("~"))
    }
}

/// A path selfie is going to write to, which nothing can resolve afterwards.
///
/// [`FileSystem::write_file_private`], [`FileSystem::write_file_no_follow`],
/// [`FileSystem::symlink_refusal`] and [`FileSystem::is_owner_only`] take this
/// instead of a `&Path`, so canonicalizing a path and then writing it fails to
/// compile. Each of those four applies to the path **as given**; handing one a
/// resolved path silently forfeits what it promises.
///
/// The guarantee is that you cannot resolve a `TargetPath` once you hold one --
/// not that the path inside was never resolved. There is deliberately no
/// `Deref`, so `Path`'s own `canonicalize`, `exists` and `metadata` are out of
/// reach:
///
/// ```compile_fail
/// use selfie::fs::{RealFileSystem, expand_target_path};
///
/// let target = expand_target_path(&RealFileSystem, "/etc/hosts");
/// let _ = target.canonicalize();
/// ```
///
/// and only this module can mint one:
///
/// ```compile_fail
/// use selfie::fs::TargetPath;
///
/// let _ = TargetPath {
///     path: std::path::PathBuf::from("/etc/hosts"),
/// };
/// ```
///
/// Neither of those may fail for some unrelated reason -- a renamed export, a
/// bad import -- so this one has to build:
///
/// ```
/// use selfie::fs::{RealFileSystem, expand_target_path};
///
/// let target = expand_target_path(&RealFileSystem, "/etc/hosts");
/// assert_eq!(target.path(), std::path::Path::new("/etc/hosts"));
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TargetPath {
    path: PathBuf,
}

impl TargetPath {
    /// The path itself, for the reads and queries that do not need the guarantee.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// A [`Display`](std::path::Display) for the path, as [`Path::display`] gives.
    #[must_use]
    pub fn display(&self) -> std::path::Display<'_> {
        self.path.display()
    }
}

/// Expand a deploy target without resolving its final component.
///
/// Expands a leading `~` and resolves `.`/`..` textually. It does **not**
/// canonicalize, and must not be made to: every writer this feeds refuses or
/// replaces a symlink at the path as given, and `canonicalize` returns the
/// link's destination, so the writer would be looking at an ordinary file. That
/// is why this takes a [`HomeDir`] rather than a [`FileSystem`].
///
/// Canonicalization forfeits the guarantee exactly when it *succeeds*. A
/// dangling link fails to canonicalize and so reaches the writer unresolved,
/// which is why reintroducing it here is caught by the tests whose links resolve
/// and not by the others.
///
/// Two consequences of never touching the filesystem, both intended: a symlinked
/// *parent* directory is still followed, and `a/link/../b` becomes `a/b` rather
/// than wherever `link` points.
///
/// `~user` is not supported. It falls through to the literal path, which is then
/// relative, and every caller refuses a relative target.
#[must_use]
pub fn expand_target_path<H: HomeDir + ?Sized>(home: &H, target: &str) -> TargetPath {
    let raw = if target.starts_with('~') {
        // Only the leading `~` component is expanded; everything after it is
        // joined unresolved, which is the whole point. A bare `~` or `~/` is the
        // whole path, so expansion does reach the final component there -- those
        // are directories and fail the write anyway.
        let (tilde, rest) = match target.split_once('/') {
            Some((tilde, rest)) => (tilde, Some(rest)),
            None => (target, None),
        };

        let expanded = if tilde == "~" { home.home().ok() } else { None };

        match expanded {
            Some(home) => match rest {
                Some(rest) => home.join(rest),
                None => home,
            },
            // No home directory, or a `~user` form. Falling back to the literal
            // path leaves it relative, and the caller's absolute-path guard then
            // refuses it -- better than writing a credential into a directory
            // literally named `~` beneath the current directory.
            None => PathBuf::from(target),
        }
    } else {
        PathBuf::from(target)
    };

    TargetPath {
        path: normalize_path(&raw),
    }
}

// The deploy state file's path.
//
// The second and weaker of the two constructors: the configured branch joins
// `state_dir` exactly as given, so what comes back is only as unresolved as what
// the caller was configured with, and `SelfieConfigBuilder::state_directory`
// accepts any `PathBuf`.
//
// The fallback expands `~` alone rather than the whole path because
// `expand_path` canonicalizes, and a canonicalized state path lets
// `write_file_private` replace whatever a planted symlink points at instead of
// the link. Creating the directory first so canonicalization succeeds would
// remove the availability problem and keep the security one.
pub(crate) fn state_file_path<H: HomeDir + ?Sized>(
    home: &H,
    state_dir: Option<&Path>,
    filename: &str,
) -> Result<TargetPath, FileSystemError> {
    if let Some(state_dir) = state_dir {
        return Ok(TargetPath {
            path: state_dir.join(filename),
        });
    }

    // XDG_STATE_HOME (`~/.local/state/selfie`) per the XDG Base Directory
    // Specification: deploy state is per-machine, non-portable data.
    let home = home.home().map_err(|_| {
        FileSystemError::IoError(std::sync::Arc::new(std::io::Error::other(
            "Cannot determine home directory for deploy state file",
        )))
    })?;

    Ok(TargetPath {
        path: home
            .join(".local")
            .join("state")
            .join("selfie")
            .join(filename),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fs::MockFileSystem;

    #[test]
    fn an_absolute_target_is_left_alone() {
        let fs = MockFileSystem::default();
        let result = expand_target_path(&fs, "/tmp/some/file");
        assert_eq!(result.path(), Path::new("/tmp/some/file"));
    }

    #[test]
    fn a_leading_tilde_expands_and_the_rest_is_joined() {
        let mut fs = MockFileSystem::default();
        fs.mock_expand_path("~", "/home/user");
        let result = expand_target_path(&fs, "~/test-file");
        assert_eq!(result.path(), Path::new("/home/user/test-file"));
    }

    #[test]
    fn a_named_tilde_is_not_expanded() {
        let fs = MockFileSystem::default();
        let result = expand_target_path(&fs, "~alice/test-file");
        assert_eq!(result.path(), Path::new("~alice/test-file"));
    }

    #[test]
    fn a_configured_state_directory_is_used_as_given() {
        let fs = MockFileSystem::default();
        let state_dir = PathBuf::from("/var/state");
        let result = state_file_path(&fs, Some(&state_dir), "deploy-state.yml").unwrap();
        assert_eq!(result.path(), Path::new("/var/state/deploy-state.yml"));
    }

    #[test]
    fn the_default_state_path_lands_under_xdg_state_home() {
        let mut fs = MockFileSystem::default();
        fs.mock_expand_path("~", "/home/user");
        let result = state_file_path(&fs, None, "deploy-state.yml").unwrap();
        assert_eq!(
            result.path(),
            Path::new("/home/user/.local/state/selfie/deploy-state.yml")
        );
    }
}
