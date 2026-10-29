//! A file system double that makes something appear at a target just before a
//! write reaches it, the race a write to an absent target has to survive, or that
//! changes a file just before a write reaches it, the race a rewrite has to
//! survive.

use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering::SeqCst};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use selfie::fs::{
    DirectoryState, FileIdentity, FileSystem, FileSystemError, IfPresent, OpenRefusal, Placed,
    RealFileSystem, Removal, RepositoryRead, TargetPath, TargetRead,
};

/// A high-entropy value standing in for another tool's own credential, written at
/// a target while selfie writes it. A leak test asserts it reaches no output.
pub const PLANTED: &str = "pl4nted-t0ken-9fK2qZx7Wm";

/// What appears at the target.
#[derive(Clone, Debug)]
pub enum Plant {
    /// A regular file with this content and mode.
    File(&'static str, u32),
    /// An empty directory.
    Directory,
    /// A symlink to a path beside the target, with the extension `nowhere`, that
    /// does not exist.
    DanglingLink,
}

/// What another writer does to a file selfie read earlier.
#[derive(Clone, Copy, Debug)]
pub enum Rewrite {
    /// The file is replaced by a rename with this content, as an editor or
    /// `git checkout` replaces one. It gets a new inode and a modification time
    /// ten seconds after the old one, whether or not the content differs.
    File(&'static str),
    /// The file is removed.
    Removed,
    /// The file is removed, and a named pipe is made in its place.
    Fifo,
}

/// A file's inode and modification time, which a [`Rewrite::File`] changes.
pub type Stamp = (u64, SystemTime);

/// [`RealFileSystem`], except that `plant` appears at `target` immediately before
/// the first write to it is delegated, as an app writing its first config would.
/// Optionally, what is at `target` is removed again at the first read after that.
#[derive(Clone, Debug)]
pub struct PlantsBeforeWrite {
    target: PathBuf,
    plant: Option<Plant>,
    vanish: bool,
    planted: Arc<AtomicBool>,
    planted_inode: Arc<Mutex<Option<u64>>>,
    vanished: Arc<AtomicBool>,
    rewrite: Option<Rewrite>,
    rewrite_claimed: Arc<AtomicBool>,
    rewritten: Arc<AtomicBool>,
    stamps: Arc<Mutex<Option<(Stamp, Stamp)>>>,
}

impl PlantsBeforeWrite {
    /// Plant `plant` at `target` before the first write to it.
    #[must_use]
    pub fn new(target: &Path, plant: Plant) -> Self {
        Self::with(target, Some(plant))
    }

    /// Plant nothing, and remove the file at `target` at the first read of it, so
    /// the checks ahead of that read find it and the read does not.
    #[must_use]
    pub fn vanishing_at_first_read(target: &Path) -> Self {
        Self::with(target, None).vanishing()
    }

    /// Plant nothing, and apply `rewrite` to `target` immediately before the
    /// first write to it, by either writer, is delegated.
    #[must_use]
    pub fn rewritten_before_write(target: &Path, rewrite: Rewrite) -> Self {
        let mut fs = Self::with(target, None);
        fs.rewrite = Some(rewrite);
        fs
    }

    /// Whether the rewrite was made and finished, so a test can confirm it was
    /// reached.
    #[must_use]
    pub fn rewritten(&self) -> bool {
        self.rewritten.load(SeqCst)
    }

    /// The file's inode and modification time just before and just after a
    /// [`Rewrite::File`], so a test can confirm both moved.
    ///
    /// # Panics
    ///
    /// If another thread panicked while recording them.
    #[must_use]
    pub fn stamps(&self) -> Option<(Stamp, Stamp)> {
        *self.stamps.lock().unwrap()
    }

    /// Also remove what was planted at the first read of `target` after it was
    /// planted.
    #[must_use]
    pub fn vanishing(mut self) -> Self {
        self.vanish = true;
        self
    }

    /// Whether the plant was made, so a test can confirm the write was reached.
    #[must_use]
    pub fn planted(&self) -> bool {
        self.planted.load(SeqCst)
    }

    /// Whether something at the target was removed by a read.
    #[must_use]
    pub fn vanished(&self) -> bool {
        self.vanished.load(SeqCst)
    }

    /// The inode of what was planted, so a test can tell it was not replaced.
    ///
    /// # Panics
    ///
    /// If another thread panicked while recording it.
    #[must_use]
    pub fn planted_inode(&self) -> Option<u64> {
        *self.planted_inode.lock().unwrap()
    }

    fn with(target: &Path, plant: Option<Plant>) -> Self {
        Self {
            target: target.to_path_buf(),
            plant,
            vanish: false,
            planted: Arc::default(),
            planted_inode: Arc::default(),
            vanished: Arc::default(),
            rewrite: None,
            rewrite_claimed: Arc::default(),
            rewritten: Arc::default(),
            stamps: Arc::default(),
        }
    }

