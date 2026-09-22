// Real file system adapter implementation

use std::{
    fs,
    os::unix::fs::PermissionsExt as _,
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
// Every read of a path that selfie does not control asks this first: opening a fifo to
// read blocks, and nothing else on a read path checks.
fn irregular_kind(path: &Path) -> Option<&'static str> {
    use std::os::unix::fs::FileTypeExt as _;

    let metadata = fs::metadata(path).ok()?;
    let file_type = metadata.file_type();

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

    // A **directory** is deliberately not one of these: opening one never blocks,
    // and writing to one fails `EISDIR` without touching anything.
    //
    // `a_directory_at_the_target_is_an_ordinary_error` pins that it stays an
    // `IoError`. A directory at a deploy target is therefore refused by the read
    // that precedes the deploy decision, as any unreadable target is.
    None
}

/// The directory `path` lives in, as one the filesystem will actually open.
///
/// `Path::parent` gives `Some("")` for a bare name like `config.toml`, and
/// `File::open("")` is `ENOENT`. `create_dir_all` and `tempfile_in` both cope
/// with the empty path; the directory fsync does not.
fn parent_dir(path: &Path) -> &Path {
    path.parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."))
}

/// [`FileSystemError::IrregularTarget`] for anything `irregular_kind` names.
///
/// Free function taking a `&Path`, the same shape `symlink_refusal` has, so the
/// writer can ask it of a plain path.
fn irregular_refusal(path: &Path) -> Option<FileSystemError> {
    irregular_kind(path).map(|kind| FileSystemError::IrregularTarget {
        path: path.to_path_buf(),
        kind,
    })
}

/// What the writer does with whatever is already at the target, and how far its
/// durability reaches.
#[derive(Clone, Copy)]
enum Replacement {
    /// Replace a symlink that does not resolve to an irregular target; create
    /// owner-only. The parent directory is not fsynced.
    // The deploy state is written this way and must stay less durable than the
    // targets it records: a record outliving the write it describes turns a
    // lost deploy into a conflict blamed on the user. `save_deploy_state` says
    // so; losing a secret target's rename costs the deploy, not the data.
    OwnerOnly,
    /// Refuse a symlink; keep an existing regular file's mode, else create at
    /// `0o666 & !umask`. The parent directory is fsynced, best effort.
    KeepingMode,
}

