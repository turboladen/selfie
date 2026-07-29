// Real file system adapter implementation

use std::{
    fs,
    path::{Path, PathBuf},
    sync::Arc,
};

use etcetera::{AppStrategy, AppStrategyArgs, choose_app_strategy};

use super::filesystem::{FileSystem, FileSystemError};

/// Real file system implementation
#[derive(Clone, Copy, Debug)]
pub struct RealFileSystem;

impl FileSystem for RealFileSystem {
    fn read_file(&self, path: &Path) -> Result<String, FileSystemError> {
        fs::read_to_string(path).map_err(|e| FileSystemError::IoError(Arc::new(e)))
    }

    fn read_file_bytes(&self, path: &Path) -> Result<Vec<u8>, FileSystemError> {
        fs::read(path).map_err(|e| FileSystemError::IoError(Arc::new(e)))
    }

    fn write_file(&self, path: &Path, data: &[u8]) -> Result<(), FileSystemError> {
        // Create parent directories if they don't exist
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|e| FileSystemError::IoError(Arc::new(e)))?;
        }
        fs::write(path, data).map_err(|e| FileSystemError::IoError(Arc::new(e)))?;
        Ok(())
    }

    fn write_file_private(&self, path: &Path, data: &[u8]) -> Result<(), FileSystemError> {
        use std::io::Write as _;

        let io_err = |e: std::io::Error| FileSystemError::IoError(Arc::new(e));
        // The temporary file's name is random and the rename carries no path at all,
        // so failures would otherwise name a file the operator never chose -- or
        // nothing. Re-tag them with the target. `kind()` is preserved; only the raw
        // OS error number is lost, which nothing here consumes.
        let target_err = |e: std::io::Error| {
            io_err(std::io::Error::new(
                e.kind(),
                format!("{}: {e}", path.display()),
            ))
        };

        // The temporary file must live in the target's own directory: elsewhere it may
        // be on another filesystem, making the rename non-atomic, or world-readable.
        //
        // `parent()` yields Some("") for a bare relative name. Filtering that to "."
        // is defensive normalization rather than load-bearing: `create_dir_all("")` is
        // a no-op returning `Ok` and `tempfile_in("")` already resolves to the current
        // directory, so removing the filter would not change behavior today. It is
        // here so the parent is always a real directory rather than relying on those
        // two coincidences holding.
        let parent = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        fs::create_dir_all(parent).map_err(target_err)?;

        let mut builder = tempfile::Builder::new();
        builder.prefix(".selfie-");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            // Applied by the creating syscall, so the content is never briefly
            // world-readable. The umask may restrict this further but can never
            // loosen it.
            builder.permissions(fs::Permissions::from_mode(0o600));
        }

        // Randomly named and unlinked on drop, so it cannot collide with a concurrent
        // write and no *error* path leaves it behind. A crash is another matter: being
        // killed or losing power between here and the rename leaves a `.selfie-*` file
        // beside the target holding the complete secret. It is mode 0600, so this is
        // debris rather than disclosure, but nothing sweeps it up.
        let mut tmp = builder.tempfile_in(parent).map_err(target_err)?;
        tmp.write_all(data).map_err(target_err)?;
        // Flush before the rename: otherwise a crash can leave the target name
        // pointing at a zero-length file.
        tmp.as_file().sync_all().map_err(target_err)?;

        // Replaces the target by rename, so readers see either the old file or the
        // complete new one, a pre-existing mode is discarded rather than inherited,
        // and a symlink at the final component is replaced rather than followed.
        //
        // The directory entry itself is not fsynced, so a crash immediately after this
        // can still lose the rename and leave the old file in place. That costs the
        // deploy, not the data -- a reader never sees a partial file either way -- so
        // it is a durability gap, not a correctness one.
        tmp.persist(path).map_err(|e| target_err(e.error))?;
        Ok(())
    }

    fn is_owner_only(&self, path: &Path) -> Result<bool, FileSystemError> {
        let metadata = fs::metadata(path).map_err(|e| FileSystemError::IoError(Arc::new(e)))?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            // Any group or other bit set means someone else can reach it.
            Ok(metadata.permissions().mode() & 0o077 == 0)
        }
        #[cfg(not(unix))]
        {
            let _ = metadata;
            Ok(true)
        }
    }

    fn remove_file(&self, path: &Path) -> Result<(), FileSystemError> {
        fs::remove_file(path).map_err(|e| FileSystemError::IoError(Arc::new(e)))
    }

    fn path_exists(&self, path: &Path) -> bool {
        path.exists()
    }

    fn expand_path(&self, path: &Path) -> Result<PathBuf, FileSystemError> {
        let binding = path.to_string_lossy();
        let expanded = shellexpand::tilde(&binding);

        PathBuf::from(expanded.as_ref())
            .canonicalize()
            .map_err(|e| FileSystemError::IoError(Arc::new(e)))
    }

    fn list_directory(&self, path: &Path) -> Result<Vec<PathBuf>, FileSystemError> {
        let entries = fs::read_dir(path).map_err(|e| FileSystemError::IoError(Arc::new(e)))?;

        let mut paths = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|e| FileSystemError::IoError(Arc::new(e)))?;
            paths.push(entry.path());
        }

        Ok(paths)
    }

    fn canonicalize(&self, path: &Path) -> Result<PathBuf, FileSystemError> {
        path.canonicalize()
            .map_err(|e| FileSystemError::IoError(Arc::new(e)))
    }

    fn config_dir(&self) -> Result<PathBuf, FileSystemError> {
        // Check for environment variable override first
        if let Ok(dir) = std::env::var("SELFIE_CONFIG_DIR") {
            return Ok(PathBuf::from(dir));
        }

        choose_app_strategy(AppStrategyArgs {
            top_level_domain: "net".to_string(),
            author: "turboladen".to_string(),
            app_name: "selfie".to_string(),
        })
        .map(|xdg| xdg.config_dir())
        .map_err(|_| FileSystemError::HomeDirNotFound)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use tempfile::tempdir;

    #[test]
    fn test_path_exists() {
        let fs = RealFileSystem;

        // Create a temporary directory
        let dir = tempdir().unwrap();
        let file_path = dir.path().join("test.txt");

        // Path shouldn't exist yet
        assert!(!fs.path_exists(&file_path));

        // Create the file
        File::create(&file_path).unwrap();

        // Path should exist now
        assert!(fs.path_exists(&file_path));
    }

    #[test]
    fn test_list_directory() {
        let fs = RealFileSystem;

        // Create a temporary directory
        let dir = tempdir().unwrap();

        // Create some files
        let file1 = dir.path().join("file1.txt");
        let file2 = dir.path().join("file2.txt");

        File::create(&file1).unwrap();
        File::create(&file2).unwrap();

        // List directory
        let paths = fs.list_directory(dir.path()).unwrap();

        // Verify both files are listed
        assert_eq!(paths.len(), 2);
        assert!(paths.contains(&file1));
        assert!(paths.contains(&file2));
    }

    #[test]
    fn test_read_file() {
        let fs = RealFileSystem;

        // Create a temporary directory and file
        let dir = tempdir().unwrap();
        let file_path = dir.path().join("test_read.txt");

        // Write test content
        let test_content = "Hello, world!";
        fs::write(&file_path, test_content).unwrap();

        // Test reading the file
        let content = fs.read_file(&file_path).unwrap();
        assert_eq!(content, test_content);

        // Test reading a non-existent file
        let non_existent = dir.path().join("non_existent.txt");
        let err = fs.read_file(&non_existent).unwrap_err();
        assert!(matches!(err, FileSystemError::IoError(_)));
    }

    #[test]
    fn test_write_file() {
        let fs = RealFileSystem;

        // Create a temporary directory
        let temp_dir = tempdir().unwrap();
        let file_path = temp_dir.path().join("test_write.txt");

        // Test writing to a file
        let test_content = b"Hello, world!";
        fs.write_file(&file_path, test_content).unwrap();

        // Verify the file was written correctly
        let content = std::fs::read(&file_path).unwrap();
        assert_eq!(content, test_content);

        // Test writing to a file in a nested directory that doesn't exist
        let nested_path = temp_dir.path().join("nested").join("dir").join("test.txt");
        fs.write_file(&nested_path, test_content).unwrap();

        // Verify the file was written and directories were created
        let nested_content = std::fs::read(&nested_path).unwrap();
        assert_eq!(nested_content, test_content);
    }

    #[test]
    fn test_remove_file() {
        let fs = RealFileSystem;

        // Create a temporary directory and file
        let temp_dir = tempdir().unwrap();
        let file_path = temp_dir.path().join("test_remove.txt");

        // Create the file
        let test_content = b"File to be removed";
        std::fs::write(&file_path, test_content).unwrap();
        assert!(file_path.exists());

        // Remove the file
        fs.remove_file(&file_path).unwrap();

        // Verify the file was removed
        assert!(!file_path.exists());

        // Test removing a non-existent file should fail
        let non_existent = temp_dir.path().join("non_existent.txt");
        let err = fs.remove_file(&non_existent).unwrap_err();
        assert!(matches!(err, FileSystemError::IoError(_)));
    }

    #[test]
    fn test_expand_path() {
        let fs = RealFileSystem;

        // Create a temporary directory
        let dir = tempdir().unwrap();
        let test_path = dir.path().join("test_dir");
        fs::create_dir(&test_path).unwrap();

        // Test expanding a real path
        let expanded = fs.expand_path(&test_path).unwrap();
        assert!(expanded.is_absolute());

        // Test expanding a non-existent path
        let non_existent = dir.path().join("non_existent");
        let err = fs.expand_path(&non_existent).unwrap_err();
        assert!(matches!(err, FileSystemError::IoError(_)));
    }

    #[test]
    fn test_canonicalize() {
        let fs = RealFileSystem;

        // Create a temporary directory with a subdirectory
        let dir = tempdir().unwrap();
        let subdir = dir.path().join("subdir");
        fs::create_dir(&subdir).unwrap();

        // Test canonicalizing a real path
        let canonical = fs.canonicalize(&subdir).unwrap();
        assert!(canonical.is_absolute());

        // Test canonicalizing a non-existent path
        let non_existent = dir.path().join("non_existent");
        let err = fs.canonicalize(&non_existent).unwrap_err();
        assert!(matches!(err, FileSystemError::IoError(_)));
    }

    #[test]
    fn test_config_dir() {
        let fs = RealFileSystem;

        // Just test that we get a path (without trying to verify its exact value
        // since it may vary by system)
        let config_dir = fs.config_dir().unwrap();
        assert!(config_dir.is_absolute());
        assert!(config_dir.to_string_lossy().contains("selfie"));
    }

    #[test]
    fn test_permission_denied() {
        // This test is conditional because it's hard to reliably create
        // permission-denied scenarios across different platforms
        if cfg!(unix) {
            use std::os::unix::fs::PermissionsExt;

            let fs = RealFileSystem;

            // Create a temporary directory and file
            let dir = tempdir().unwrap();
            let file_path = dir.path().join("no_access.txt");

            // Write test content
            let test_content = "Hello, world!";
            fs::write(&file_path, test_content).unwrap();

            // Set permissions to read-only for owner, nothing for others
            let metadata = fs::metadata(&file_path).unwrap();
            let mut perms = metadata.permissions();
            perms.set_mode(0o400); // Read-only for owner
            fs::set_permissions(&file_path, perms).unwrap();

            // If running as root, this test won't work properly
            if !nix::unistd::Uid::effective().is_root() {
                // Remove read permission for current user
                // This is a best-effort test - it may not work in all environments
                let _ = std::process::Command::new("chmod")
                    .args(["000", file_path.to_str().unwrap()])
                    .output();

                // Try to read the file - may or may not fail with permission denied
                // depending on the environment
                let result = fs.read_file(&file_path);
                if let Err(FileSystemError::IoError(_)) = result {
                    // Test passed
                }
            }
        }
    }

    #[test]
    fn test_read_file_error_handling() {
        let fs = RealFileSystem;

        // Test reading a file that doesn't exist
        let result = fs.read_file(Path::new("/nonexistent/file.txt"));
        assert!(result.is_err());

        match result.unwrap_err() {
            FileSystemError::IoError(io_error) => {
                assert_eq!(io_error.kind(), std::io::ErrorKind::NotFound);
            }
            FileSystemError::HomeDirNotFound => panic!("Expected IoError with NotFound"),
        }
    }

    #[test]
    fn test_list_directory_error_handling() {
        let fs = RealFileSystem;

        // Test listing a directory that doesn't exist
        let result = fs.list_directory(Path::new("/nonexistent/directory"));
        assert!(result.is_err());

        match result.unwrap_err() {
            FileSystemError::IoError(io_error) => {
                assert_eq!(io_error.kind(), std::io::ErrorKind::NotFound);
            }
            FileSystemError::HomeDirNotFound => panic!("Expected IoError with NotFound"),
        }
    }

    #[test]
    fn test_canonicalize_error_handling() {
        let fs = RealFileSystem;

        // Test canonicalizing a path that doesn't exist
        let result = fs.canonicalize(Path::new("/nonexistent/path"));
        assert!(result.is_err());

        match result.unwrap_err() {
            FileSystemError::IoError(io_error) => {
                assert_eq!(io_error.kind(), std::io::ErrorKind::NotFound);
            }
            FileSystemError::HomeDirNotFound => panic!("Expected IoError with NotFound"),
        }
    }

    #[test]
    fn test_filesystem_error_display() {
        let io_error = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "Access denied");
        let fs_error = FileSystemError::IoError(Arc::new(io_error));

        assert_eq!(fs_error.to_string(), "IO error: Access denied");

        let home_error = FileSystemError::HomeDirNotFound;
        assert_eq!(home_error.to_string(), "Home directory not found");
    }

    #[test]
    fn test_filesystem_error_from_io_error() {
        let io_error = std::io::Error::other("test error");
        let fs_error = FileSystemError::IoError(Arc::new(io_error));

        match fs_error {
            FileSystemError::IoError(inner) => {
                assert_eq!(inner.kind(), std::io::ErrorKind::Other);
                assert_eq!(inner.to_string(), "test error");
            }
            FileSystemError::HomeDirNotFound => panic!("Expected IoError variant"),
        }
    }
}

