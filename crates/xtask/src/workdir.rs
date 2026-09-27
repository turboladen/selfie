//! Choosing the directory a run writes its archives, targets and logs into.

use std::env;
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

/// Returns a canonical, absolute work directory outside every path in
/// `forbidden`.
///
/// With `explicit`, that directory is used, and must be new or empty. Without
/// it, a new directory named after `command` is created under `TMPDIR` (or
/// `/tmp`) and kept after the run, so its logs can be read.
///
/// # Errors
///
/// Refuses a relative `explicit` path, a relative `TMPDIR`, a non-empty
/// `explicit` directory, and any directory inside `forbidden`. Nothing is
/// created when it refuses.
pub fn resolve(explicit: Option<&Path>, command: &str, forbidden: &[PathBuf]) -> Result<PathBuf> {
    if let Some(path) = explicit {
        if !path.is_absolute() {
            bail!("--work-dir must be absolute, got {}", path.display());
        }
        refuse_inside(&prospective(path)?, forbidden)?;
        if path.exists() && fs::read_dir(path)?.next().is_some() {
            bail!(
                "--work-dir {} is not empty; name a new or empty directory",
                path.display()
            );
        }
        fs::create_dir_all(path).with_context(|| format!("could not create {}", path.display()))?;
        return Ok(fs::canonicalize(path)?);
    }

    // `std::env::temp_dir` hands back a relative TMPDIR verbatim, which would
    // put archives and sandboxes under the current directory.
    let base = match env::var_os("TMPDIR") {
        Some(tmp) if !tmp.is_empty() => PathBuf::from(tmp),
        _ => PathBuf::from("/tmp"),
    };
    if !base.is_absolute() {
        bail!("TMPDIR must be absolute, got {}", base.display());
    }
    let base = fs::canonicalize(&base)
        .with_context(|| format!("could not resolve TMPDIR {}", base.display()))?;
    refuse_inside(&base, forbidden)?;
    let dir = tempfile::Builder::new()
        .prefix(&format!("xtask-{command}-"))
        .tempdir_in(&base)?
        .keep();
    Ok(dir)
}

// This computes the path `path` will have once created: its nearest existing
// ancestor resolved through any symlinks, with the missing components
// appended. The check has to happen before anything is created, or a refused
// directory is left behind inside the checkout.
fn prospective(path: &Path) -> Result<PathBuf> {
    let mut existing = path;
    let mut missing = Vec::new();
    while !existing.exists() {
        let name = existing
            .file_name()
            .with_context(|| format!("cannot resolve {}", path.display()))?;
        missing.push(name.to_owned());
        existing = existing
            .parent()
            .with_context(|| format!("cannot resolve {}", path.display()))?;
    }
    let mut resolved = fs::canonicalize(existing)?;
    resolved.extend(missing.iter().rev());
    Ok(resolved)
}

// Inside a checkout, typos, dprint and `git status` would all sweep up the
// archives, and cargo would see a second workspace.
fn refuse_inside(dir: &Path, forbidden: &[PathBuf]) -> Result<()> {
    if let Some(root) = forbidden.iter().find(|root| dir.starts_with(root)) {
        bail!(
            "the work directory {} is inside the checkout {}; choose one outside it",
            dir.display(),
            root.display()
        );
    }
    Ok(())
}

/// A run's report: each line goes to stdout and to a file as it is produced,
/// so a run that is interrupted still leaves its verdicts behind.
pub struct Summary(File);

impl Summary {
    /// Creates the report file at `path`.
    ///
    /// # Errors
    ///
    /// Fails when `path` already exists.
    pub fn create(path: &Path) -> Result<Self> {
        Ok(Self(File::create_new(path)?))
    }

    /// Prints `text` and appends it to the file.
    ///
    /// # Errors
    ///
    /// Fails when the file cannot be written.
    pub fn line(&mut self, text: &str) -> Result<()> {
        println!("{text}");
        writeln!(self.0, "{text}")?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roots(dir: &Path) -> Vec<PathBuf> {
        vec![fs::canonicalize(dir).unwrap()]
    }

    #[test]
    fn a_relative_work_dir_is_refused() {
        let err = resolve(Some(Path::new("scratch")), "t", &[]).unwrap_err();
        assert!(err.to_string().contains("must be absolute"), "{err}");
    }

    #[test]
    fn a_work_dir_inside_a_checkout_is_refused_before_anything_is_created() {
        let repo = tempfile::tempdir().unwrap();
        let wanted = repo.path().join("a").join("b");
        let err = resolve(Some(&wanted), "t", &roots(repo.path())).unwrap_err();
        assert!(err.to_string().contains("inside the checkout"), "{err}");
        assert!(
            !repo.path().join("a").exists(),
            "a refused directory was created"
        );
    }

    #[test]
    fn a_symlink_into_a_checkout_is_refused() {
        let repo = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        let link = elsewhere.path().join("link");
        std::os::unix::fs::symlink(repo.path(), &link).unwrap();
        let err = resolve(Some(&link.join("x")), "t", &roots(repo.path())).unwrap_err();
        assert!(err.to_string().contains("inside the checkout"), "{err}");
    }

    #[test]
    fn a_non_empty_work_dir_is_refused_and_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("summary.txt"), "precious").unwrap();
        let err = resolve(Some(dir.path()), "t", &[]).unwrap_err();
        assert!(err.to_string().contains("not empty"), "{err}");
        let kept = fs::read_to_string(dir.path().join("summary.txt")).unwrap();
        assert_eq!(kept, "precious");
    }

    #[test]
    fn a_new_work_dir_outside_every_checkout_is_created() {
        let repo = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let wanted = outside.path().join("run");
        let dir = resolve(Some(&wanted), "t", &roots(repo.path())).unwrap();
        assert!(dir.is_absolute() && dir.is_dir(), "{}", dir.display());
    }
}
