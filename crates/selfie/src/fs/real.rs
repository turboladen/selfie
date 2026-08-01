// Real file system adapter implementation

use std::{
    fs,
    path::{Path, PathBuf},
    sync::Arc,
};

use etcetera::{AppStrategy, AppStrategyArgs, choose_app_strategy};

use super::filesystem::{FileSystem, FileSystemError};
use super::target::TargetPath;

/// Real file system implementation
#[derive(Clone, Copy, Debug)]
pub struct RealFileSystem;

/// [`FileSystemError::SymlinkedTarget`] if `path`'s final component is a symlink.
///
/// `symlink_metadata` does not follow that component, so a symlink is reported as a
/// symlink rather than as whatever it points at. Returns `None` for anything else,
/// including a path that does not exist.
fn symlink_refusal(path: &Path) -> Option<FileSystemError> {
    fs::symlink_metadata(path)
        .ok()
        .filter(|metadata| metadata.file_type().is_symlink())
        .map(|_| FileSystemError::SymlinkedTarget {
            path: path.to_path_buf(),
            points_to: fs::read_link(path).ok(),
        })
}

/// What `path` is, if it is something selfie must not read from or write to.
///
/// `fs::metadata` **follows**, unlike `symlink_refusal`'s `symlink_metadata`, and
/// the difference is the point: the hazard is what an `open` would land on, so a
/// symlink pointing at a fifo has to answer the same as a bare fifo. A dangling
/// link fails the stat and is `None` — nothing to open, and `symlink_refusal`
/// covers it.
///
/// `stat` never blocks, including on a fifo. Only `open` does, which is what makes
/// it safe to ask this question about the very targets that would hang.
fn irregular_kind(path: &Path) -> Option<&'static str> {
    let metadata = fs::metadata(path).ok()?;
    let file_type = metadata.file_type();

    #[cfg(unix)]
    {
        use std::os::unix::fs::FileTypeExt as _;

        if file_type.is_fifo() {
            // The blocking one: `open` waits for the other end.
            return Some("named pipe (fifo)");
        }
        if file_type.is_socket() {
            return Some("socket");
        }
        if file_type.is_char_device() {
            return Some("character device");
        }
        if file_type.is_block_device() {
            return Some("block device");
        }
    }

    // A **directory** is deliberately not one of these, though it is not a regular
    // file either. It cannot produce either hazard this guard exists for: opening
    // one never blocks, and writing to one fails `EISDIR` without touching
    // anything. Classifying it here would only relabel an error that is already
    // loud and already accurate.
    //
    // Two tests hold that, and the second is the surprising one:
    // `a_directory_at_the_target_is_an_ordinary_error` pins that it stays an
    // `IoError`, and `a_write_that_fails_after_an_accepted_conflict_is_refused`
    // reaches `perform_deploy`'s *second* `Err` arm **by putting a directory at
    // the target**. Folding directories in here refuses that entry earlier, and
    // the only test covering that arm stops covering it -- silently, because it
    // would still pass. Anyone tidying this up would see the first failure and
    // never learn about the second.
    //
    // Not dead on unix -- `file_type` is unused there once the `cfg(unix)` block
    // above has returned, and this consumes it so the non-unix build has no
    // unused-variable warning. Deleting it breaks that build only.
    let _ = file_type;
    None
}

