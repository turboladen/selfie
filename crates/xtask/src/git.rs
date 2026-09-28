//! The few git operations the harness needs, run against the repository at a
//! given path.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

/// A git repository, addressed by its top-level directory.
pub struct Repo {
    root: PathBuf,
}

impl Repo {
    /// Finds the repository containing the current directory.
    ///
    /// # Errors
    ///
    /// Fails outside a git repository.
    pub fn discover() -> Result<Self> {
        let out = run_git(Path::new("."), &["rev-parse", "--show-toplevel"])?;
        let root = fs::canonicalize(out.trim()).context("could not resolve the repository root")?;
        Ok(Self { root })
    }

    /// Every checkout of this repository: the main one and each linked
    /// worktree, canonicalized. A checkout whose path no longer exists is
    /// left out.
    ///
    /// # Errors
    ///
    /// Fails when git cannot list the worktrees.
    pub fn checkouts(&self) -> Result<Vec<PathBuf>> {
        let listing = self.git(&["worktree", "list", "--porcelain"])?;
        let mut roots: Vec<PathBuf> = listing
            .lines()
            .filter_map(|line| line.strip_prefix("worktree "))
            .filter_map(|path| fs::canonicalize(path).ok())
            .collect();
        roots.push(self.root.clone());
        Ok(roots)
    }

    fn git(&self, args: &[&str]) -> Result<String> {
        run_git(&self.root, args)
    }

    /// Resolves `rev` to a full commit SHA.
    ///
    /// # Errors
    ///
    /// Fails when `rev` does not name a commit.
    pub fn commit(&self, rev: &str) -> Result<String> {
        let spec = format!("{rev}^{{commit}}");
        let sha = self
            .git(&["rev-parse", "--verify", "--quiet", &spec])
            .with_context(|| format!("{rev} does not name a commit"))?;
        Ok(sha.trim().to_owned())
    }

    /// The best common ancestor of `a` and `b`.
    ///
    /// # Errors
    ///
    /// Fails when the two share no history.
    pub fn merge_base(&self, a: &str, b: &str) -> Result<String> {
        Ok(self.git(&["merge-base", a, b])?.trim().to_owned())
    }

    /// The commits in `base..head`, oldest first.
    ///
    /// # Errors
    ///
    /// Fails when git cannot list the range.
    pub fn range(&self, base: &str, head: &str) -> Result<Vec<String>> {
        let range = format!("{base}..{head}");
        let out = self.git(&["rev-list", "--reverse", &range])?;
        Ok(out.lines().map(str::to_owned).collect())
    }

    /// The first line of a commit's message.
    ///
    /// # Errors
    ///
    /// Fails when `sha` is not a commit.
    pub fn subject(&self, sha: &str) -> Result<String> {
        Ok(self
            .git(&["log", "-1", "--format=%s", sha])?
            .trim()
            .to_owned())
    }

    /// The content of `path` as committed at `sha`.
    ///
    /// # Errors
    ///
    /// Fails when the file does not exist at that commit or is not UTF-8.
    pub fn show(&self, sha: &str, path: &str) -> Result<String> {
        self.git(&["show", &format!("{sha}:{path}")])
    }

    /// Whether the working tree differs from `HEAD`, counting untracked files
    /// that are not ignored.
    ///
    /// # Errors
    ///
    /// Fails when git cannot answer.
    pub fn is_dirty(&self) -> Result<bool> {
        // Explicit, because `status.showUntrackedFiles=no` would otherwise
        // hide an untracked module the working tree compiles with.
        let status = self.git(&["status", "--porcelain", "--untracked-files=normal"])?;
        Ok(!status.is_empty())
    }

    /// Extracts the tree of `sha` into `dest`, which must not exist yet.
    ///
    /// # Errors
    ///
    /// Fails when `dest` exists or either git or tar fails.
    pub fn archive(&self, sha: &str, dest: &Path) -> Result<()> {
        fs::create_dir(dest).with_context(|| format!("could not create {}", dest.display()))?;
        let tar = dest.with_extension("tar");
        let tar_arg = tar.to_str().context("archive path is not UTF-8")?;
        self.git(&["archive", "--format=tar", "-o", tar_arg, sha])?;
        let status = Command::new("tar")
            .arg("-xf")
            .arg(&tar)
            .arg("-C")
            .arg(dest)
            .status()?;
        if !status.success() {
            bail!("tar could not extract {}: {status}", tar.display());
        }
        fs::remove_file(&tar)?;
        Ok(())
    }
}

fn run_git(dir: &Path, args: &[&str]) -> Result<String> {
    // No optional locks: `git status` otherwise refreshes the index and can
    // make another writer's `git add` in the same checkout fail on index.lock.
    let out = Command::new("git")
        .current_dir(dir)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .args(args)
        .output()
        .context("could not run git")?;
    if !out.status.success() {
        bail!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    String::from_utf8(out.stdout).context("git printed non-UTF-8 output")
}