/// Tests for [`FileSystem::write_file_private`].
///
/// Grouped by what each test actually proves.
///
/// The six tests directly below all still pass against [`FileSystem::write_file`]'s
/// `create_dir_all` + `fs::write`, so they guard against gross breakage rather than
/// against the defects this method exists to fix.
///
/// Everything in `unix` is load-bearing: temporarily swapping this implementation
/// back to `create_dir_all` + `fs::write` was confirmed to fail all six of them that
/// run there, and none of the six above. The `/dev/shm` test is Linux-only, so it was
/// not part of that check.
#[cfg(test)]
mod private_write_tests {
    use super::*;
    use tempfile::tempdir;

    /// Names of everything in `dir`, for asserting that no temporary file survived.
    fn entries(dir: &Path) -> Vec<String> {
        let mut names: Vec<_> = fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn writes_content_to_a_new_file() {
        let dir = tempdir().unwrap();
        let target = dir.path().join("creds");

        RealFileSystem
            .write_file_private(&target, b"secret")
            .unwrap();

        assert_eq!(fs::read(&target).unwrap(), b"secret");
    }

    #[test]
    fn replaces_the_content_of_an_existing_file() {
        let dir = tempdir().unwrap();
        let target = dir.path().join("creds");
        fs::write(&target, b"a much longer previous value").unwrap();

        RealFileSystem.write_file_private(&target, b"new").unwrap();

        // Not merely overwritten in place: nothing of the old value survives.
        assert_eq!(fs::read(&target).unwrap(), b"new");
    }

    #[test]
    fn creates_missing_parent_directories() {
        let dir = tempdir().unwrap();
        let target = dir.path().join("nested").join("deeper").join("creds");

        RealFileSystem
            .write_file_private(&target, b"secret")
            .unwrap();

        assert_eq!(fs::read(&target).unwrap(), b"secret");
    }

    #[test]
    fn writes_empty_data() {
        let dir = tempdir().unwrap();
        let target = dir.path().join("creds");

        RealFileSystem.write_file_private(&target, b"").unwrap();

        assert_eq!(fs::read(&target).unwrap(), b"");
    }

    #[test]
    fn leaves_no_temporary_file_behind_on_success() {
        let dir = tempdir().unwrap();
        let target = dir.path().join("creds");

        RealFileSystem
            .write_file_private(&target, b"secret")
            .unwrap();

        assert_eq!(entries(dir.path()), ["creds"]);
    }

    #[test]
    fn leaves_no_temporary_file_behind_when_the_rename_fails() {
        let dir = tempdir().unwrap();
        // A directory at the target path cannot be replaced by a rename, so the
        // write gets as far as the temporary file and then fails. The specific
        // error differs by platform, so only the failure itself is asserted.
        let target = dir.path().join("creds");
        fs::create_dir(&target).unwrap();

        let err = RealFileSystem.write_file_private(&target, b"secret");

        assert!(err.is_err());
        assert_eq!(entries(dir.path()), ["creds"]);
    }

    #[cfg(unix)]
    mod unix {
        use super::*;
        use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};

        fn mode_of(path: &Path) -> u32 {
            fs::metadata(path).unwrap().permissions().mode() & 0o777
        }

        /// The security property, asserted the only way that cannot flake: the
        /// umask may restrict the mode further than the 0o600 we request, but it
        /// can never loosen it, so group and other bits must be clear.
        fn assert_owner_only(path: &Path) {
            assert_eq!(
                mode_of(path) & 0o077,
                0,
                "group/other bits set on {}: {:04o}",
                path.display(),
                mode_of(path)
            );
        }

        #[test]
        fn creates_new_file_owner_only() {
            let dir = tempdir().unwrap();
            let target = dir.path().join("creds");

            RealFileSystem
                .write_file_private(&target, b"secret")
                .unwrap();

            assert_owner_only(&target);
        }

        #[test]
        fn tightens_mode_of_an_existing_world_readable_file() {
            let dir = tempdir().unwrap();
            let target = dir.path().join("creds");
            fs::write(&target, b"old").unwrap();
            fs::set_permissions(&target, fs::Permissions::from_mode(0o644)).unwrap();

            RealFileSystem.write_file_private(&target, b"new").unwrap();

            // `OpenOptions::mode` applies only when a file is created, so an
            // implementation that opens the target directly would silently leave
            // this at 0644 while passing every other test here.
            assert_owner_only(&target);
            assert_eq!(fs::read(&target).unwrap(), b"new");
        }

        #[test]
        fn replaces_a_symlink_instead_of_writing_through_it() {
            let dir = tempdir().unwrap();
            let elsewhere = dir.path().join("elsewhere");
            fs::write(&elsewhere, b"untouched").unwrap();
            let target = dir.path().join("creds");
            std::os::unix::fs::symlink(&elsewhere, &target).unwrap();

            RealFileSystem
                .write_file_private(&target, b"secret")
                .unwrap();

            assert_eq!(fs::read(&elsewhere).unwrap(), b"untouched");
            assert!(
                !fs::symlink_metadata(&target)
                    .unwrap()
                    .file_type()
                    .is_symlink()
            );
            assert_eq!(fs::read(&target).unwrap(), b"secret");
            assert_owner_only(&target);
        }

        #[test]
        fn does_not_follow_a_dangling_symlink() {
            let dir = tempdir().unwrap();
            let never_created = dir.path().join("never-created");
            let target = dir.path().join("creds");
            std::os::unix::fs::symlink(&never_created, &target).unwrap();

            RealFileSystem
                .write_file_private(&target, b"secret")
                .unwrap();

            // `fs::write` would have created the file the symlink points at.
            assert!(!never_created.exists());
            assert_eq!(fs::read(&target).unwrap(), b"secret");
            assert_owner_only(&target);
        }

        #[test]
        fn replaces_by_rename_rather_than_truncating_in_place() {
            let dir = tempdir().unwrap();
            let target = dir.path().join("creds");
            fs::write(&target, b"old").unwrap();

            // Holding the original open keeps its inode allocated, so it cannot be
            // recycled for the replacement and make the comparison below flaky.
            let held = fs::File::open(&target).unwrap();
            let before = fs::metadata(&target).unwrap().ino();

            RealFileSystem.write_file_private(&target, b"new").unwrap();

            let after = fs::metadata(&target).unwrap().ino();
            assert_ne!(
                before, after,
                "target kept its inode, so it was modified in place rather than replaced"
            );
            drop(held);
        }

        #[test]
        fn errors_when_the_parent_directory_is_not_writable() {
            if nix::unistd::Uid::effective().is_root() {
                eprintln!("SKIP errors_when_the_parent_directory_is_not_writable: running as root");
                return;
            }
            let dir = tempdir().unwrap();
            let parent = dir.path().join("locked");
            fs::create_dir(&parent).unwrap();
            let target = parent.join("creds");
            // The target must already exist for this to discriminate. Rewriting an
            // existing file needs write permission on the *file*, not on its
            // directory, so `write_file` succeeds here; an atomic replace still has to
            // create a sibling, so it cannot.
            fs::write(&target, b"old").unwrap();
            fs::set_permissions(&parent, fs::Permissions::from_mode(0o500)).unwrap();

            let result = RealFileSystem.write_file_private(&target, b"secret");

            assert!(result.is_err());
            assert_eq!(fs::read(&target).unwrap(), b"old", "target was modified");

            // Restore write access so the temporary directory can be cleaned up.
            fs::set_permissions(&parent, fs::Permissions::from_mode(0o700)).unwrap();
        }

        /// Pins the temporary file to the target's own directory.
        ///
        /// This is only observable where the target and `$TMPDIR` are on different
        /// filesystems: a temporary file created anywhere else would then fail to
        /// rename into place with `EXDEV`. On CI runners `/dev/shm` is a tmpfs while
        /// `$TMPDIR` is not. Where that does not hold the test cannot discriminate,
        /// so it skips rather than asserting something vacuous — each skip path says
        /// so, since an early return is otherwise indistinguishable from a pass.
        #[cfg(target_os = "linux")]
        #[test]
        fn creates_the_temporary_file_in_the_targets_own_directory() {
            const SKIP: &str = "SKIP creates_the_temporary_file_in_the_targets_own_directory:";

            let Ok(shm) = fs::metadata("/dev/shm") else {
                eprintln!("{SKIP} /dev/shm is not present");
                return;
            };
            let Ok(tmp) = fs::metadata(std::env::temp_dir()) else {
                eprintln!("{SKIP} $TMPDIR is not readable");
                return;
            };
            if shm.dev() == tmp.dev() {
                eprintln!("{SKIP} /dev/shm and $TMPDIR are on the same filesystem");
                return;
            }
            let Ok(dir) = tempfile::tempdir_in("/dev/shm") else {
                eprintln!("{SKIP} /dev/shm is not writable");
                return;
            };

            let target = dir.path().join("creds");

            RealFileSystem
                .write_file_private(&target, b"secret")
                .unwrap();

            assert_eq!(fs::read(&target).unwrap(), b"secret");
            assert_owner_only(&target);
        }
    }
}
