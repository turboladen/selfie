pub mod filesystem;
pub mod real;

pub use self::filesystem::FileSystem;
pub use self::filesystem::FileSystemError;
pub use self::real::RealFileSystem;

#[cfg(feature = "with_mocks")]
pub use self::filesystem::MockFileSystem;
