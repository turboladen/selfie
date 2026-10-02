//! Directories a test locks against the current process.

use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};

/// A directory created with a restrictive mode, restored to `0o755` when dropped
/// so a failing test cannot leave behind a directory its temp dir is unable to
/// remove.
#[derive(Debug)]
pub struct LockedDir {
    path: PathBuf,
}

impl LockedDir {
    /// Create `path` and set its mode to `mode`.
    ///
    /// # Panics
    ///
    /// If the directory cannot be created or its mode set.
    #[must_use]
    pub fn create(path: &Path, mode: u32) -> Self {
        std::fs::create_dir_all(path).expect("creating the locked directory");
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
            .expect("setting the locked directory's mode");
        Self {
            path: path.to_path_buf(),
        }
    }

    /// The locked directory.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Whether this process is kept from listing the directory.
    ///
    /// False when running as root, which ignores mode bits. A test that needs the
    /// lock checks this and skips, rather than inferring it from the user id.
    #[must_use]
    pub fn holds(&self) -> bool {
        std::fs::read_dir(&self.path).is_err()
    }

    /// Whether this process is kept from creating a file in the directory.
    ///
    /// The question for a write-protecting mode such as `0o500`, which still lets
    /// the directory be listed, so [`holds`](Self::holds) answers false for it.
    /// False when running as root.
    #[must_use]
    pub fn refuses_new_files(&self) -> bool {
        let probe = self.path.join(".locked-dir-probe");
        let refused = std::fs::write(&probe, "x").is_err();
        let _ = std::fs::remove_file(&probe);
        refused
    }
}

impl Drop for LockedDir {
    fn drop(&mut self) {
        let _ = std::fs::set_permissions(&self.path, std::fs::Permissions::from_mode(0o755));
    }
}
