//! Opening a file inside an extracted archive without following a symlink the
//! archive carries.

use std::fs::{self, File, OpenOptions};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Component, Path};

use anyhow::{Context, Result, bail};

/// Opens `rel` below `root` with `options`, refusing any symlink on the way.
///
/// Every directory between `root` and the file must be a real directory, and
/// the file must be a regular file, or be absent when `options` creates it.
/// `root` itself is trusted.
///
/// # Errors
///
/// Fails when `rel` is empty, absolute or contains `.` or `..`, when a
/// component below `root` is a symlink or not what it has to be, or when the
/// file cannot be opened.
pub fn open_in(root: &Path, rel: &Path, options: &mut OpenOptions) -> Result<File> {
    let mut parts = rel.components().peekable();
    if parts.peek().is_none() {
        bail!("refusing an empty path below {}", root.display());
    }
    let mut path = root.to_path_buf();
    while let Some(part) = parts.next() {
        let Component::Normal(name) = part else {
            bail!("refusing {}: not a plain relative path", rel.display());
        };
        path.push(name);
        if parts.peek().is_some() {
            let meta = fs::symlink_metadata(&path)
                .with_context(|| format!("could not stat {}", path.display()))?;
            if !meta.file_type().is_dir() {
                bail!(
                    "refusing {}: {} is a symlink or not a directory",
                    rel.display(),
                    path.display()
                );
            }
        }
    }
    // A commit can carry a symlink pointing anywhere, so the file itself is
    // opened with O_NOFOLLOW. `open` then fails with ELOOP on a symlink, even a
    // dangling one that O_CREAT would otherwise create a file through.
    let file = options
        .custom_flags(nix::libc::O_NOFOLLOW)
        .open(&path)
        .with_context(|| {
            format!(
                "could not open {} without following a symlink",
                path.display()
            )
        })?;
    if !file.metadata()?.file_type().is_file() {
        bail!("refusing {}: not a regular file", path.display());
    }
    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::os::unix::fs::symlink;

    fn append() -> OpenOptions {
        let mut options = OpenOptions::new();
        options.append(true);
        options
    }

    fn overwrite() -> OpenOptions {
        let mut options = OpenOptions::new();
        options.write(true).create(true).truncate(true);
        options
    }

    #[test]
    fn a_regular_file_below_real_directories_opens() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir_all(root.path().join("a/b")).unwrap();
        fs::write(root.path().join("a/b/f"), "x").unwrap();
        let mut file = open_in(root.path(), Path::new("a/b/f"), &mut append()).unwrap();
        file.write_all(b"y").unwrap();
        assert_eq!(fs::read_to_string(root.path().join("a/b/f")).unwrap(), "xy");
    }

    #[test]
    fn a_symlinked_file_is_refused_and_its_target_left_alone() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let victim = outside.path().join("victim");
        fs::write(&victim, "keep").unwrap();
        symlink(&victim, root.path().join("Justfile")).unwrap();
        assert!(open_in(root.path(), Path::new("Justfile"), &mut overwrite()).is_err());
        assert!(open_in(root.path(), Path::new("Justfile"), &mut append()).is_err());
        assert_eq!(fs::read_to_string(&victim).unwrap(), "keep");
    }

    #[test]
    fn a_dangling_symlink_is_not_created_through() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let victim = outside.path().join("new");
        symlink(&victim, root.path().join("Justfile")).unwrap();
        assert!(open_in(root.path(), Path::new("Justfile"), &mut overwrite()).is_err());
        assert!(!victim.exists());
    }

    #[test]
    fn a_symlinked_directory_on_the_way_is_refused() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::create_dir(outside.path().join("src")).unwrap();
        let victim = outside.path().join("src/lib.rs");
        fs::write(&victim, "keep").unwrap();
        symlink(outside.path(), root.path().join("crates")).unwrap();
        assert!(open_in(root.path(), Path::new("crates/src/lib.rs"), &mut append()).is_err());
        assert_eq!(fs::read_to_string(&victim).unwrap(), "keep");
    }

    #[test]
    fn a_path_that_is_not_plain_and_relative_is_refused() {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("f"), "x").unwrap();
        for rel in ["", "/etc/hosts", "../f", "./f", "a/../f"] {
            assert!(
                open_in(root.path(), Path::new(rel), &mut append()).is_err(),
                "{rel:?} was opened"
            );
        }
    }

    #[test]
    fn a_missing_file_is_created_when_the_options_create() {
        let root = tempfile::tempdir().unwrap();
        open_in(root.path(), Path::new("Justfile"), &mut overwrite()).unwrap();
        assert!(root.path().join("Justfile").is_file());
    }
}