/// [`FileSystemError::IrregularTarget`] for anything `irregular_kind` names.
///
/// Free function taking a `&Path` so `write_file_no_follow` can classify a failed
/// `open` with it, the same shape `symlink_refusal` has and for the same reason.
fn irregular_refusal(path: &Path) -> Option<FileSystemError> {
    irregular_kind(path).map(|kind| FileSystemError::IrregularTarget {
        path: path.to_path_buf(),
        kind,
    })
}

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

    fn write_file_private(&self, path: &TargetPath, data: &[u8]) -> Result<(), FileSystemError> {
        use std::io::Write as _;

        let path = path.path();
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

    fn write_file_no_follow(&self, path: &TargetPath, data: &[u8]) -> Result<(), FileSystemError> {
        use std::io::Write as _;

        let path = path.path();
        let io_err = |e: std::io::Error| FileSystemError::IoError(Arc::new(e));

        // Matches `write_file`: a target whose directory does not exist yet is
        // created rather than refused.
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(io_err)?;
        }

        let mut options = fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);

        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            // The kernel refuses the open when the final component is a symlink, so
            // there is no interval between deciding and writing for a planter to
            // win. Checking first and then writing would be exactly that race.
            //
            // `O_NONBLOCK` is here for a second kind of target: opening a fifo for
            // writing blocks until a reader arrives, so without it this call hangs
            // forever on one and no timeout in selfie bounds it. With it the open
            // fails `ENXIO` instead, and the classifier below names the fifo.
            //
            // It does not make the guarantee on its own — with a reader attached
            // the open succeeds — which is what the descriptor check after it is
            // for. On a regular file the flag has no effect, here or on the writes
            // that follow.
            options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
        }
        #[cfg(not(unix))]
        {
            // No equivalent flag here, so this is a check and therefore racy. Said
            // plainly rather than presented as the guarantee the Unix path gives.
            //
            // Written out rather than reusing `symlink_refusal`, which answers "is
            // it a symlink" and folds a failed stat into "no". That is right where
            // it only classifies an open that already failed, and wrong here, where
            // it is the whole enforcement: a stat that fails for any reason other
            // than the path being absent leaves us unable to tell, and proceeding
            // would write through a link we simply could not see.
            match fs::symlink_metadata(path) {
                Ok(metadata) if metadata.file_type().is_symlink() => {
                    return Err(FileSystemError::SymlinkedTarget {
                        path: path.to_path_buf(),
                        points_to: fs::read_link(path).ok(),
                    });
                }
                Ok(_) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(io_err(e)),
            }
        }

        let mut file = match options.open(path) {
            Ok(file) => file,
            // Classified by asking what is at the path rather than by matching an
            // errno. What a caller has to tell apart is a refusal from a failure,
            // and the two stats answer that directly -- so this needs no errno
            // constant, and therefore makes no claim about which one any platform
            // returns for `O_NOFOLLOW` or for a readerless fifo under
            // `O_NONBLOCK`.
            //
            // Symlink first: a link to a fifo fails the open with `ELOOP` before
            // the fifo is ever reached, so reporting it as a fifo would name the
            // wrong problem and suggest the wrong fix.
            Err(e) => {
                return Err(symlink_refusal(path)
                    .or_else(|| irregular_refusal(path))
                    .unwrap_or_else(|| io_err(e)));
            }
        };

        // Ask the descriptor, not the path. Everything above this line is a
        // question about a name, and a name can be replaced between the asking and
        // the answering; this inspects the object actually opened, so a fifo or
        // device planted mid-apply is refused rather than written to. It is the
        // only check here that is not a race.
        //
        // Reached when the open succeeded, which for a fifo means a reader was
        // already attached. Nothing has been written yet: `O_TRUNC` on a
        // non-regular file is ignored for a fifo or terminal and unspecified
        // elsewhere, so this must run before `write_all` rather than rely on the
        // open being harmless.
        match file.metadata() {
            Ok(metadata) if metadata.file_type().is_file() => {}
            Ok(_) => {
                return Err(irregular_refusal(path).unwrap_or_else(|| {
                    // The descriptor says it is not a regular file and the path no
                    // longer agrees -- it was replaced in between. Refuse anyway,
                    // naming what was opened rather than what is there now.
                    FileSystemError::IrregularTarget {
                        path: path.to_path_buf(),
                        kind: "non-regular file",
                    }
                }));
            }
            // Fails **closed**, and matched explicitly rather than folded into the
            // arm above: a descriptor selfie cannot classify is one it must not
            // write to, for the reason the non-unix symlink branch gives -- being
            // unable to tell is not permission to proceed.
            //
            // The error is propagated rather than turned into a refusal. Reporting
            // `IrregularTarget` here would name a file type nothing observed; this
            // is a stat that failed, and it says so. `fstat` on a live descriptor
            // is close to infallible, so this is a matter of not encoding the
            // wrong precedent rather than a path anyone is expected to hit.
            Err(e) => return Err(io_err(e)),
        }

        file.write_all(data).map_err(io_err)
    }

    fn symlink_refusal(&self, path: &TargetPath) -> Option<FileSystemError> {
        symlink_refusal(path.path())
    }

    fn irregular_target_refusal(&self, path: &TargetPath) -> Option<FileSystemError> {
        irregular_refusal(path.path())
    }

    fn is_owner_only(&self, path: &TargetPath) -> Result<bool, FileSystemError> {
        let metadata =
            fs::metadata(path.path()).map_err(|e| FileSystemError::IoError(Arc::new(e)))?;

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
            other => panic!("Expected IoError with NotFound, got {other:?}"),
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
            other => panic!("Expected IoError with NotFound, got {other:?}"),
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
            other => panic!("Expected IoError with NotFound, got {other:?}"),
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
            other => panic!("Expected IoError variant, got {other:?}"),
        }
    }
}

