//! gix-based implementation of [`GitStatusProvider`] and [`GitSyncProvider`].
//!
//! Uses the `gix` crate for local git operations (discover, status, stage,
//! commit) and shells out to the `git` binary for:
//! - **Network operations** (push, fetch) — to leverage the user's existing
//!   SSH agent and credential helpers, which gix's transport layer doesn't
//!   automatically integrate with.
//! - **Fast-forward merge** — because it updates both the ref and the working
//!   tree, requiring checkout logic (`gix-worktree-state`) we don't pull in.
//! - **Ancillary queries** (branch name, remote, ahead/behind, deleted files,
//!   diff) — tracked for conversion to native gix.

use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
};

use super::message::GitMessage;
use super::status_provider::{
    GitDirectoryStatus, GitFileStatus, GitStatusError, GitStatusProvider,
};
use super::sync_provider::{
    ChangeType, ChangedFile, CommitId, FastForwardResult, GitSyncError, GitSyncProvider, RepoInfo,
    RepoStatus,
};

/// Git adapter backed by the `gix` crate (local ops) and `git` CLI (network ops).
///
/// [`GixGitAdapter::default`] reads git configuration as `git` itself does:
/// the repository's, the user's global file and the system file.
#[derive(Debug, Clone, Default)]
pub struct GixGitAdapter {
    scope: ConfigScope,
}

impl GixGitAdapter {
    /// An adapter whose git reads only each repository's own configuration and
    /// none of its caller's environment. No developer's hooks, includes,
    /// excludes, attributes, identity, templates or signing key reach a test
    /// repository, and nothing an outer git exports points it elsewhere.
    ///
    /// For tests only. A user's run must read their configuration, so code
    /// outside tests uses [`GixGitAdapter::default`].
    #[doc(hidden)]
    #[must_use]
    pub fn isolated() -> Self {
        Self {
            scope: ConfigScope::Isolated,
        }
    }
}

// A scope names which git configuration the adapter's git and gix read.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum ConfigScope {
    // Git reads the repository's, the global and the system configuration.
    #[default]
    User,
    // Git reads the repository's configuration and nothing else.
    Isolated,
}

impl ConfigScope {
    // Have `git` read the configuration this scope names.
    fn apply(self, git: &mut Command) {
        if self == Self::Isolated {
            isolated_environment(git);
        }
    }
}

// The one recipe for an isolated git: start from an empty environment, so
// nothing the caller exports reaches git (an outer git's `GIT_DIR` or `-c`
// settings, `GIT_AUTHOR_*`, `GIT_TEMPLATE_DIR`, `EMAIL`), then add back only
// what git needs. A `HOME` and `XDG_CONFIG_HOME` of /dev/null hold no config,
// ignore or attributes file, and git 2.50 reads them as absent without a
// warning. An empty `GIT_TEMPLATE_DIR` copies no templates, also silently;
// /dev/null would warn. The identity is fixed, since no config supplies one.
fn isolated_environment(git: &mut Command) {
    git.env_clear();
    if let Some(path) = std::env::var_os("PATH") {
        git.env("PATH", path);
    }
    git.env("HOME", "/dev/null")
        .env("XDG_CONFIG_HOME", "/dev/null")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_ATTR_NOSYSTEM", "1")
        .env("GIT_TEMPLATE_DIR", "")
        .env("GIT_AUTHOR_NAME", "selfie-test")
        .env("GIT_AUTHOR_EMAIL", "selfie-test@example.invalid")
        .env("GIT_COMMITTER_NAME", "selfie-test")
        .env("GIT_COMMITTER_EMAIL", "selfie-test@example.invalid")
        .env("GIT_TERMINAL_PROMPT", "0");
}

/// Give `git` the environment [`GixGitAdapter::isolated`] runs its git in:
/// empty apart from `PATH`, a fixed identity and settings that turn off every
/// global, system and template source.
///
/// For tests only, as [`GixGitAdapter::isolated`] is.
#[doc(hidden)]
pub fn isolate_git_config(git: &mut Command) {
    ConfigScope::Isolated.apply(git);
}

// ─── Helpers ─────────────────────────────────────────────────────────────────

/// Open a `gix` repo from a path, mapping errors to `GitSyncError`.
///
/// Distinguishes "not a git repository" from real I/O or corruption errors,
/// matching the error discrimination in `GitStatusProvider::status_for_directory`.
fn open_repo(scope: ConfigScope, path: &Path) -> Result<gix::Repository, GitSyncError> {
    discover(scope, path).map_err(|e| match e {
        gix::discover::Error::Discover(_) => GitSyncError::NotARepo {
            path: path.to_path_buf(),
        },
        other => GitSyncError::OperationFailed {
            operation: GitMessage::new("discover repository"),
            message: GitMessage::new(other),
        },
    })
}

// `gix::discover`, reading only the repository's own configuration when
// `scope` is isolated.
#[expect(
    clippy::result_large_err,
    reason = "gix's own discover error, returned as gix::discover returns it"
)]
fn discover(scope: ConfigScope, path: &Path) -> Result<gix::Repository, gix::discover::Error> {
    match scope {
        ConfigScope::User => gix::discover(path),
        ConfigScope::Isolated => {
            let isolated = gix::sec::trust::Mapping {
                full: gix::open::Options::isolated(),
                reduced: gix::open::Options::isolated(),
            };
            gix::ThreadSafeRepository::discover_opts(path, Default::default(), isolated)
                .map(Into::into)
        }
    }
}

/// Check if a file has the executable bit set.
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|m| m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

/// Shorthand for creating a [`GitSyncError::OperationFailed`] from any error.
fn git_sync_err(operation: &str, e: impl std::fmt::Display) -> GitSyncError {
    GitSyncError::OperationFailed {
        operation: GitMessage::new(operation),
        message: GitMessage::new(e),
    }
}

/// Run a `git` CLI command in the given directory.
///
/// Stdin is set to `Stdio::null` and `GIT_TERMINAL_PROMPT=0` to prevent git
/// from blocking on credential prompts, which is critical for non-interactive
/// callers like the MCP server.
fn run_git(scope: ConfigScope, repo_root: &Path, args: &[&str]) -> Result<String, GitSyncError> {
    let output = git_output(scope, repo_root, args)?;

    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    } else {
        Err(failure(args, &output, ""))
    }
}