    fn rewrite_if_first(&self, path: &Path) {
        let Some(rewrite) = self.rewrite else {
            return;
        };
        if path != self.target || self.rewrite_claimed.swap(true, SeqCst) {
            return;
        }
        match rewrite {
            Rewrite::File(content) => {
                let before = stamp(path);
                let staged = path.with_extension("rewrite");
                std::fs::write(&staged, content).unwrap();
                std::fs::File::options()
                    .write(true)
                    .open(&staged)
                    .unwrap()
                    .set_modified(before.1 + Duration::from_secs(10))
                    .unwrap();
                std::fs::rename(&staged, path).unwrap();
                *self.stamps.lock().unwrap() = Some((before, stamp(path)));
            }
            Rewrite::Removed => std::fs::remove_file(path).unwrap(),
            Rewrite::Fifo => {
                std::fs::remove_file(path).unwrap();
                nix::unistd::mkfifo(path, nix::sys::stat::Mode::S_IRWXU).unwrap();
            }
        }
        self.rewritten.store(true, SeqCst);
    }

    fn plant_if_first(&self, path: &Path) {
        let Some(plant) = &self.plant else {
            return;
        };
        if path != self.target || self.planted.swap(true, SeqCst) {
            return;
        }
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        match plant {
            Plant::File(content, mode) => {
                std::fs::write(path, content).unwrap();
                std::fs::set_permissions(path, std::fs::Permissions::from_mode(*mode)).unwrap();
            }
            Plant::Directory => std::fs::create_dir(path).unwrap(),
            Plant::DanglingLink => {
                std::os::unix::fs::symlink(path.with_extension("nowhere"), path).unwrap();
            }
        }
        *self.planted_inode.lock().unwrap() = std::fs::symlink_metadata(path)
            .ok()
            .map(|metadata| metadata.ino());
    }

    fn vanish_if_armed(&self, path: &Path) {
        let armed = self.plant.is_none() || self.planted();
        if self.vanish && armed && path == self.target && !self.vanished.swap(true, SeqCst) {
            std::fs::remove_file(path).unwrap();
        }
    }
}

fn stamp(path: &Path) -> Stamp {
    let metadata = std::fs::symlink_metadata(path).unwrap();
    (metadata.ino(), metadata.modified().unwrap())
}

impl FileSystem for PlantsBeforeWrite {
    fn read_file(&self, path: &Path) -> Result<String, FileSystemError> {
        RealFileSystem.read_file(path)
    }
    fn read_repository_file(&self, path: &Path) -> Result<String, RepositoryRead> {
        RealFileSystem.read_repository_file(path)
    }
    fn read_file_no_follow(&self, path: &TargetPath) -> Result<TargetRead, FileSystemError> {
        self.vanish_if_armed(path.path());
        RealFileSystem.read_file_no_follow(path)
    }
    fn write_file_private(
        &self,
        path: &TargetPath,
        data: &[u8],
        if_present: IfPresent,
    ) -> Result<Placed, FileSystemError> {
        self.rewrite_if_first(path.path());
        self.plant_if_first(path.path());
        RealFileSystem.write_file_private(path, data, if_present)
    }
    fn write_file_no_follow(
        &self,
        path: &TargetPath,
        data: &[u8],
        if_present: IfPresent,
    ) -> Result<Placed, FileSystemError> {
        self.rewrite_if_first(path.path());
        self.plant_if_first(path.path());
        RealFileSystem.write_file_no_follow(path, data, if_present)
    }
    fn symlink_refusal(&self, path: &TargetPath) -> Option<FileSystemError> {
        RealFileSystem.symlink_refusal(path)
    }
    fn irregular_target_refusal(&self, path: &TargetPath) -> Option<FileSystemError> {
        RealFileSystem.irregular_target_refusal(path)
    }
    fn open_for_read_refusal(&self, path: &TargetPath) -> Option<OpenRefusal> {
        RealFileSystem.open_for_read_refusal(path)
    }
    fn access_refusal(&self, directory: &Path) -> Option<FileSystemError> {
        RealFileSystem.access_refusal(directory)
    }
    fn is_owner_only(&self, path: &TargetPath) -> Result<bool, FileSystemError> {
        RealFileSystem.is_owner_only(path)
    }
    fn remove_file(&self, path: &Path) -> Result<(), FileSystemError> {
        RealFileSystem.remove_file(path)
    }
    fn remove_file_if_holding(
        &self,
        path: &TargetPath,
        expected: &[u8],
    ) -> Result<Removal, FileSystemError> {
        RealFileSystem.remove_file_if_holding(path, expected)
    }
    fn path_exists(&self, path: &Path) -> bool {
        RealFileSystem.path_exists(path)
    }
    fn directory_state(&self, path: &Path) -> DirectoryState {
        RealFileSystem.directory_state(path)
    }
    fn expand_path(&self, path: &Path) -> Result<PathBuf, FileSystemError> {
        RealFileSystem.expand_path(path)
    }
    fn list_directory(&self, path: &Path) -> Result<Vec<PathBuf>, FileSystemError> {
        RealFileSystem.list_directory(path)
    }
    fn canonicalize(&self, path: &Path) -> Result<PathBuf, FileSystemError> {
        RealFileSystem.canonicalize(path)
    }
    fn identity(&self, path: &Path) -> Result<FileIdentity, FileSystemError> {
        RealFileSystem.identity(path)
    }
    fn config_dir(&self) -> Result<PathBuf, FileSystemError> {
        RealFileSystem.config_dir()
    }
}