/// Write `data` to a temporary file beside `path` and rename it into place.
///
/// # Errors
///
/// A refusal for an irregular target, or for a symlink under `KeepingMode`,
/// present when checked; or an IO error naming `path` for anything else.
fn write_by_rename(path: &Path, data: &[u8], how: Replacement) -> Result<(), FileSystemError> {
    use std::io::Write as _;

    // The temporary file's name is random and the rename carries no path at all,
    // so failures would otherwise name a file the operator never chose -- or
    // nothing. Re-tag them with the target. `kind()` is preserved; only the raw
    // OS error number is lost, which nothing here consumes.
    let target_err = |e: std::io::Error| {
        FileSystemError::IoError(Arc::new(std::io::Error::new(
            e.kind(),
            format!("{}: {e}", path.display()),
        )))
    };

    // A target whose directory does not exist yet is created rather than refused.
    // See `parent_dir` for why the bare-relative-name case needs normalizing.
    let parent = parent_dir(path);
    // A directory that could not be created is the parent's failure, so the
    // message names the parent; naming the target would blame a file that was
    // never touched.
    fs::create_dir_all(parent).map_err(|e| {
        FileSystemError::IoError(Arc::new(std::io::Error::new(
            e.kind(),
            format!("{}: {e}", parent.display()),
        )))
    })?;

    // Refusals are decided here, before a temporary file exists, so a refused
    // write leaves nothing beside the target. They are about the path as it is
    // now: a link or fifo planted after this point is replaced by the rename
    // below, never followed or opened, and the write succeeds.
    //
    // Symlink first: a link to a fifo would otherwise be reported as a fifo,
    // naming the wrong problem and suggesting the wrong fix. The irregular
    // check follows links, so a private write refuses a link to a fifo too:
    // renaming over a fifo, or over a link to one, would silently destroy it.
    let keep_mode = matches!(how, Replacement::KeepingMode);
    if keep_mode && let Some(refusal) = symlink_refusal(path) {
        return Err(refusal);
    }
    if let Some(refusal) = irregular_refusal(path) {
        return Err(refusal);
    }

    // The permission bits only. setuid, setgid and sticky are not something a
    // dotfile carries, and an unprivileged in-place write clears the first two
    // anyway.
    //
    // A non-following stat, like the symlink check above: the mode has to come
    // from what is at the name, never from what a link there points at. With a
    // following stat, a link planted after that check hands its destination's
    // mode -- chosen by whoever planted it -- to the file the rename then puts
    // in the link's place.
    let existing_mode = if keep_mode {
        fs::symlink_metadata(path)
            .ok()
            .filter(|metadata| metadata.is_file())
            .map(|metadata| metadata.permissions().mode() & 0o777)
    } else {
        None
    };

    // The temporary file must live in the target's own directory: elsewhere it may
    // be on another filesystem, making the rename non-atomic, or world-readable.
    let mut builder = tempfile::Builder::new();
    builder.prefix(".selfie-");
    match (how, existing_mode) {
        // Applied by the creating syscall, so the content is never briefly
        // world-readable. The umask may restrict this further but can never
        // loosen it.
        (Replacement::OwnerOnly, _) => {
            builder.permissions(fs::Permissions::from_mode(0o600));
        }
        // What an ordinary `open(O_CREAT)` gives: the umask is applied by the
        // kernel, so this is exactly what `fs::write` would have created.
        (Replacement::KeepingMode, None) => {
            builder.permissions(fs::Permissions::from_mode(0o666));
        }
        // Created at tempfile's default of 0o600 and widened by the `fchmod`
        // below. Passing the mode here instead would put it through the umask,
        // stripping group and other bits the user had set.
        (Replacement::KeepingMode, Some(_)) => {}
    }

    // Randomly named and unlinked on drop, so it cannot collide with a concurrent
    // write and no *error* path leaves it behind. A crash is another matter: being
    // killed or losing power between here and the rename leaves a `.selfie-*` file
    // beside the target holding the complete content. For a secret it is mode
    // 0600, so this is debris rather than disclosure, but nothing sweeps it up.
    let mut tmp = builder.tempfile_in(parent).map_err(target_err)?;
    tmp.write_all(data).map_err(target_err)?;
    if let Some(mode) = existing_mode {
        tmp.as_file()
            .set_permissions(fs::Permissions::from_mode(mode))
            .map_err(target_err)?;
    }
    // Flush before the rename: otherwise a crash can leave the target name
    // pointing at a zero-length file.
    //
    // For an ordinary target this is also what lets a caller record the write as
    // having happened. `record_and_save` records the deployment as soon as the
    // writer returns; if the write were lost to a crash while that
    // record survived, the state would claim content the target does not have,
    // and the entry would become a sticky conflict blamed on the user
    // (selfie-aub). Ordering is the fix, so it belongs here, before the record.
    tmp.as_file().sync_all().map_err(target_err)?;

    // Replaces the target by rename, so readers see either the old file or the
    // complete new one, and a symlink at the final component is replaced rather
    // than followed.
    tmp.persist(path).map_err(|e| target_err(e.error))?;

    // The data is durable; the directory entry naming it is not. A freshly
    // created file can survive its own fsync and still vanish. `OwnerOnly`
    // skips this on purpose; see the variant.
    //
    // Best-effort, deliberately: opening a directory needs read permission, so
    // a `0o300` directory refuses this open with `EACCES`, and failing here
    // would break a deploy that works today.
    // `a_write_only_parent_directory_still_succeeds` holds it. Only the
    // immediate parent; on Apple targets std's `sync_all` is `fcntl(F_FULLFSYNC)`,
    // and its failure is swallowed here like any other. Do not claim more.
    if keep_mode && let Ok(dir) = fs::File::open(parent) {
        let _ = dir.sync_all();
    }

    Ok(())
}