/// A [`TargetPath`] for a test fixture: the writers under test take nothing else.
#[cfg(test)]
fn tp(path: &Path) -> TargetPath {
    super::target::expand_target_path(&RealFileSystem, path.to_str().unwrap())
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
            .write_file_private(&tp(&target), b"secret")
            .unwrap();

        assert_eq!(fs::read(&target).unwrap(), b"secret");
    }

    #[test]
    fn replaces_the_content_of_an_existing_file() {
        let dir = tempdir().unwrap();
        let target = dir.path().join("creds");
        fs::write(&target, b"a much longer previous value").unwrap();

        RealFileSystem
            .write_file_private(&tp(&target), b"new")
            .unwrap();

        // Not merely overwritten in place: nothing of the old value survives.
        assert_eq!(fs::read(&target).unwrap(), b"new");
    }

    #[test]
    fn creates_missing_parent_directories() {
        let dir = tempdir().unwrap();
        let target = dir.path().join("nested").join("deeper").join("creds");

        RealFileSystem
            .write_file_private(&tp(&target), b"secret")
            .unwrap();

        assert_eq!(fs::read(&target).unwrap(), b"secret");
    }

    #[test]
    fn writes_empty_data() {
        let dir = tempdir().unwrap();
        let target = dir.path().join("creds");

        RealFileSystem
            .write_file_private(&tp(&target), b"")
            .unwrap();

        assert_eq!(fs::read(&target).unwrap(), b"");
    }

    #[test]
    fn leaves_no_temporary_file_behind_on_success() {
        let dir = tempdir().unwrap();
        let target = dir.path().join("creds");

        RealFileSystem
            .write_file_private(&tp(&target), b"secret")
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

        let err = RealFileSystem.write_file_private(&tp(&target), b"secret");

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
                .write_file_private(&tp(&target), b"secret")
                .unwrap();

            assert_owner_only(&target);
        }

        #[test]
        fn tightens_mode_of_an_existing_world_readable_file() {
            let dir = tempdir().unwrap();
            let target = dir.path().join("creds");
            fs::write(&target, b"old").unwrap();
            fs::set_permissions(&target, fs::Permissions::from_mode(0o644)).unwrap();

            RealFileSystem
                .write_file_private(&tp(&target), b"new")
                .unwrap();

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
                .write_file_private(&tp(&target), b"secret")
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
                .write_file_private(&tp(&target), b"secret")
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

            RealFileSystem
                .write_file_private(&tp(&target), b"new")
                .unwrap();

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

            let result = RealFileSystem.write_file_private(&tp(&target), b"secret");

            assert!(result.is_err());
            assert_eq!(fs::read(&target).unwrap(), b"old", "target was modified");

            // Restore write access so the temporary directory can be cleaned up.
            fs::set_permissions(&parent, fs::Permissions::from_mode(0o700)).unwrap();
        }

        // Pins the temporary file to the target's own directory.
        //
        // This is only observable where the target and `$TMPDIR` are on different
        // filesystems: a temporary file created anywhere else would then fail to
        // rename into place with `EXDEV`. On CI runners `/dev/shm` is a tmpfs while
        // `$TMPDIR` is not. Where that does not hold the test cannot discriminate,
        // so it skips rather than asserting something vacuous — each skip path says
        // so, since an early return is otherwise indistinguishable from a pass.
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
                .write_file_private(&tp(&target), b"secret")
                .unwrap();

            assert_eq!(fs::read(&target).unwrap(), b"secret");
            assert_owner_only(&target);
        }
    }
}