// Run `git` and hand back its output whatever its exit status, for a caller
// that reads stdout of a failed run. Fails only when git cannot be started.
fn git_output(scope: ConfigScope, repo_root: &Path, args: &[&str]) -> Result<Output, GitSyncError> {
    git_command(scope, repo_root, args)
        .output()
        .map_err(|e| GitSyncError::OperationFailed {
            operation: GitMessage::new(format!("git {}", args.join(" "))),
            message: GitMessage::new(e),
        })
}

// The error for a git run that exited non-zero.
//
// git's stderr is process output selfie does not control, and it reaches an AI
// assistant's transcript through the sync MCP tools. `GitMessage::from_stderr`
// is what redacts a credential out of it and bounds what is left; there is no
// path here that hands the raw bytes on.
//
// `status_lines` is extra text for the message, which is redacted and bounded
// together with stderr.
fn failure(args: &[&str], output: &Output, status_lines: &str) -> GitSyncError {
    let mut text = output.stderr.trim_ascii().to_vec();
    if !status_lines.is_empty() {
        if !text.is_empty() {
            text.push(b'\n');
        }
        text.extend_from_slice(status_lines.as_bytes());
    }
    GitSyncError::OperationFailed {
        operation: GitMessage::new(format!("git {}", args.join(" "))),
        message: GitMessage::from_stderr(&text),
    }
}

// The `git` command `run_git` runs, built in one place so the leak tests can
// run exactly what it runs.
fn git_command(scope: ConfigScope, repo_root: &Path, args: &[&str]) -> Command {
    let mut command = Command::new("git");
    command
        .args(args)
        .current_dir(repo_root)
        .stdin(Stdio::null())
        .env("GIT_TERMINAL_PROMPT", "0");
    scope.apply(&mut command);
    command
}

// The arguments `push` runs git with. `--porcelain` puts one tab-separated
// status line per ref on stdout, which the classification below reads.
const PUSH_ARGS: [&str; 2] = ["push", "--porcelain"];

// Whether `git push --porcelain` refused to send `current_ref`, the branch being
// pushed, because the remote has commits it lacks. The current branch is named
// by its full ref, or as `HEAD` when a `HEAD` or `HEAD:<ref>` refspec pushed it.
// Each ref's line is
// `<flag>\t<from>:<to>\t<summary> (<reason>)`. A flag of `!` with the summary
// and reason `[rejected] (fetch first)` or `[rejected] (non-fast-forward)` on
// the current branch's own line is that refusal. git's transport.c prints these
// strings untranslated, so they hold under any locale.
//
// Other lines do not count: `[remote rejected]` (a server hook or a protected
// branch) and `[remote failure]` are not fixed by pulling, nor is a tag or
// another branch rejected beside the current one.
fn push_was_rejected(porcelain_stdout: &str, current_ref: &str) -> bool {
    porcelain_stdout.lines().any(|line| {
        let mut fields = line.split('\t');
        let (flag, refs, status) = (fields.next(), fields.next(), fields.next());
        flag == Some("!")
            && refs
                .and_then(|refs| refs.split_once(':'))
                .is_some_and(|(from, _)| from == current_ref || from == "HEAD")
            && matches!(
                status,
                Some("[rejected] (fetch first)" | "[rejected] (non-fast-forward)")
            )
    })
}