impl FileSystem for RealFileSystem {
    fn read_file(&self, path: &Path) -> Result<String, FileSystemError> {
        fs::read_to_string(path).map_err(|e| FileSystemError::IoError(Arc::new(e)))
    }

    fn read_file_bytes(&self, path: &Path) -> Result<Vec<u8>, FileSystemError> {
        fs::read(path).map_err(|e| FileSystemError::IoError(Arc::new(e)))
    }

    fn write_file_private(&self, path: &TargetPath, data: &[u8]) -> Result<(), FileSystemError> {
        write_by_rename(path.path(), data, Replacement::OwnerOnly)
    }

    fn write_file_no_follow(&self, path: &TargetPath, data: &[u8]) -> Result<(), FileSystemError> {
        write_by_rename(path.path(), data, Replacement::KeepingMode)
    }

    fn symlink_refusal(&self, path: &TargetPath) -> Option<FileSystemError> {
        symlink_refusal(path.path())
    }

    // Uses a **following** stat, unlike `symlink_refusal` above. `read_file`
    // resolves the path, so a non-following stat would see a symlink, answer
    // `None`, and let the fifo behind it block the read. The writers never open
    // the target, but they ask the same question so that a symlink to a fifo is
    // refused exactly as a fifo is, which is the documented rule. A dangling link
    // fails the stat and is `None`, which is right: nothing to open, and
    // `symlink_refusal` reports it. The two guards answer different questions and
    // need different syscalls; mirroring this one on the other reintroduces the
    // hang on the read path.
    fn irregular_target_refusal(&self, path: &TargetPath) -> Option<FileSystemError> {
        irregular_refusal(path.path())
    }

    fn is_directory(&self, path: &TargetPath) -> Result<bool, FileSystemError> {
        // A following stat, like `irregular_target_refusal`'s, so it answers for what
        // a write would land on rather than for a link in the way.
        match fs::metadata(path.path()) {
            Ok(metadata) => Ok(metadata.is_dir()),
            // Nothing there is not an error: an absent target is the ordinary case
            // for a first deploy, and a write creates it.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(FileSystemError::IoError(Arc::new(e))),
        }
    }