/// Tests for [`FileSystem::write_file_no_follow`].
///
/// The plain-file tests below hold equally for [`FileSystem::write_file`], so they
/// guard against gross breakage rather than against the defect this method exists
/// to fix.
///
/// In `unix`, the three refusal tests are the ones that fail against `write_file`;
/// the two mode tests are the ones that fail against
/// [`FileSystem::write_file_private`], which would tighten a dotfile nobody asked to
/// have tightened. Neither group covers both neighbors, which is why both are here.
/// `a_symlinked_parent_directory_is_still_followed` passes against all three: it
/// pins a documented limitation so it cannot later be overstated, and is not a
/// distinguishing test. Stated per-test rather than as a blanket claim, because the
/// blanket version was not true of all six.
#[cfg(test)]
mod no_follow_write_tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn writes_content_to_a_new_file() {
        let dir = tempdir().unwrap();
        let target = dir.path().join("config");

        RealFileSystem
            .write_file_no_follow(&tp(&target), b"content")
            .unwrap();

        assert_eq!(fs::read(&target).unwrap(), b"content");
    }

    #[test]
    fn truncates_an_existing_file_rather_than_overwriting_in_place() {
        let dir = tempdir().unwrap();
        let target = dir.path().join("config");
        fs::write(&target, b"a much longer previous value").unwrap();

        RealFileSystem
            .write_file_no_follow(&tp(&target), b"new")
            .unwrap();

        // Without O_TRUNC the tail of the old value would survive past the new one.
        assert_eq!(fs::read(&target).unwrap(), b"new");
    }

    #[test]
    fn creates_missing_parent_directories() {
        let dir = tempdir().unwrap();
        let target = dir.path().join("nested").join("deeper").join("config");

        RealFileSystem
            .write_file_no_follow(&tp(&target), b"content")
            .unwrap();

        assert_eq!(fs::read(&target).unwrap(), b"content");
    }

    #[test]
    fn writes_empty_data() {
        let dir = tempdir().unwrap();
        let target = dir.path().join("config");

        RealFileSystem
            .write_file_no_follow(&tp(&target), b"")
            .unwrap();

        assert_eq!(fs::read(&target).unwrap(), b"");
    }

    #[test]
    fn a_directory_at_the_target_is_an_ordinary_error() {
        let dir = tempdir().unwrap();
        let target = dir.path().join("config");
        fs::create_dir(&target).unwrap();

        let err = RealFileSystem
            .write_file_no_follow(&tp(&target), b"content")
            .unwrap_err();

        // Not every failure is a refusal. Misclassifying this one would report
        // "target is a symlink" for a path that is nothing of the kind.
        assert!(
            matches!(err, FileSystemError::IoError(_)),
            "expected an IO error, got {err:?}"
        );
    }

    #[cfg(unix)]
    mod unix {
        use super::*;
        use std::os::unix::fs::PermissionsExt as _;

        fn mode_of(path: &Path) -> u32 {
            fs::metadata(path).unwrap().permissions().mode() & 0o777
        }

        #[test]
        fn refuses_a_symlink_and_leaves_its_destination_alone() {
            let dir = tempdir().unwrap();
            let destination = dir.path().join("destination");
            fs::write(&destination, b"untouched").unwrap();
            let target = dir.path().join("config");
            std::os::unix::fs::symlink(&destination, &target).unwrap();

            let err = RealFileSystem
                .write_file_no_follow(&tp(&target), b"content")
                .unwrap_err();

            assert!(matches!(err, FileSystemError::SymlinkedTarget { .. }));
            assert_eq!(fs::read(&destination).unwrap(), b"untouched");
            assert!(
                fs::symlink_metadata(&target)
                    .unwrap()
                    .file_type()
                    .is_symlink(),
                "the link itself must be left in place"
            );
        }

        #[test]
        fn refuses_a_dangling_symlink_without_creating_its_destination() {
            let dir = tempdir().unwrap();
            let never_created = dir.path().join("never-created");
            let target = dir.path().join("config");
            std::os::unix::fs::symlink(&never_created, &target).unwrap();

            let err = RealFileSystem
                .write_file_no_follow(&tp(&target), b"content")
                .unwrap_err();

            assert!(matches!(err, FileSystemError::SymlinkedTarget { .. }));
            // `fs::write` would have created the file the link points at, which is
            // the case a caller cannot see by looking at the target afterwards.
            assert!(!never_created.exists());
        }

        #[test]
        fn the_refusal_names_the_destination() {
            let dir = tempdir().unwrap();
            let destination = dir.path().join("destination");
            let target = dir.path().join("config");
            std::os::unix::fs::symlink(&destination, &target).unwrap();

            match RealFileSystem
                .write_file_no_follow(&tp(&target), b"content")
                .unwrap_err()
            {
                FileSystemError::SymlinkedTarget { path, points_to } => {
                    // Without the destination a user is told their deploy was
                    // refused but not where the link would have sent it, which is
                    // the one fact they need in order to act.
                    assert_eq!(path, target);
                    assert_eq!(points_to.as_deref(), Some(destination.as_path()));
                }
                other => panic!("expected SymlinkedTarget, got {other:?}"),
            }
        }

        #[test]
        fn leaves_an_existing_files_mode_alone() {
            let dir = tempdir().unwrap();
            let target = dir.path().join("script");
            fs::write(&target, b"old").unwrap();
            fs::set_permissions(&target, fs::Permissions::from_mode(0o755)).unwrap();

            RealFileSystem
                .write_file_no_follow(&tp(&target), b"new")
                .unwrap();

            // An executable dotfile stays executable. This is where the method
            // parts company with `write_file_private`, which would replace the
            // file and discard the mode.
            assert_eq!(mode_of(&target), 0o755);
        }

        #[test]
        fn creates_a_new_file_at_the_umask_default_not_owner_only() {
            let dir = tempdir().unwrap();
            // The control establishes what an ordinary write produces here. Without
            // it, an implementation that made every target owner-only would pass
            // under a restrictive umask, which is the vacuous pass this guards
            // against -- an ordinary dotfile is not a credential and this method
            // must not quietly tighten one.
            let control = dir.path().join("control");
            RealFileSystem.write_file(&control, b"x").unwrap();
            if mode_of(&control) & 0o077 == 0 {
                let message = "the ambient umask makes ordinary writes owner-only, \
                               so this cannot tell the two apart";
                assert!(
                    std::env::var_os("CI").is_none(),
                    "creates_a_new_file_at_the_umask_default_not_owner_only: {message}"
                );
                eprintln!("SKIP creates_a_new_file_at_the_umask_default_not_owner_only: {message}");
                return;
            }

            let target = dir.path().join("config");
            RealFileSystem
                .write_file_no_follow(&tp(&target), b"content")
                .unwrap();

            assert_eq!(mode_of(&target), mode_of(&control));
        }

        #[test]
        fn a_symlinked_parent_directory_is_still_followed() {
            let dir = tempdir().unwrap();
            let real = dir.path().join("real");
            fs::create_dir(&real).unwrap();
            let linked = dir.path().join("linked");
            std::os::unix::fs::symlink(&real, &linked).unwrap();

            RealFileSystem
                .write_file_no_follow(&tp(&linked.join("config")), b"content")
                .unwrap();

            // Asserted rather than only documented, so the limitation cannot be
            // quietly overstated later: `O_NOFOLLOW` covers the final component
            // only.
            assert!(real.join("config").exists());
        }
    }
}

