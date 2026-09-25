pub mod filesystem;
pub mod real;
pub mod target;

pub use self::filesystem::FileSystem;
pub use self::filesystem::FileSystemError;
pub use self::filesystem::{AbsentReason, DirectoryState, TargetRead, shell_quote};
pub use self::real::RealFileSystem;
pub use self::target::{
    HomeDir, StatePathError, TargetPath, TargetRejection, deploy_target, expand_target_path,
    state_directory,
};

#[cfg(feature = "with_mocks")]
pub use self::filesystem::MockFileSystem;