// The `!` lines of `git push --porcelain`'s stdout: which refs failed and why.
// The `To <url>` line is left out, since a remote URL can carry a credential.
fn failed_ref_lines(porcelain_stdout: &str) -> String {
    porcelain_stdout
        .lines()
        .filter(|line| line.starts_with("!\t"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Intermediate representation of gix status output, shared between
/// [`GitStatusProvider::status_for_directory`] and
/// [`GitSyncProvider::repo_status`].
struct RawStatus {
    worktree_mods: HashSet<String>,
    index_mods: HashSet<String>,
    untracked: Vec<String>,
}

/// Iterate gix status items and classify them into worktree modifications,
/// index (staged) modifications, and untracked files.
fn collect_raw_status(repo: &gix::Repository) -> Result<RawStatus, String> {
    let platform = repo
        .status(gix::progress::Discard)
        .map_err(|e| e.to_string())?;

    let iter = platform
        .into_iter(std::iter::empty::<gix::bstr::BString>())
        .map_err(|e| e.to_string())?;

    let mut worktree_mods = HashSet::new();
    let mut index_mods = HashSet::new();
    let mut untracked = Vec::new();

    for item in iter {
        let item = item.map_err(|e| e.to_string())?;

        match item {
            gix::status::Item::IndexWorktree(iw_item) => {
                use gix::status::index_worktree::Item;
                match iw_item {
                    Item::Modification { rela_path, .. } => {
                        worktree_mods.insert(rela_path.to_string());
                    }
                    Item::DirectoryContents { entry, .. } => {
                        if entry.status == gix::dir::entry::Status::Untracked {
                            untracked.push(entry.rela_path.to_string());
                        }
                    }
                    Item::Rewrite { .. } => {}
                }
            }
            gix::status::Item::TreeIndex(change) => {
                let location = change.location().to_string();
                index_mods.insert(location);
            }
        }
    }

    Ok(RawStatus {
        worktree_mods,
        index_mods,
        untracked,
    })
}

/// Parse a single line from `git diff --name-status` output into a
/// [`ChangedFile`], returning `None` for blank or unparsable lines.
///
/// Rename/copy lines have the format `R100\told\tnew` (three tab-separated
/// fields). We take the *last* field (the new path) for those statuses.
fn parse_diff_line(line: &str) -> Option<ChangedFile> {
    let fields: Vec<&str> = line.split('\t').collect();
    if fields.len() < 2 {
        return None;
    }

    let status = fields[0];
    let change_type = match status {
        "A" => ChangeType::Added,
        "M" => ChangeType::Modified,
        "D" => ChangeType::Deleted,
        _ => ChangeType::Modified, // R*, C*, etc. → treat as modified
    };

    // For rename/copy (R*, C*) use the last field (new path); otherwise use
    // the second field.
    let path = fields.last().filter(|p| !p.is_empty())?;

    Some(ChangedFile {
        path: PathBuf::from(path),
        change_type,
    })
}

// ─── GitStatusProvider (read-only, existing) ─────────────────────────────────

impl GitStatusProvider for GixGitAdapter {
    fn status_for_directory(&self, directory: &Path) -> Result<GitDirectoryStatus, GitStatusError> {
        let repo = match discover(self.scope, directory) {
            Ok(repo) => repo,
            Err(gix::discover::Error::Discover(_)) => {
                return Ok(GitDirectoryStatus {
                    in_repo: false,
                    files: HashMap::new(),
                });
            }
            Err(other) => {
                return Err(GitStatusError::StatusError(GitMessage::new(other)));
            }
        };

        let workdir = repo
            .workdir()
            .ok_or_else(|| GitStatusError::StatusError(GitMessage::new("bare repository")))?;
        let workdir = dunce::canonicalize(workdir).unwrap_or_else(|_| workdir.to_path_buf());

        let raw = collect_raw_status(&repo)
            .map_err(|e| GitStatusError::StatusError(GitMessage::new(e)))?;

        let mut files = HashMap::new();

        for path in &raw.untracked {
            files.insert(workdir.join(path), GitFileStatus::Untracked);
        }

        for path in &raw.worktree_mods {
            let status = if raw.index_mods.contains(path) {
                GitFileStatus::StagedAndModified
            } else {
                GitFileStatus::Modified
            };
            files.insert(workdir.join(path), status);
        }

        for path in &raw.index_mods {
            if !raw.worktree_mods.contains(path) {
                files.insert(workdir.join(path), GitFileStatus::Staged);
            }
        }

        Ok(GitDirectoryStatus {
            in_repo: true,
            files,
        })
    }
}

// ─── GitSyncProvider (write + network) ───────────────────────────────────────

impl GitSyncProvider for GixGitAdapter {
    fn discover_repo(&self, path: &Path) -> Result<RepoInfo, GitSyncError> {
        let repo = open_repo(self.scope, path)?;

        let root = repo
            .workdir()
            .ok_or_else(|| GitSyncError::OperationFailed {
                operation: GitMessage::new("discover"),
                message: GitMessage::new("bare repository"),
            })?
            .to_path_buf();
        let root = dunce::canonicalize(&root).unwrap_or(root);

        // Get branch name via git CLI (more reliable across gix versions)
        let branch = run_git(self.scope, &root, &["rev-parse", "--abbrev-ref", "HEAD"])
            .ok()
            .filter(|b| b != "HEAD"); // Detached HEAD → None

        // Get remote name via git CLI
        let remote_name = run_git(self.scope, &root, &["remote"])
            .ok()
            .and_then(|output| output.lines().next().map(String::from));

        Ok(RepoInfo {
            root,
            branch,
            remote_name,
        })
    }

    fn repo_status(&self, repo_root: &Path) -> Result<RepoStatus, GitSyncError> {
        let repo = open_repo(self.scope, repo_root)?;

        let raw = collect_raw_status(&repo).map_err(|msg| GitSyncError::OperationFailed {
            operation: GitMessage::new("status"),
            message: GitMessage::new(msg),
        })?;

        let modified = raw.worktree_mods.iter().map(PathBuf::from).collect();
        let staged = raw.index_mods.iter().map(PathBuf::from).collect();
        let untracked = raw.untracked.iter().map(PathBuf::from).collect();

        // Detect deleted files via git CLI (gix status doesn't surface deletions cleanly)
        let mut deleted = Vec::new();
        if let Ok(output) = run_git(self.scope, repo_root, &["ls-files", "--deleted"]) {
            for line in output.lines().filter(|l| !l.is_empty()) {
                deleted.push(PathBuf::from(line));
            }
        }

        // Ahead/behind via git CLI (simpler than gix for remote tracking)
        let (ahead, behind) = self.ahead_behind_counts(repo_root);

        Ok(RepoStatus {
            modified,
            staged,
            untracked,
            deleted,
            ahead,
            behind,
        })
    }

    fn stage_files(&self, repo_root: &Path, files: &[PathBuf]) -> Result<(), GitSyncError> {
        if files.is_empty() {
            return Ok(());
        }

        let repo = open_repo(self.scope, repo_root)?;
        let mut index = repo
            .open_index()
            .map_err(|e| git_sync_err("open index", e))?;

        let workdir = repo
            .workdir()
            .ok_or_else(|| GitSyncError::OperationFailed {
                operation: GitMessage::new("stage"),
                message: GitMessage::new("bare repository"),
            })?;

        for file in files {
            let full_path = workdir.join(file);
            let relative_bstring = gix::path::into_bstr(file);
            let relative_path: &gix::bstr::BStr = relative_bstring.as_ref();

            if full_path.exists() {
                // File exists → hash it as a blob and upsert into the index.
                let data = std::fs::read(&full_path).map_err(|e| git_sync_err("read file", e))?;
                let blob_id = repo
                    .write_blob(&data)
                    .map_err(|e| git_sync_err("write blob", e))?;

                let entry_mode = if is_executable(&full_path) {
                    gix::index::entry::Mode::FILE_EXECUTABLE
                } else {
                    gix::index::entry::Mode::FILE
                };

                match index.entry_index_by_path_and_stage(
                    relative_path,
                    gix::index::entry::Stage::Unconflicted,
                ) {
                    Some(pos) => {
                        // Update existing entry in place.
                        let entry = &mut index.entries_mut()[pos];
                        entry.id = blob_id.detach();
                        entry.mode = entry_mode;
                        entry.flags.remove(
                            gix::index::entry::Flags::ASSUME_VALID
                                | gix::index::entry::Flags::UPDATE,
                        );
                    }
                    None => {
                        // New file — push and re-sort.
                        index.dangerously_push_entry(
                            Default::default(),
                            blob_id.detach(),
                            gix::index::entry::Flags::empty(),
                            entry_mode,
                            relative_path,
                        );
                        index.sort_entries();
                    }
                }
            } else {
                // File deleted → remove from index.
                let relative_owned = relative_path.to_owned();
                index.remove_entries(|_, path, _| path == relative_owned);
            }
        }

        index
            .write(Default::default())
            .map_err(|e| git_sync_err("write index", e))?;
        Ok(())
    }

    fn commit(&self, _repo_root: &Path, _message: &str) -> Result<CommitId, GitSyncError> {
        // commit_tree can build a tree that deletes every tracked file (seen on
        // a nested path, selfie-ugf9.6). Refused until that is fixed and tested.
        Err(GitSyncError::CommitDisabled)
    }

    fn push(&self, repo_root: &Path) -> Result<(), GitSyncError> {
        // Shells out because gix's transport layer (blocking-network-client)
        // doesn't automatically integrate with the user's SSH agent and
        // credential helpers the way the git CLI does.
        //
        // The classification reads the porcelain status lines rather than
        // git's messages, so it holds under any locale and leaves git's own
        // error text, which is relayed to the user, in their language.
        let output = git_output(self.scope, repo_root, &PUSH_ARGS)?;
        if !output.status.success() {
            let stdout = String::from_utf8_lossy(&output.stdout);
            // A detached HEAD has no branch to pull into, so it has no current
            // ref and keeps the generic failure.
            let current_ref =
                run_git(self.scope, repo_root, &["symbolic-ref", "--quiet", "HEAD"]).ok();
            return Err(
                if current_ref.is_some_and(|current| push_was_rejected(&stdout, &current)) {
                    GitSyncError::PushRejected
                } else {
                    failure(&PUSH_ARGS, &output, &failed_ref_lines(&stdout))
                },
            );
        }
        Ok(())
    }

    fn fetch(&self, repo_root: &Path) -> Result<(), GitSyncError> {
        // Shells out for the same reason as push — SSH/credential integration.
        run_git(self.scope, repo_root, &["fetch"])?;
        Ok(())
    }

    fn fast_forward(&self, repo_root: &Path) -> Result<FastForwardResult, GitSyncError> {
        // Shells out because ff-merge updates both the ref AND the working tree.
        // gix can update refs, but a full working-tree checkout (handling file
        // modes, symlinks, dirty-file guards) requires gix-worktree-state which
        // we don't pull in.
        // The counts decide before git runs, so a fast-forward that fails for
        // another reason (an untracked file in the way) stays an error.
        let (ahead, behind) = self.ahead_behind(repo_root)?;
        if behind == 0 {
            return Ok(FastForwardResult::AlreadyUpToDate);
        }
        if ahead > 0 {
            return Ok(FastForwardResult::Diverged);
        }

        let old_head = run_git(self.scope, repo_root, &["rev-parse", "HEAD"])?;
        run_git(self.scope, repo_root, &["merge", "--ff-only", "@{u}"])?;
        let new_head = run_git(self.scope, repo_root, &["rev-parse", "HEAD"])?;

        let commit_count = run_git(
            self.scope,
            repo_root,
            &["rev-list", "--count", &format!("{old_head}..{new_head}")],
        )?
        .parse::<usize>()
        .unwrap_or(0);

        Ok(FastForwardResult::Advanced {
            from: CommitId(old_head),
            to: CommitId(new_head),
            commit_count,
        })
    }

    fn diff_commits(
        &self,
        repo_root: &Path,
        from: &CommitId,
        to: &CommitId,
    ) -> Result<Vec<ChangedFile>, GitSyncError> {
        let output = run_git(
            self.scope,
            repo_root,
            &["diff", "--name-status", &from.0, &to.0],
        )?;
        Ok(output.lines().filter_map(parse_diff_line).collect())
    }
}

impl GixGitAdapter {
    /// Get ahead/behind counts relative to the upstream tracking branch.
    /// Returns (0, 0) if no upstream is configured.
    fn ahead_behind_counts(&self, repo_root: &Path) -> (usize, usize) {
        self.ahead_behind(repo_root).unwrap_or((0, 0))
    }

    /// Get ahead/behind counts relative to the upstream tracking branch, or the
    /// error git gave, which includes there being no upstream.
    fn ahead_behind(&self, repo_root: &Path) -> Result<(usize, usize), GitSyncError> {
        let counts = run_git(
            self.scope,
            repo_root,
            &["rev-list", "--left-right", "--count", "HEAD...@{u}"],
        )?;
        let mut parts = counts.split_whitespace().map(str::parse::<usize>);
        match (parts.next(), parts.next()) {
            (Some(Ok(ahead)), Some(Ok(behind))) => Ok((ahead, behind)),
            _ => Err(git_sync_err(
                "rev-list --count",
                format!("unexpected output {counts:?}"),
            )),
        }
    }
}

// The commit implementation sync push used before it was disabled. It can
// build a tree that deletes every tracked file; kept, with its test, for the
// fix to start from.
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "commit is disabled until the tree bug is fixed")
)]
fn commit_tree(
    scope: ConfigScope,
    repo_root: &Path,
    message: &str,
) -> Result<CommitId, GitSyncError> {
    let repo = open_repo(scope, repo_root)?;
    let index = repo
        .open_index()
        .map_err(|e| git_sync_err("open index", e))?;

    // Build a tree from the current index by starting from HEAD's tree
    // (or the empty tree for the first commit) and upserting every entry.
    let head_tree_id = repo
        .head_tree_id_or_empty()
        .map_err(|e| git_sync_err("head tree", e))?;
    let mut editor = repo
        .edit_tree(head_tree_id)
        .map_err(|e| git_sync_err("edit tree", e))?;

    // Collect HEAD tree entries so we can detect deletions.
    let head_entries: HashSet<String> = repo
        .find_tree(head_tree_id)
        .ok()
        .map(|tree| {
            tree.traverse()
                .breadthfirst
                .files()
                .unwrap_or_default()
                .into_iter()
                .map(|r| r.filepath.to_string())
                .collect()
        })
        .unwrap_or_default();

    // Upsert all index entries into the tree.
    let index_paths: HashSet<String> = index
        .entries()
        .iter()
        .map(|e| e.path(&index).to_string())
        .collect();

    for entry in index.entries() {
        let path = entry.path(&index);
        let kind = match entry.mode {
            gix::index::entry::Mode::FILE_EXECUTABLE => {
                gix::object::tree::EntryKind::BlobExecutable
            }
            gix::index::entry::Mode::SYMLINK => gix::object::tree::EntryKind::Link,
            _ => gix::object::tree::EntryKind::Blob,
        };
        editor
            .upsert(path.to_owned(), kind, entry.id)
            .map_err(|e| git_sync_err("upsert tree entry", e))?;
    }

    // Remove entries that were in HEAD but are no longer in the index.
    for path in &head_entries {
        if !index_paths.contains(path) {
            let bpath: &gix::bstr::BStr = path.as_str().into();
            editor
                .remove(bpath.to_owned())
                .map_err(|e| git_sync_err("remove tree entry", e))?;
        }
    }

    let tree_id = editor.write().map_err(|e| git_sync_err("write tree", e))?;

    // Determine parent(s).
    let parents: Vec<gix::ObjectId> = repo
        .head_id()
        .map(|id| vec![id.detach()])
        .unwrap_or_default();

    let commit_id = repo
        .commit("HEAD", message, tree_id, parents)
        .map_err(|e| git_sync_err("commit", e))?;

    Ok(CommitId(commit_id.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    use test_common::assert_secret_free;

    // This returns this crate's own adapter, isolated.
    // `test_common::isolated_git_adapter` builds the adapter of the `selfie`
    // that test-common links, which is a different crate from the one these
    // tests compile.
    fn isolated() -> GixGitAdapter {
        GixGitAdapter {
            scope: ConfigScope::Isolated,
        }
    }

    // Run `git` with `args` in `path` as this crate's isolated adapter runs it.
    fn git(path: &Path, args: &[&str]) -> Result<String, GitSyncError> {
        run_git(ConfigScope::Isolated, path, args)
    }

    // Create a temporary git repo, returning the tempdir handle and its path.
    // `gix::init` would read the developer's `init.defaultBranch`, so the repo
    // is created with isolated options instead.
    fn init_repo() -> (tempfile::TempDir, PathBuf) {
        let temp = tempfile::TempDir::new().unwrap();
        gix::ThreadSafeRepository::init_opts(
            temp.path(),
            gix::create::Kind::WithWorktree,
            gix::create::Options::default(),
            gix::open::Options::isolated(),
        )
        .unwrap();
        let path = temp.path().to_path_buf();
        (temp, path)
    }

    // Create a temporary git repo with an initial commit (so the index exists).
    fn init_repo_for_commits() -> (tempfile::TempDir, PathBuf) {
        let (temp, path) = init_repo();
        // gix, opened isolated, reads no identity from the environment, so the
        // repository carries one for `commit_tree`.
        configure_identity(&path);
        // Create an initial commit so the index file exists on disk.
        git(&path, &["commit", "--allow-empty", "-m", "initial"]).unwrap();
        (temp, path)
    }

    // ─── parse_diff_line tests ──────────────────────────────────────────────

    #[test]
    fn parse_diff_line_added() {
        let result = parse_diff_line("A\tnew_file.yml").unwrap();
        assert_eq!(result.path, PathBuf::from("new_file.yml"));
        assert_eq!(result.change_type, ChangeType::Added);
    }

    #[test]
    fn parse_diff_line_modified() {
        let result = parse_diff_line("M\texisting.yml").unwrap();
        assert_eq!(result.change_type, ChangeType::Modified);
    }

    #[test]
    fn parse_diff_line_deleted() {
        let result = parse_diff_line("D\tremoved.yml").unwrap();
        assert_eq!(result.change_type, ChangeType::Deleted);
    }

    #[test]
    fn parse_diff_line_rename_uses_new_path() {
        // Rename lines have three fields: R100\told\tnew
        let result = parse_diff_line("R100\told_name.yml\tnew_name.yml").unwrap();
        assert_eq!(result.change_type, ChangeType::Modified);
        assert_eq!(result.path, PathBuf::from("new_name.yml"));
    }

    #[test]
    fn parse_diff_line_rename_single_field_uses_that_path() {
        // Two-field rename (shouldn't happen in practice, but test graceful handling)
        let result = parse_diff_line("R100\trenamed.yml").unwrap();
        assert_eq!(result.path, PathBuf::from("renamed.yml"));
    }

    #[test]
    fn parse_diff_line_empty_returns_none() {
        assert!(parse_diff_line("").is_none());
    }

    #[test]
    fn parse_diff_line_no_tab_returns_none() {
        assert!(parse_diff_line("M").is_none());
    }

    // ─── GitStatusProvider tests ─────────────────────────────────────────────

    #[test]
    fn non_git_directory_returns_not_in_repo() {
        let temp = tempfile::TempDir::new().unwrap();
        let result = isolated().status_for_directory(temp.path()).unwrap();
        assert!(!result.in_repo);
        assert!(result.files.is_empty());
    }

    #[test]
    fn git_repo_detected_as_in_repo() {
        let (_temp, path) = init_repo();
        let result = isolated().status_for_directory(&path).unwrap();
        assert!(result.in_repo);
    }

    #[test]
    fn untracked_file_detected() {
        let (_temp, path) = init_repo();
        let file_path = path.join("untracked.yml");
        fs::write(&file_path, "name: untracked\n").unwrap();

        let result = isolated().status_for_directory(&path).unwrap();

        assert!(result.in_repo);
        assert_eq!(result.status_for_file(&file_path), GitFileStatus::Untracked);
    }

    // ─── GitSyncProvider tests ───────────────────────────────────────────────

    #[test]
    fn discover_repo_not_found() {
        let temp = tempfile::TempDir::new().unwrap();
        assert!(isolated().discover_repo(temp.path()).is_err());
    }

    #[test]
    fn discover_repo_finds_root() {
        let (_temp, path) = init_repo();
        let info = isolated().discover_repo(&path).unwrap();

        let expected = dunce::canonicalize(&path).unwrap();
        assert_eq!(info.root, expected);
    }

    #[test]
    fn repo_status_clean() {
        let (_temp, path) = init_repo();
        let status = isolated().repo_status(&path).unwrap();
        assert!(status.is_clean());
    }

    #[test]
    fn repo_status_detects_untracked() {
        let (_temp, path) = init_repo();
        fs::write(path.join("new.yml"), "name: new\n").unwrap();

        let status = isolated().repo_status(&path).unwrap();

        assert!(status.is_dirty());
        assert_eq!(status.untracked.len(), 1);
    }

    #[test]
    fn stage_files_works() {
        let (_temp, path) = init_repo_for_commits();
        fs::write(path.join("test.yml"), "name: test\n").unwrap();

        isolated()
            .stage_files(&path, &[PathBuf::from("test.yml")])
            .unwrap();

        let status = isolated().repo_status(&path).unwrap();
        assert!(!status.staged.is_empty() || status.untracked.is_empty());
    }

    #[test]
    fn commit_creates_commit() {
        let (_temp, path) = init_repo_for_commits();
        fs::write(path.join("test.yml"), "name: test\n").unwrap();

        isolated()
            .stage_files(&path, &[PathBuf::from("test.yml")])
            .unwrap();

        let result = commit_tree(ConfigScope::Isolated, &path, "test commit");
        assert!(result.is_ok(), "commit should succeed: {result:?}");
    }

    // A tree that deletes every tracked file was recorded on a nested path, so
    // committing is refused, and nothing is written, until that is fixed.
    #[test]
    fn commit_is_refused_and_writes_nothing() {
        let (_temp, path) = init_repo_for_commits();
        fs::create_dir_all(path.join("packages/foo")).unwrap();
        fs::write(path.join("packages/foo.yml"), "name: foo\n").unwrap();
        fs::write(path.join("packages/foo/rc"), "a\n").unwrap();
        git(&path, &["add", "-A"]).unwrap();
        git(&path, &["commit", "-m", "init"]).unwrap();
        let before = git(&path, &["rev-parse", "HEAD"]).unwrap();
        fs::write(path.join("packages/foo/rc"), "b\n").unwrap();
        isolated()
            .stage_files(&path, &[PathBuf::from("packages/foo/rc")])
            .unwrap();

        let result = isolated().commit(&path, "chore(foo): update dotfile");

        assert!(
            matches!(result, Err(GitSyncError::CommitDisabled)),
            "commit must be refused: {result:?}"
        );
        assert_eq!(git(&path, &["rev-parse", "HEAD"]).unwrap(), before);
    }

    #[test]
    fn diff_commits_detects_changes() {
        let (_temp, path) = init_repo_for_commits();

        // First commit
        fs::write(path.join("a.yml"), "name: a\n").unwrap();
        git(&path, &["add", "a.yml"]).unwrap();
        git(&path, &["commit", "-m", "first"]).unwrap();
        let first = git(&path, &["rev-parse", "HEAD"]).unwrap();

        // Second commit
        fs::write(path.join("b.yml"), "name: b\n").unwrap();
        git(&path, &["add", "b.yml"]).unwrap();
        git(&path, &["commit", "-m", "second"]).unwrap();
        let second = git(&path, &["rev-parse", "HEAD"]).unwrap();

        let changes = isolated()
            .diff_commits(&path, &CommitId(first), &CommitId(second))
            .unwrap();

        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].path, PathBuf::from("b.yml"));
        assert_eq!(changes[0].change_type, ChangeType::Added);
    }

    // ─── Credential redaction, end to end through the real `git` ─────────────
    //
    // These are the only tests that prove redaction fires on output a real
    // `git` actually produced. Everything else in this change exercises
    // `GitMessage` on strings a test wrote.

    // High-entropy and shaped like nothing else in a fixture, so it cannot
    // collide with a path, a package name or an environment name. A fixture
    // value, never a real credential: `assert_secret_free` prints an excerpt of
    // whatever matched.
    const FIXTURE_TOKEN: &str = "Zk9qP2mW7xR4tL6vB1nH3jD5";

    // A loopback server answering every request `401 Basic`, returning its port.
    //
    // A 401 is what puts git on the path that leaks: it has a username from the
    // URL, needs a password, cannot prompt, and names the URL in the failure.
    // Against a refused port git fails earlier and strips the userinfo, so a
    // listener is required.
    //
    // Loops on `accept` because git makes more than one connection. The thread is
    // deliberately not joined -- it blocks in `accept` until the binary exits.
    fn spawn_401_server() -> u16 {
        use std::io::{Read, Write};

        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let port = listener.local_addr().expect("local addr").port();

        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                // Drain enough of the request that the client can finish sending;
                // only the response matters.
                let mut buf = [0u8; 1024];
                let _ = stream.read(&mut buf);
                let _ = stream.write_all(
                    b"HTTP/1.1 401 Unauthorized\r\n\
                      WWW-Authenticate: Basic realm=\"git\"\r\n\
                      Content-Length: 0\r\n\
                      Connection: close\r\n\r\n",
                );
            }
        });

        port
    }

    // A repo whose `origin` carries [`FIXTURE_TOKEN`] as the URL's username —
    // the shape `gh auth setup-git` writes, and the one that leaks.
    fn repo_with_a_credential_bearing_remote(port: u16) -> (tempfile::TempDir, PathBuf) {
        let (temp, path) = init_repo_for_commits();
        let url = format!("http://{FIXTURE_TOKEN}@127.0.0.1:{port}/o/r.git");

        git(&path, &["remote", "add", "origin", &url]).unwrap();
        // Repo-local, so an ambient credential helper cannot answer the prompt
        // and dissolve the premise…
        git(&path, &["config", "credential.helper", ""]).unwrap();
        // …and so an ambient proxy cannot turn this loopback request into real
        // network egress, which in a security test is the worse failure.
        git(&path, &["config", "http.proxy", ""]).unwrap();
        // A libcurl that strips credentials from CURLINFO_EFFECTIVE_URL makes
        // git treat every response as a redirect and rebuild its credential
        // from the stripped URL, losing the username. Configured here as well,
        // git applies it to the rebuilt credential and still names the token.
        // On an unpatched libcurl the URL's own userinfo is the token too, so
        // both routes give the same text.
        git(&path, &["config", "credential.username", FIXTURE_TOKEN]).unwrap();

        // `git push` refuses before contacting the remote without an upstream,
        // so set one without needing a fetch to succeed first.
        let branch = git(&path, &["rev-parse", "--abbrev-ref", "HEAD"]).unwrap();
        git(
            &path,
            &["config", &format!("branch.{branch}.remote"), "origin"],
        )
        .unwrap();
        git(
            &path,
            &[
                "config",
                &format!("branch.{branch}.merge"),
                &format!("refs/heads/{branch}"),
            ],
        )
        .unwrap();

        (temp, path)
    }

    // The premise: on *this* machine, with the environment `run_git` gives git,
    // the raw stderr really does contain the token.
    //
    // Without this the leak tests below pass whenever git stops leaking -- or,
    // far more likely, whenever something answers the credential prompt so git
    // never names the URL. A leak test that passes because the secret was never
    // produced is worse than no test.
    //
    // It runs the command `run_git` runs, built by the same `git_command`.
    fn assert_git_really_leaks_the_token(repo_root: &Path, args: &[&str]) {
        let output = git_command(ConfigScope::Isolated, repo_root, args)
            .output()
            .expect("spawn git");
        let stderr = String::from_utf8_lossy(&output.stderr);

        assert!(
            stderr.contains(FIXTURE_TOKEN),
            "PREMISE GONE: raw `git {}` stderr no longer contains the URL's \
             username, so the redaction assertions below would pass without \
             proving anything.\n\nBefore concluding that git changed, check \
             this machine for something that answered the credential prompt or \
             rewrote the request: GIT_ASKPASS, SSH_ASKPASS, core.askPass, a \
             global or system credential.helper, url.*.insteadOf, or \
             http_proxy/https_proxy. A libcurl that strips credentials from \
             the effective URL drops the URL's username, which \
             credential.username in the fixture is there to restore.\n\ngit said: {stderr}",
            args.join(" ")
        );
    }

    #[test]
    fn a_failed_fetch_keeps_the_remote_url_credential_out_of_the_error() {
        let port = spawn_401_server();
        let (_temp, path) = repo_with_a_credential_bearing_remote(port);

        assert_git_really_leaks_the_token(&path, &["fetch"]);

        let error = isolated()
            .fetch(&path)
            .expect_err("a 401 with no password must fail");

        assert_secret_free(&error.to_string(), FIXTURE_TOKEN, "a GitSyncError Display");
        assert_secret_free(&format!("{error:?}"), FIXTURE_TOKEN, "a GitSyncError Debug");
        // The other direction: redaction must not have blanked the diagnosis.
        // Left a plain `contains` deliberately — `assert_secret_free` is its
        // negation, and converting one inverts the test silently.
        assert!(
            error.to_string().contains("127.0.0.1"),
            "the error must stay diagnosable, got: {error}"
        );
    }

    #[test]
    fn a_failed_push_keeps_the_remote_url_credential_out_of_the_error() {
        let port = spawn_401_server();
        let (_temp, path) = repo_with_a_credential_bearing_remote(port);

        assert_git_really_leaks_the_token(&path, &PUSH_ARGS);

        let error = isolated()
            .push(&path)
            .expect_err("a 401 with no password must fail");

        assert_secret_free(&error.to_string(), FIXTURE_TOKEN, "a GitSyncError Display");
        assert_secret_free(&format!("{error:?}"), FIXTURE_TOKEN, "a GitSyncError Debug");
        assert!(
            error.to_string().contains("127.0.0.1"),
            "the error must stay diagnosable, got: {error}"
        );
    }

    // ─── message locale and result classification ───────────────────────────

    #[test]
    fn push_was_rejected_reads_the_status_line_of_the_current_branch() {
        let main = "refs/heads/main";
        let line = |refs: &str, status: &str| format!("!\t{refs}\t{status}\nDone\n");
        let on_main = |status: &str| line("refs/heads/main:refs/heads/main", status);

        assert!(push_was_rejected(
            &on_main("[rejected] (fetch first)"),
            main
        ));
        assert!(push_was_rejected(
            &on_main("[rejected] (non-fast-forward)"),
            main
        ));
        // A `HEAD` refspec prints `HEAD` as the local side.
        assert!(push_was_rejected(
            &line("HEAD:refs/heads/main", "[rejected] (fetch first)"),
            main
        ));
        assert!(!push_was_rejected(
            &on_main("[remote rejected] (pre-receive hook declined)"),
            main
        ));
        assert!(!push_was_rejected(
            &on_main("[remote failure] (timeout)"),
            main
        ));
        assert!(!push_was_rejected(
            &on_main("[rejected] (already exists)"),
            main
        ));
        // A tag or another branch rejected beside the current one is not the
        // current branch being behind.
        assert!(!push_was_rejected(
            &line("refs/tags/v1:refs/tags/v1", "[rejected] (already exists)"),
            main
        ));
        assert!(!push_was_rejected(
            &line(
                "refs/heads/other:refs/heads/other",
                "[rejected] (fetch first)"
            ),
            main
        ));
        assert!(!push_was_rejected(
            "=\trefs/heads/main:refs/heads/main\t[up to date]\nDone\n",
            main
        ));
        assert!(!push_was_rejected("", main));
    }

    #[test]
    fn a_failure_with_no_stderr_starts_at_its_status_lines() {
        use std::os::unix::process::ExitStatusExt;
        let output = Output {
            status: std::process::ExitStatus::from_raw(1 << 8),
            stdout: Vec::new(),
            stderr: Vec::new(),
        };

        let error = failure(&PUSH_ARGS, &output, "!\tx:y\t[rejected] (z)");

        assert!(
            matches!(
                &error,
                GitSyncError::OperationFailed { message, .. }
                    if message.as_str() == "!\tx:y\t[rejected] (z)"
            ),
            "got: {error:?}"
        );
    }

    #[test]
    fn failed_ref_lines_keeps_only_the_failed_refs() {
        let stdout = "To https://user:tok@host/r\n!\trefs/heads/main:refs/heads/main\t[remote rejected] (hook)\n \trefs/heads/x:refs/heads/x\ta..b\nDone\n";

        assert_eq!(
            failed_ref_lines(stdout),
            "!\trefs/heads/main:refs/heads/main\t[remote rejected] (hook)"
        );
    }

    // A bare remote and a clone of it with one pushed commit, so `@{u}` exists.
    fn clone_with_upstream() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let temp = tempfile::TempDir::new().unwrap();
        let remote = temp.path().join("remote.git");
        let local = temp.path().join("local");
        fs::create_dir(&remote).unwrap();
        git(&remote, &["init", "--bare", "--initial-branch=main"]).unwrap();
        fs::create_dir(&local).unwrap();
        git(&local, &["init", "--initial-branch=main"]).unwrap();
        configure_identity(&local);
        git(
            &local,
            &["remote", "add", "origin", remote.to_str().unwrap()],
        )
        .unwrap();
        git(&local, &["commit", "--allow-empty", "-m", "one"]).unwrap();
        git(&local, &["push", "-u", "origin", "main"]).unwrap();
        (temp, remote, local)
    }

    fn configure_identity(path: &Path) {
        git(
            path,
            &["config", "user.email", "selfie-test@example.invalid"],
        )
        .unwrap();
        git(path, &["config", "user.name", "selfie-test"]).unwrap();
        git(path, &["config", "commit.gpgsign", "false"]).unwrap();
    }

    // A second clone of the remote, made once.
    fn other_clone(temp: &Path, remote: &Path) -> PathBuf {
        let other = temp.join("other");
        if !other.exists() {
            git(
                temp,
                &["clone", remote.to_str().unwrap(), other.to_str().unwrap()],
            )
            .unwrap();
            configure_identity(&other);
        }
        other
    }

    // The second clone adds a commit to the remote.
    fn advance_remote(temp: &Path, remote: &Path) {
        let other = other_clone(temp, remote);
        git(&other, &["commit", "--allow-empty", "-m", "remote work"]).unwrap();
        git(&other, &["push"]).unwrap();
    }

    #[test]
    fn fast_forward_reports_up_to_date_from_an_unmoved_head() {
        let (_temp, _remote, local) = clone_with_upstream();

        let result = isolated().fast_forward(&local).unwrap();
        assert!(
            matches!(result, FastForwardResult::AlreadyUpToDate),
            "got {result:?}"
        );
    }

    #[test]
    fn fast_forward_reports_up_to_date_when_only_ahead() {
        let (_temp, _remote, local) = clone_with_upstream();
        git(&local, &["commit", "--allow-empty", "-m", "local"]).unwrap();

        let result = isolated().fast_forward(&local).unwrap();
        assert!(
            matches!(result, FastForwardResult::AlreadyUpToDate),
            "got {result:?}"
        );
    }

    #[test]
    fn fast_forward_advances_over_new_remote_commits() {
        let (temp, remote, local) = clone_with_upstream();
        advance_remote(temp.path(), &remote);
        git(&local, &["fetch"]).unwrap();

        let result = isolated().fast_forward(&local).unwrap();

        assert!(
            matches!(
                result,
                FastForwardResult::Advanced {
                    commit_count: 1,
                    ..
                }
            ),
            "got {result:?}"
        );
    }

    #[test]
    fn fast_forward_that_fails_without_divergence_is_an_error() {
        let (temp, remote, local) = clone_with_upstream();
        // The remote adds a file that an untracked local file is in the way of,
        // so the fast-forward is refused although nothing has diverged.
        let other = other_clone(temp.path(), &remote);
        fs::write(other.join("f"), "remote").unwrap();
        git(&other, &["add", "f"]).unwrap();
        git(&other, &["commit", "-m", "add f"]).unwrap();
        git(&other, &["push"]).unwrap();
        git(&local, &["fetch"]).unwrap();
        fs::write(local.join("f"), "local").unwrap();

        let result = isolated().fast_forward(&local);

        assert!(
            matches!(result, Err(GitSyncError::OperationFailed { .. })),
            "got {result:?}"
        );
    }

    #[test]
    fn fast_forward_without_an_upstream_is_an_error_not_up_to_date() {
        let (_temp, _remote, local) = clone_with_upstream();
        git(&local, &["branch", "--unset-upstream"]).unwrap();

        let result = isolated().fast_forward(&local);

        assert!(
            matches!(result, Err(GitSyncError::OperationFailed { .. })),
            "got {result:?}"
        );
    }

    #[test]
    fn fast_forward_reports_a_diverged_branch() {
        let (temp, remote, local) = clone_with_upstream();
        advance_remote(temp.path(), &remote);
        git(&local, &["commit", "--allow-empty", "-m", "local"]).unwrap();
        git(&local, &["fetch"]).unwrap();

        let result = isolated().fast_forward(&local).unwrap();
        assert!(
            matches!(result, FastForwardResult::Diverged),
            "got {result:?}"
        );
    }

    #[test]
    fn a_push_the_remote_is_ahead_of_is_rejected_with_the_pull_advice() {
        let (temp, remote, local) = clone_with_upstream();
        advance_remote(temp.path(), &remote);
        git(&local, &["commit", "--allow-empty", "-m", "local"]).unwrap();

        let error = isolated().push(&local).expect_err("push must be rejected");

        assert!(matches!(error, GitSyncError::PushRejected), "got: {error}");
    }

    #[test]
    fn a_push_by_a_head_refspec_that_the_remote_is_ahead_of_is_rejected() {
        let (temp, remote, local) = clone_with_upstream();
        advance_remote(temp.path(), &remote);
        git(&local, &["commit", "--allow-empty", "-m", "local"]).unwrap();
        git(
            &local,
            &["config", "remote.origin.push", "HEAD:refs/heads/main"],
        )
        .unwrap();

        let error = isolated().push(&local).expect_err("push must be rejected");

        assert!(matches!(error, GitSyncError::PushRejected), "got: {error}");
    }

    #[test]
    fn a_push_the_remote_refuses_gives_no_pull_advice() {
        let (_temp, remote, local) = clone_with_upstream();
        // The isolated template directory is empty, so the bare remote has no
        // hooks directory yet.
        fs::create_dir_all(remote.join("hooks")).unwrap();
        let hook = remote.join("hooks/pre-receive");
        fs::write(&hook, "#!/bin/sh\nexit 1\n").unwrap();
        fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).unwrap();
        git(&local, &["commit", "--allow-empty", "-m", "local"]).unwrap();

        let error = isolated().push(&local).expect_err("the hook must refuse");

        assert!(
            matches!(error, GitSyncError::OperationFailed { .. }),
            "got: {error}"
        );
        assert!(!error.to_string().contains("sync pull"), "got: {error}");
    }

    #[test]
    fn a_rejected_tag_beside_a_pushed_branch_gives_no_pull_advice() {
        let (_temp, remote, local) = clone_with_upstream();
        // The remote already has `v1` at the first commit; the local `v1` is on a
        // newer one, and the push refspecs send both it and the branch.
        git(&remote, &["tag", "v1"]).unwrap();
        git(&local, &["commit", "--allow-empty", "-m", "local"]).unwrap();
        git(&local, &["tag", "v1"]).unwrap();
        for refspec in ["refs/heads/main", "refs/tags/v1"] {
            git(
                &local,
                &[
                    "config",
                    "--add",
                    "remote.origin.push",
                    &format!("{refspec}:{refspec}"),
                ],
            )
            .unwrap();
        }

        let error = isolated()
            .push(&local)
            .expect_err("the tag must be rejected");

        assert!(
            matches!(error, GitSyncError::OperationFailed { .. }),
            "got: {error}"
        );
        assert!(!error.to_string().contains("sync pull"), "got: {error}");
        assert!(
            error.to_string().contains("refs/tags/v1:refs/tags/v1"),
            "the failed ref must be named, got: {error}"
        );
    }

    #[test]
    fn a_refused_push_names_the_ref_and_the_reason() {
        let (_temp, remote, local) = clone_with_upstream();
        fs::create_dir_all(remote.join("hooks")).unwrap();
        let hook = remote.join("hooks/pre-receive");
        fs::write(&hook, "#!/bin/sh\nexit 1\n").unwrap();
        fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).unwrap();
        git(&local, &["commit", "--allow-empty", "-m", "local"]).unwrap();

        let error = isolated().push(&local).expect_err("the hook must refuse");

        assert!(
            error
                .to_string()
                .contains("[remote rejected] (pre-receive hook declined)"),
            "got: {error}"
        );
    }

    #[test]
    fn a_push_accepted_by_the_remote_succeeds() {
        let (_temp, _remote, local) = clone_with_upstream();
        git(&local, &["commit", "--allow-empty", "-m", "local"]).unwrap();

        isolated().push(&local).unwrap();
    }
}