/// Targets that are neither absent nor a regular file.
///
/// Unix-only: fifos, sockets and device nodes are Unix file types, and
/// `irregular_target_refusal` answers `None` everywhere else by construction.
///
/// A fifo is the reason this exists. Opening one blocks until the other end is
/// opened — for reading *and* for writing — so a test that reaches the unguarded
/// path does not fail, it **hangs**, and a hang is scored as neither pass nor
/// fail. Every test here that could reach an open runs the call on a blocking
/// thread and times out the handle, so the failure mode is a failed assertion
/// rather than a wedged run. `tokio::time::timeout` around the call itself would
/// not do: these are synchronous, so the future polls the blocking call inline
/// and the timer never gets to run.
#[cfg(all(test, unix))]
mod irregular_targets {
    use super::*;
    use std::io::Read as _;
    use std::time::Duration;
    use tempfile::tempdir;

    /// A `TargetPath` for `path`, unresolved as the type requires.
    fn tp(path: &Path) -> TargetPath {
        crate::fs::target::expand_target_path(&RealFileSystem, path.to_str().unwrap())
    }

    /// A fifo, and a scoped temp dir to keep it in.
    fn fifo_in(dir: &Path) -> PathBuf {
        let path = dir.join("target");
        nix::unistd::mkfifo(&path, nix::sys::stat::Mode::S_IRWXU).unwrap();
        path
    }

