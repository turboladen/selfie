pub mod filesystem;
pub mod real;
pub mod target;

pub use self::filesystem::FileSystem;
pub use self::filesystem::FileSystemError;
pub use self::real::RealFileSystem;
pub use self::target::{HomeDir, TargetPath, expand_target_path};

#[cfg(feature = "with_mocks")]
pub use self::filesystem::MockFileSystem;