    fn is_owner_only(&self, path: &TargetPath) -> Result<bool, FileSystemError> {
        let metadata =
            fs::metadata(path.path()).map_err(|e| FileSystemError::IoError(Arc::new(e)))?;

        // Any group or other bit set means someone else can reach it.
        Ok(metadata.permissions().mode() & 0o077 == 0)
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

    // Tested on the helper, not through a write: reaching this case through
    // `write_file_no_follow` needs a relative path, and that needs
    // `set_current_dir`, which is process-global and would race the suite.
    // selfie-aub
    #[test]
    fn a_bare_relative_name_gets_the_current_directory_as_its_parent() {
        assert_eq!(parent_dir(Path::new("config.toml")), Path::new("."));
    }

    // The control: an ordinary path keeps its real parent, so the helper is not
    // simply answering "." for everything.
    #[test]
    fn a_nested_path_keeps_its_own_parent() {
        assert_eq!(
            parent_dir(Path::new("/pkgs/myapp/config.toml")),
            Path::new("/pkgs/myapp")
        );
        assert_eq!(parent_dir(Path::new("/config.toml")), Path::new("/"));
    }

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

// Names of everything in `dir`, for asserting that no temporary file survived.
#[cfg(test)]
fn entries(dir: &Path) -> Vec<String> {
    let mut names: Vec<_> = fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

// The permission bits of what `path` resolves to.
#[cfg(test)]
fn mode_of(path: &Path) -> u32 {
    fs::metadata(path).unwrap().permissions().mode() & 0o777
}

// Tests for [`FileSystem::write_file_private`].

// Grouped by what each test actually proves.

// The six tests directly below all still pass against a naive `create_dir_all` +
// `fs::write`, so they guard against gross breakage rather than against the defects
// this method exists to fix. Named as that pair rather than as a method: the
// comparison is with the implementation someone might reach for, not with an API
// the port offers.

// Everything in `unix` is load-bearing: against `create_dir_all` + `fs::write`,
// every one of them that runs here fails, and none of the six above does. Two
// are not part of that claim: the `/dev/shm` test is Linux-only, and
// `refuses_a_fifo_rather_than_renaming_over_it` would hang on a following
// open rather than fail.
#[cfg(test)]
mod private_write_tests {
    use super::*;
    use tempfile::tempdir;

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

    mod unix {
        use super::*;
        use std::os::unix::fs::MetadataExt as _;

        // The security property, asserted the only way that cannot flake: the
        // umask may restrict the mode further than the 0o600 we request, but it
        // can never loosen it, so group and other bits must be clear.
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

        // A fifo at a secret target is refused, not renamed over: the rename
        // would destroy whatever was reading it, and the secret path checks
        // for exactly this before it resolves the content.
        #[test]
        fn refuses_a_fifo_rather_than_renaming_over_it() {
            use std::os::unix::fs::FileTypeExt as _;

            let dir = tempdir().unwrap();
            let target = dir.path().join("creds");
            nix::unistd::mkfifo(&target, nix::sys::stat::Mode::S_IRWXU).unwrap();

            let err = RealFileSystem
                .write_file_private(&tp(&target), b"secret")
                .unwrap_err();

            match err {
                FileSystemError::IrregularTarget { kind, .. } => {
                    assert_eq!(kind, "named pipe (fifo)");
                }
                other => panic!("expected an irregular-target refusal, got {other:?}"),
            }
            assert!(
                fs::symlink_metadata(&target).unwrap().file_type().is_fifo(),
                "the fifo must be left in place"
            );
            assert_eq!(entries(dir.path()), ["creds"]);
        }

        // The failure names the file it was writing, as the ordinary writer's
        // does, and the original survives it.
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
            // directory, so a plain `fs::write` succeeds here; an atomic replace
            // still has to create a sibling, so it cannot.
            fs::write(&target, b"old").unwrap();
            fs::set_permissions(&parent, fs::Permissions::from_mode(0o500)).unwrap();

            let err = RealFileSystem
                .write_file_private(&tp(&target), b"secret")
                .unwrap_err();

            // Restore before asserting, so a failure still leaves a removable
            // temporary directory behind.
            fs::set_permissions(&parent, fs::Permissions::from_mode(0o700)).unwrap();

            assert!(matches!(err, FileSystemError::IoError(_)), "got {err:?}");
            let message = err.to_string();
            assert!(
                message.contains(target.to_str().unwrap()),
                "the failure does not name the target: {message}"
            );
            assert_eq!(fs::read(&target).unwrap(), b"old", "target was modified");
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

// Tests for [`FileSystem::write_file_no_follow`].

// The plain-file tests below hold equally for a naive `create_dir_all` + `fs::write`,
// so they guard against gross breakage rather than against the defects this method
// exists to fix.

// In `unix`, each group fails against a different neighbor, which is why all of
// them are here. The three symlink refusal tests fail against the naive pair,
// which follows the link. The two mode tests fail against
// [`FileSystem::write_file_private`], which would tighten a dotfile nobody asked to
// have tightened. `replaces_by_rename_rather_than_truncating_in_place`,
// `errors_when_the_parent_directory_is_not_writable` and
// `a_read_only_existing_file_is_replaced_and_keeps_its_mode` fail against a writer
// that truncates the existing file in place, which is the one a partial write
// damages. `a_symlinked_parent_directory_is_still_followed` passes against all of
// them: it pins a documented limitation so it cannot later be overstated.
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
    fn replaces_the_content_of_an_existing_file() {
        let dir = tempdir().unwrap();
        let target = dir.path().join("config");
        fs::write(&target, b"a much longer previous value").unwrap();

        RealFileSystem
            .write_file_no_follow(&tp(&target), b"new")
            .unwrap();

        // Nothing of the old value survives past the new one.
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
    fn leaves_no_temporary_file_behind_on_success() {
        let dir = tempdir().unwrap();
        let target = dir.path().join("config");

        RealFileSystem
            .write_file_no_follow(&tp(&target), b"content")
            .unwrap();

        assert_eq!(entries(dir.path()), ["config"]);
    }

    #[test]
    fn leaves_no_temporary_file_behind_when_the_rename_fails() {
        let dir = tempdir().unwrap();
        // A directory at the target cannot be replaced by a rename, so the write
        // gets as far as the temporary file and then fails.
        let target = dir.path().join("config");
        fs::create_dir(&target).unwrap();

        let err = RealFileSystem.write_file_no_follow(&tp(&target), b"content");

        assert!(err.is_err());
        assert_eq!(entries(dir.path()), ["config"]);
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

    mod unix {
        use super::*;

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

        // An executable dotfile stays executable, and so do the group and other
        // bits the user set; `write_file_private` would discard the mode.

        // The fixture has bits an ordinary umask strips: a writer passing the
        // mode through the creating `open` comes back `0o745` under `022` and
        // would pass at `0o755`. Where the umask strips nothing the two are
        // indistinguishable, so the test skips instead of asserting vacuously.

        // The control creates a file through `open` with the fixture mode, so
        // it shows whether this umask strips anything. `fs::write` asks for
        // `0o666`, which has no execute bit to strip, so a control made with it
        // could never come back `0o767` and the guard would never fire.
        #[test]
        fn leaves_an_existing_files_mode_alone() {
            use std::os::unix::fs::OpenOptionsExt as _;

            let dir = tempdir().unwrap();
            let control = dir.path().join("control");
            fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o767)
                .open(&control)
                .unwrap();
            if mode_of(&control) == 0o767 {
                let message = "the ambient umask strips no bits from 0o767, \
                               so this cannot tell the two apart";
                assert!(
                    std::env::var_os("CI").is_none(),
                    "leaves_an_existing_files_mode_alone: {message}"
                );
                eprintln!("SKIP leaves_an_existing_files_mode_alone: {message}");
                return;
            }

            let target = dir.path().join("script");
            fs::write(&target, b"old").unwrap();
            fs::set_permissions(&target, fs::Permissions::from_mode(0o767)).unwrap();

            RealFileSystem
                .write_file_no_follow(&tp(&target), b"new")
                .unwrap();

            assert_eq!(mode_of(&target), 0o767);
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
            // `fs::write` rather than a port method: the control has to be an
            // *ordinary* write, and every writer the port still offers has a
            // property under test here.
            fs::write(&control, b"x").unwrap();
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
            // quietly overstated later: the symlink check covers the final
            // component only.
            assert!(real.join("config").exists());
        }

        #[test]
        fn replaces_by_rename_rather_than_truncating_in_place() {
            use std::os::unix::fs::MetadataExt as _;

            let dir = tempdir().unwrap();
            let target = dir.path().join("config");
            fs::write(&target, b"old").unwrap();

            // Holding the original open keeps its inode allocated, so it cannot be
            // recycled for the replacement and make the comparison below flaky.
            let held = fs::File::open(&target).unwrap();
            let before = fs::metadata(&target).unwrap().ino();

            RealFileSystem
                .write_file_no_follow(&tp(&target), b"new")
                .unwrap();

            // A truncate-in-place writer that fails part way leaves the user's
            // file holding whatever it got to. Replacing the name is what makes a
            // partial write land in the temporary file instead.
            let after = fs::metadata(&target).unwrap().ino();
            assert_ne!(
                before, after,
                "target kept its inode, so it was modified in place rather than replaced"
            );
            drop(held);
        }

        // Rewriting an existing file needs write permission on the file, not on
        // its directory, so a truncate-in-place writer succeeds here. An atomic
        // replace has to create a sibling, so it cannot, and the original must
        // survive the failure untouched.
        //
        // The failure has to say which file it was writing: an apply over many
        // dotfiles otherwise reports "No space left on device" and nothing else.
        #[test]
        fn errors_when_the_parent_directory_is_not_writable() {
            if nix::unistd::Uid::effective().is_root() {
                eprintln!("SKIP errors_when_the_parent_directory_is_not_writable: running as root");
                return;
            }
            let dir = tempdir().unwrap();
            let parent = dir.path().join("locked");
            fs::create_dir(&parent).unwrap();
            let target = parent.join("config");
            fs::write(&target, b"old").unwrap();
            fs::set_permissions(&parent, fs::Permissions::from_mode(0o500)).unwrap();

            let err = RealFileSystem
                .write_file_no_follow(&tp(&target), b"new")
                .unwrap_err();

            // Restore before asserting, so a failure still leaves a removable
            // temporary directory behind.
            fs::set_permissions(&parent, fs::Permissions::from_mode(0o700)).unwrap();

            assert!(matches!(err, FileSystemError::IoError(_)), "got {err:?}");
            let message = err.to_string();
            assert!(
                message.contains(target.to_str().unwrap()),
                "the failure does not name the target: {message}"
            );
            assert_eq!(fs::read(&target).unwrap(), b"old", "target was modified");
        }

        // The directory, not the file, decides whether a name can be replaced, so
        // a read-only target is replaced and comes back read-only.
        #[test]
        fn a_read_only_existing_file_is_replaced_and_keeps_its_mode() {
            if nix::unistd::Uid::effective().is_root() {
                eprintln!(
                    "SKIP a_read_only_existing_file_is_replaced_and_keeps_its_mode: running as root"
                );
                return;
            }
            let dir = tempdir().unwrap();
            let target = dir.path().join("config");
            fs::write(&target, b"old").unwrap();
            fs::set_permissions(&target, fs::Permissions::from_mode(0o444)).unwrap();

            RealFileSystem
                .write_file_no_follow(&tp(&target), b"new")
                .unwrap();

            assert_eq!(fs::read(&target).unwrap(), b"new");
            assert_eq!(mode_of(&target), 0o444);
        }

        // The directory fsync must not turn a working write into a failure.
        //
        // Opening a directory needs read permission; writing a file in it needs
        // write and execute. So `0o300` is a directory selfie can write into and
        // cannot open -- only the directory fsync fails, with `EACCES`. Making
        // that fatal would break deploys into any write-only directory.
        //
        // The mode is the whole fixture: at `0o700` this passes against a fatal
        // implementation too, and proves nothing. selfie-aub
        #[test]
        fn a_write_only_parent_directory_still_succeeds() {
            if nix::unistd::Uid::effective().is_root() {
                eprintln!("SKIP a_write_only_parent_directory_still_succeeds: running as root");
                return;
            }
            let dir = tempdir().unwrap();
            let parent = dir.path().join("write-only");
            fs::create_dir(&parent).unwrap();
            let target = parent.join("config");
            fs::set_permissions(&parent, fs::Permissions::from_mode(0o300)).unwrap();

            let result = RealFileSystem.write_file_no_follow(&tp(&target), b"content");

            // Restore before asserting, so a failure still leaves a removable
            // temporary directory behind.
            fs::set_permissions(&parent, fs::Permissions::from_mode(0o700)).unwrap();

            result.expect("a write-only parent directory must not fail the write");
            assert_eq!(fs::read(&target).unwrap(), b"content");
        }
    }
}

// Targets that are neither absent nor a regular file.

// Unix-only: fifos, sockets and device nodes are Unix file types, and
// `irregular_target_refusal` answers `None` everywhere else by construction.

// A fifo is the reason this exists. Opening one blocks until the other end is
// opened — for reading *and* for writing — so a test that reaches the unguarded
// path does not fail, it **hangs**, and a hang is scored as neither pass nor
// fail. Every test here that could reach an open runs the call on a blocking
// thread and times out the handle, so the failure mode is a failed assertion
// rather than a wedged run. `tokio::time::timeout` around the call itself would
// not do: these are synchronous, so the future polls the blocking call inline
// and the timer never gets to run.
#[cfg(test)]
mod irregular_targets {
    use super::*;
    use std::io::Read as _;
    use std::time::Duration;
    use tempfile::tempdir;

    // A `TargetPath` for `path`, unresolved as the type requires.
    fn tp(path: &Path) -> TargetPath {
        crate::fs::target::expand_target_path(&RealFileSystem, path.to_str().unwrap())
    }

    // A fifo, and a scoped temp dir to keep it in.
    fn fifo_in(dir: &Path) -> PathBuf {
        let path = dir.join("target");
        nix::unistd::mkfifo(&path, nix::sys::stat::Mode::S_IRWXU).unwrap();
        path
    }

    // Run a blocking filesystem call with a deadline.
    //
    // Returns `None` if it did not finish, which is how a regression that
    // reintroduces the hang reports itself as a test failure. The blocked thread
    // is left behind deliberately: it cannot be cancelled, and the process is
    // about to end.
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

    // The controls: an ordinary target, and one that is not there yet.
    #[test]
    fn a_regular_file_and_an_absent_path_are_not_refused() {
        let dir = tempdir().unwrap();
        let file = dir.path().join("regular");
        fs::write(&file, b"content").unwrap();

        assert!(irregular_refusal(&file).is_none());
        assert!(irregular_refusal(&dir.path().join("absent")).is_none());
    }

    // A symlink to a fifo is refused, because the read that follows would follow
    // the link and block on the fifo.
    //
    // The whole reason this question is asked with a *following* stat. With
    // `symlink_metadata` — the syscall `symlink_refusal` uses — this answers
    // `None`, the target read follows the link, and apply hangs exactly as it
    // did before the guard existed. The guard would have looked present and
    // done nothing.
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

    // A symlink to a regular file is not this check's business.
    //
    // It is `symlink_refusal`'s, and answering here too would report one problem
    // in two voices.
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

    // A dangling link has nothing to open, so it is not irregular.
    #[test]
    fn a_dangling_symlink_is_not_irregular() {
        let dir = tempdir().unwrap();
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(dir.path().join("nowhere"), &link).unwrap();

        assert!(irregular_refusal(&link).is_none());
        assert!(symlink_refusal(&link).is_some(), "control: it is a symlink");
    }

    // The writer refuses a fifo with no reader, and does not block doing it: the
    // refusal comes from a stat, and a stat never blocks on a fifo.
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

    // A fifo with a reader attached is refused the same way, and the reader
    // receives nothing: the refusal precedes any open of the target. The reader
    // is opened by this thread -- a reader thread could not signal readiness,
    // because its own open would block until a writer arrived.
    #[test]
    fn the_writer_refuses_a_fifo_that_has_a_reader() {
        use std::os::unix::fs::OpenOptionsExt as _;

        let dir = tempdir().unwrap();
        let fifo = fifo_in(dir.path());

        let mut reader = fs::OpenOptions::new()
            .read(true)
            .custom_flags(nix::fcntl::OFlag::O_NONBLOCK.bits())
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

    // The writer refuses a character device rather than writing to it.
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