    /// Run a blocking filesystem call with a deadline.
    ///
    /// Returns `None` if it did not finish, which is how a regression that
    /// reintroduces the hang reports itself as a test failure. The blocked thread
    /// is left behind deliberately: it cannot be cancelled, and the process is
    /// about to end.
    fn with_deadline<T, F>(f: F) -> Option<T>
    where
        F: FnOnce() -> T + Send + 'static,
        T: Send + 'static,
    {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(f());
        });
        rx.recv_timeout(Duration::from_secs(5)).ok()
    }

    #[test]
    fn a_fifo_is_named_as_one() {
        let dir = tempdir().unwrap();
        let fifo = fifo_in(dir.path());

        match irregular_refusal(&fifo) {
            Some(FileSystemError::IrregularTarget { kind, path }) => {
                assert_eq!(kind, "named pipe (fifo)");
                assert_eq!(path, fifo);
            }
            other => panic!("expected an irregular-target refusal, got {other:?}"),
        }
    }

    #[test]
    fn a_socket_is_named_as_one() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("target");
        let _listener = std::os::unix::net::UnixListener::bind(&path).unwrap();

        match irregular_refusal(&path) {
            Some(FileSystemError::IrregularTarget { kind, .. }) => assert_eq!(kind, "socket"),
            other => panic!("expected an irregular-target refusal, got {other:?}"),
        }
    }

    #[test]
    fn a_character_device_is_named_as_one() {
        match irregular_refusal(Path::new("/dev/null")) {
            Some(FileSystemError::IrregularTarget { kind, .. }) => {
                assert_eq!(kind, "character device");
            }
            other => panic!("expected an irregular-target refusal, got {other:?}"),
        }
    }

    /// The controls: an ordinary target, and one that is not there yet.
    #[test]
    fn a_regular_file_and_an_absent_path_are_not_refused() {
        let dir = tempdir().unwrap();
        let file = dir.path().join("regular");
        fs::write(&file, b"content").unwrap();

        assert!(irregular_refusal(&file).is_none());
        assert!(irregular_refusal(&dir.path().join("absent")).is_none());
    }

    /// A symlink to a fifo is refused, because the read that follows would follow
    /// the link and block on the fifo.
    ///
    /// The whole reason this question is asked with a *following* stat. With
    /// `symlink_metadata` — the syscall `symlink_refusal` uses — this answers
    /// `None`, the target read follows the link, and apply hangs exactly as it
    /// did before the guard existed. The guard would have looked present and
    /// done nothing.
    #[test]
    fn a_symlink_to_a_fifo_is_refused() {
        let dir = tempdir().unwrap();
        let fifo = fifo_in(dir.path());
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&fifo, &link).unwrap();

        match irregular_refusal(&link) {
            Some(FileSystemError::IrregularTarget { kind, .. }) => {
                assert_eq!(kind, "named pipe (fifo)");
            }
            other => panic!("a link to a fifo must be refused, got {other:?}"),
        }
    }

    /// A symlink to a regular file is not this check's business.
    ///
    /// It is `symlink_refusal`'s, and answering here too would report one problem
    /// in two voices.
    #[test]
    fn a_symlink_to_a_regular_file_is_left_to_the_symlink_check() {
        let dir = tempdir().unwrap();
        let file = dir.path().join("regular");
        fs::write(&file, b"content").unwrap();
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&file, &link).unwrap();

        assert!(irregular_refusal(&link).is_none());
        assert!(symlink_refusal(&link).is_some(), "control: it is a symlink");
    }

    /// A dangling link has nothing to open, so it is not irregular.
    #[test]
    fn a_dangling_symlink_is_not_irregular() {
        let dir = tempdir().unwrap();
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(dir.path().join("nowhere"), &link).unwrap();

        assert!(irregular_refusal(&link).is_none());
        assert!(symlink_refusal(&link).is_some(), "control: it is a symlink");
    }

    /// The writer refuses a fifo with no reader, and does not block doing it.
    ///
    /// Without `O_NONBLOCK` the `open` itself blocks here and never reaches the
    /// descriptor check, so this is the test that observes the flag.
    #[test]
    fn the_writer_refuses_a_readerless_fifo_without_blocking() {
        let dir = tempdir().unwrap();
        let fifo = fifo_in(dir.path());

        let result =
            with_deadline(move || RealFileSystem.write_file_no_follow(&tp(&fifo), b"data"))
                .expect("writing a readerless fifo must not block");

        match result {
            Err(FileSystemError::IrregularTarget { kind, .. }) => {
                assert_eq!(kind, "named pipe (fifo)");
            }
            other => panic!("expected an irregular-target refusal, got {other:?}"),
        }
    }

    /// With a reader attached the open succeeds, and the descriptor check is what
    /// refuses.
    ///
    /// The route is pinned by construction rather than asserted: POSIX guarantees
    /// `open(O_WRONLY | O_NONBLOCK)` cannot return `ENXIO` while a reader holds
    /// the fifo open, so the failed-open path is unreachable here and only the
    /// `fstat` after the open can produce the refusal. The reader is opened by
    /// this thread with `O_NONBLOCK`, which POSIX guarantees returns immediately
    /// with no writer — a reader thread could not signal readiness, because its
    /// own open would block until the writer arrived, and the test would silently
    /// fall back to the readerless route it is written to exclude.
    ///
    /// That the `fstat` fired, rather than the write proceeding, is observable:
    /// the reader receives nothing.
    #[test]
    fn the_writer_refuses_a_fifo_that_has_a_reader() {
        use std::os::unix::fs::OpenOptionsExt as _;

        let dir = tempdir().unwrap();
        let fifo = fifo_in(dir.path());

        let mut reader = fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(&fifo)
            .expect("a reader may open a fifo with O_NONBLOCK before any writer");

        let target = fifo.clone();
        let result =
            with_deadline(move || RealFileSystem.write_file_no_follow(&tp(&target), b"data"))
                .expect("the open cannot block while a reader is attached");

        match result {
            Err(FileSystemError::IrregularTarget { kind, .. }) => {
                assert_eq!(kind, "named pipe (fifo)");
            }
            other => panic!("expected an irregular-target refusal, got {other:?}"),
        }

        let mut buf = [0u8; 16];
        let read = reader.read(&mut buf);
        assert!(
            matches!(&read, Err(e) if e.kind() == std::io::ErrorKind::WouldBlock)
                || matches!(&read, Ok(0)),
            "nothing may reach the reader: the refusal must precede the write, got {read:?}"
        );
    }

    /// The writer refuses a character device rather than writing to it.
    #[test]
    fn the_writer_refuses_a_character_device() {
        let err = RealFileSystem
            .write_file_no_follow(&tp(Path::new("/dev/null")), b"data")
            .unwrap_err();

        match err {
            FileSystemError::IrregularTarget { kind, .. } => {
                assert_eq!(kind, "character device");
            }
            other => panic!("expected an irregular-target refusal, got {other:?}"),
        }
    }
}
