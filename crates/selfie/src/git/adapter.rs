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
    ChangeType, ChangedFile, CommitId, FastForwardResult, GitSyncError, GitSyncProvider,
    InProgressOperation, RepoInfo, RepoStatus,
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
    discover(scope, path).map_err(|e| {
        if is_no_repository(&e, path) {
            GitSyncError::NotARepo {
                path: path.to_path_buf(),
            }
        } else {
            GitSyncError::OperationFailed {
                operation: "discover repository",
                message: GitMessage::new(e),
            }
        }
    })
}

// Whether a failed discovery means `path` is not in a repository selfie may
// open, rather than that a repository's own files could not be read. Three
// cases count: no repository above `path`; one gix found and does not trust,
// which is reported as no repository; and a `path` that is missing or is not a
// directory. gix raises the first two as its own error type, and the third only
// as an untyped message, so that one is asked of the path itself.
fn is_no_repository(error: &gix::Error, path: &Path) -> bool {
    error
        .downcast_any_ref::<gix::discover::upwards::Error>()
        .is_some()
        || !path.is_dir()
}

// `gix::discover`, reading only the repository's own configuration when
// `scope` is isolated.
fn discover(scope: ConfigScope, path: &Path) -> Result<gix::Repository, gix::Error> {
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

/// Shorthand for creating a [`GitSyncError::OperationFailed`] from any error.
fn git_sync_err(operation: &'static str, e: impl std::fmt::Display) -> GitSyncError {
    GitSyncError::OperationFailed {
        operation,
        message: GitMessage::new(e),
    }
}

/// Run a `git` CLI command in the given directory. A failure names it by
/// `operation`, such as `git fetch`.
///
/// Stdin is set to `Stdio::null` and `GIT_TERMINAL_PROMPT=0` to prevent git
/// from blocking on credential prompts, which is critical for non-interactive
/// callers like the MCP server.
// The label is a literal rather than `args` joined, since an argument can come
// from git's own output, such as a commit ID.
fn run_git(
    scope: ConfigScope,
    repo_root: &Path,
    operation: &'static str,
    args: &[&str],
) -> Result<String, GitSyncError> {
    let output = git_output(scope, repo_root, operation, args)?;

    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    } else {
        Err(failure(operation, &output, ""))
    }
}

// Run `git` and hand back its output whatever its exit status, for a caller
// that reads stdout of a failed run. Fails only when git cannot be started.
fn git_output(
    scope: ConfigScope,
    repo_root: &Path,
    operation: &'static str,
    args: &[&str],
) -> Result<Output, GitSyncError> {
    git_command(scope, repo_root, args)
        .output()
        .map_err(|e| GitSyncError::OperationFailed {
            operation,
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
fn failure(operation: &'static str, output: &Output, status_lines: &str) -> GitSyncError {
    let mut text = output.stderr.trim_ascii().to_vec();
    if !status_lines.is_empty() {
        if !text.is_empty() {
            text.push(b'\n');
        }
        text.extend_from_slice(status_lines.as_bytes());
    }
    GitSyncError::OperationFailed {
        operation,
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
pub(crate) const PUSH: GitRun = GitRun::new("git push --porcelain", &["push", "--porcelain"]);

/// A git command selfie runs with fixed arguments, and the words a failure
/// names it by, kept together so the two cannot drift apart.
#[derive(Clone, Copy)]
pub(crate) struct GitRun {
    pub(crate) label: &'static str,
    pub(crate) args: &'static [&'static str],
}

impl GitRun {
    const fn new(label: &'static str, args: &'static [&'static str]) -> Self {
        Self { label, args }
    }
}

const REV_PARSE_BRANCH: GitRun = GitRun::new(
    "git rev-parse --abbrev-ref HEAD",
    &["rev-parse", "--abbrev-ref", "HEAD"],
);
const REMOTE: GitRun = GitRun::new("git remote", &["remote"]);
const LS_FILES_DELETED: GitRun = GitRun::new("git ls-files --deleted", &["ls-files", "--deleted"]);
const SYMBOLIC_REF: GitRun = GitRun::new(
    "git symbolic-ref --quiet HEAD",
    &["symbolic-ref", "--quiet", "HEAD"],
);
pub(crate) const FETCH: GitRun = GitRun::new("git fetch", &["fetch"]);
// The paths follow a `--`. Literal pathspecs keep a name such as `*.yml` from
// matching other files, and `--all` records a named path that is gone as
// removed.
const ADD: GitRun = GitRun::new("git add", &["--literal-pathspecs", "add", "--all"]);
const REV_PARSE_HEAD: GitRun = GitRun::new("git rev-parse HEAD", &["rev-parse", "HEAD"]);
const MERGE_FF: GitRun = GitRun::new("git merge --ff-only @{u}", &["merge", "--ff-only", "@{u}"]);
const AHEAD_BEHIND: GitRun = GitRun::new(
    "git rev-list --left-right --count HEAD...@{u}",
    &["rev-list", "--left-right", "--count", "HEAD...@{u}"],
);

// The two commands whose arguments come from git's own output, commit IDs, so
// only their labels are fixed.
const REV_LIST_COUNT: &str = "git rev-list --count";
pub(crate) const DIFF_NAME_STATUS: &str = "git diff --name-status";

// Whether `git push --porcelain` refused to send `current_ref`, the branch being
// pushed, because the remote has commits it lacks. The current branch is named
// by its full ref, or as `HEAD` when a `HEAD` or `HEAD:<ref>` refspec pushed it.
// Each ref's line is `<flag>\t<from>:<to>\t<summary> (<reason>)`, and the refusal
// is a `!` flag with `[rejected] (fetch first)` or `[rejected] (non-fast-forward)`
// on the current branch's own line. git's transport.c prints these strings
// untranslated, so they hold under any locale.
fn push_was_rejected(porcelain_stdout: &str, current_ref: &str) -> bool {
    // Other lines do not count: `[remote rejected]` (a server hook or a protected
    // branch) and `[remote failure]` are not fixed by pulling, nor is a tag or
    // another branch rejected beside the current one.
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
    // The subset of `index_mods` the index no longer has, as `git rm` leaves.
    index_deletions: HashSet<String>,
    untracked: Vec<String>,
}

/// Iterate gix status items and classify them into worktree modifications,
/// index (staged) modifications, and untracked files.
fn collect_raw_status(repo: &gix::Repository) -> Result<RawStatus, String> {
    // File by file, not gix's default of one entry per new directory: a
    // commit records index paths, and a directory is never one. Renames are
    // not tracked for the same reason: a staged rename is the removal of one
    // path and the addition of another, and its new path alone leaves the old
    // one in the tree.
    let platform = repo
        .status(gix::progress::Discard)
        .map_err(|e| e.to_string())?
        .untracked_files(gix::status::UntrackedFiles::Files)
        .tree_index_track_renames(gix::status::tree_index::TrackRenames::Disabled);

    let iter = platform
        .into_iter(std::iter::empty::<gix::bstr::BString>())
        .map_err(|e| e.to_string())?;

    let mut worktree_mods = HashSet::new();
    let mut index_mods = HashSet::new();
    let mut index_deletions = HashSet::new();
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
                if matches!(change, gix::diff::index::Change::Deletion { .. }) {
                    index_deletions.insert(location.clone());
                }
                index_mods.insert(location);
            }
        }
    }

    Ok(RawStatus {
        worktree_mods,
        index_mods,
        index_deletions,
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

// The author or committer git configuration names, owned.
fn identity<E: std::fmt::Display>(
    configured: Option<Result<gix::actor::SignatureRef<'_>, E>>,
    role: &str,
) -> Result<gix::actor::Signature, GitSyncError> {
    configured
        .ok_or_else(|| git_sync_err("commit", format!("no {role} identity is configured")))?
        .map(Into::into)
        .map_err(|e| git_sync_err("commit", e))
}

// Refuses what neither staging nor committing may write over: an operation
// git has in progress, which owns the index and its own state files, and a
// path with conflict stages, which has no single entry to record. Asked by both
// before anything is written, so a refusal leaves the repository as it was.
fn refuse_unsafe_state(
    repo: &gix::Repository,
    index: &gix::index::File,
    files: &[PathBuf],
) -> Result<(), GitSyncError> {
    if let Some(state) = repo.state() {
        return Err(GitSyncError::OperationInProgress {
            operation: in_progress_operation(state),
        });
    }
    for file in files {
        let path = gix::path::into_bstr(file).map_err(|e| git_sync_err("read path", e))?;
        // Every stage of the path: a conflict deleted on one side has no
        // stage 2, so probing for "ours" alone would miss it.
        let conflicted = index.entry_range(path.as_ref()).is_some_and(|range| {
            index.entries()[range]
                .iter()
                .any(|entry| entry.stage() != gix::index::entry::Stage::Unconflicted)
        });
        if conflicted {
            return Err(GitSyncError::Conflicted { path: file.clone() });
        }
    }
    Ok(())
}

// Whether `file` has no index entry, no entries below it and nothing on disk,
// as `git rm` leaves a path, so that `git add` would find nothing to match.
fn is_gone(index: &gix::index::File, repo_root: &Path, file: &Path) -> Result<bool, GitSyncError> {
    let path = gix::path::into_bstr(file).map_err(|e| git_sync_err("stage", e))?;
    let mut below = path.clone().into_owned();
    below.push(b'/');
    if index.entry_range(path.as_ref()).is_some()
        || index.prefixed_entries_range(below.as_ref()).is_some()
    {
        return Ok(false);
    }
    // Only an answer that the path is absent counts. Any other failure to stat
    // it leaves it to git, which reports what is wrong.
    Ok(
        std::fs::symlink_metadata(repo_root.join(file)).is_err_and(|e| {
            matches!(
                e.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
            )
        }),
    )
}

// gix's name for the operation, in the port's terms.
fn in_progress_operation(state: gix::state::InProgress) -> InProgressOperation {
    use gix::state::InProgress;
    match state {
        InProgress::Merge => InProgressOperation::Merge,
        InProgress::Rebase | InProgress::RebaseInteractive => InProgressOperation::Rebase,
        InProgress::CherryPick | InProgress::CherryPickSequence => InProgressOperation::CherryPick,
        InProgress::Revert | InProgress::RevertSequence => InProgressOperation::Revert,
        InProgress::Bisect => InProgressOperation::Bisect,
        InProgress::ApplyMailbox | InProgress::ApplyMailboxRebase => {
            InProgressOperation::ApplyMailbox
        }
    }
}

// ─── GitStatusProvider (read-only, existing) ─────────────────────────────────

impl GitStatusProvider for GixGitAdapter {
    fn status_for_directory(&self, directory: &Path) -> Result<GitDirectoryStatus, GitStatusError> {
        let repo = match discover(self.scope, directory) {
            Ok(repo) => repo,
            Err(error) if is_no_repository(&error, directory) => {
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
                operation: "discover repository",
                message: GitMessage::new("bare repository"),
            })?
            .to_path_buf();
        let root = dunce::canonicalize(&root).unwrap_or(root);

        // Get branch name via git CLI (more reliable across gix versions)
        let branch = run_git(
            self.scope,
            &root,
            REV_PARSE_BRANCH.label,
            REV_PARSE_BRANCH.args,
        )
        .ok()
        .filter(|b| b != "HEAD"); // Detached HEAD → None

        // Get remote name via git CLI
        let remote_name = run_git(self.scope, &root, REMOTE.label, REMOTE.args)
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
            operation: "status",
            message: GitMessage::new(msg),
        })?;

        let modified = raw.worktree_mods.iter().map(PathBuf::from).collect();
        // A staged deletion is reported as deleted, not as staged, so sync
        // groups it as a removal rather than read a spec that is gone.
        let staged = raw
            .index_mods
            .difference(&raw.index_deletions)
            .map(PathBuf::from)
            .collect();
        let untracked = raw.untracked.iter().map(PathBuf::from).collect();

        // Detect deleted files via git CLI (gix status doesn't surface deletions cleanly)
        let mut deleted = Vec::new();
        if let Ok(output) = run_git(
            self.scope,
            repo_root,
            LS_FILES_DELETED.label,
            LS_FILES_DELETED.args,
        ) {
            for line in output.lines().filter(|l| !l.is_empty()) {
                deleted.push(PathBuf::from(line));
            }
        }
        deleted.extend(raw.index_deletions.iter().map(PathBuf::from));

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
        let repo = open_repo(self.scope, repo_root)?;
        let index = repo
            .index_or_empty()
            .map_err(|e| git_sync_err("open index", e))?;
        refuse_unsafe_state(&repo, &index, files)?;

        // A gone path has nothing left to stage, and git refuses a pathspec
        // that matches nothing. The commit records its removal all the same,
        // because the index has no entry for it.
        let mut paths = Vec::with_capacity(files.len());
        for file in files {
            if !is_gone(&index, repo_root, file)? {
                paths.push(file);
            }
        }
        // `git add --all` with no paths stages the whole tree.
        if paths.is_empty() {
            return Ok(());
        }

        // git decides each path's blob, mode and entry, applying the
        // repository's attributes and clean filters, which a git-crypt or LFS
        // repository depends on to keep plaintext and large files out of
        // history. Only the named paths are given to it.
        let output = git_command(self.scope, repo_root, ADD.args)
            .arg("--")
            .args(paths)
            .output()
            .map_err(|e| git_sync_err(ADD.label, e))?;
        if !output.status.success() {
            return Err(failure(ADD.label, &output, ""));
        }
        Ok(())
    }

    fn commit(
        &self,
        repo_root: &Path,
        files: &[PathBuf],
        message: &str,
    ) -> Result<CommitId, GitSyncError> {
        let repo = open_repo(self.scope, repo_root)?;
        let index = repo
            .index_or_empty()
            .map_err(|e| git_sync_err("open index", e))?;
        refuse_unsafe_state(&repo, &index, files)?;

        // Only the named paths are edited. Nothing walks HEAD's tree to infer
        // deletions from what the index lacks: a walk reports directories too,
        // a directory is never an index path, and inferring would remove every
        // file below one. A path the index has no entry for is removed because
        // selfie named it, never because it is missing.
        //
        // HEAD is read once, and both the tree to edit and the parent come from
        // that read. Two reads let another writer move the branch in between,
        // and the commit would then undo that writer's change under a parent
        // the reference check accepts.
        let parent = repo
            .head()
            .map_err(|e| git_sync_err("read HEAD", e))?
            .id()
            .map(gix::Id::detach);
        let head_tree_id = match parent {
            Some(id) => repo
                .find_commit(id)
                .and_then(|commit| commit.tree_id())
                .map_err(|e| git_sync_err("head tree", e))?
                .detach(),
            None => gix::ObjectId::empty_tree(repo.object_hash()),
        };
        let mut editor = repo
            .edit_tree(head_tree_id)
            .map_err(|e| git_sync_err("edit tree", e))?;
        // Removals go first, so a file replaced by a directory of the same name
        // is removed before the files below it are added, in whatever order
        // `files` names them. A removal refuses a tree: a named directory has no
        // index entry, and removing it would take every file below it along.
        let mut upserts = Vec::with_capacity(files.len());
        for file in files {
            let path = gix::path::into_bstr(file).map_err(|e| git_sync_err("commit", e))?;
            match index
                .entry_by_path_and_stage(path.as_ref(), gix::index::entry::Stage::Unconflicted)
            {
                Some(entry) => upserts.push((path, entry)),
                None => {
                    editor
                        .remove_leaf(path.as_ref())
                        .map_err(|e| git_sync_err("remove tree entry", e))?;
                }
            }
        }
        for (path, entry) in upserts {
            let kind = entry
                .mode
                .to_tree_entry_mode()
                .map(gix::object::tree::EntryKind::from)
                .ok_or_else(|| git_sync_err("commit", "an index entry has no tree mode"))?;
            editor
                .upsert(path.as_ref(), kind, entry.id)
                .map_err(|e| git_sync_err("upsert tree entry", e))?;
        }
        let tree_id = editor
            .write()
            .map_err(|e| git_sync_err("write tree", e))?
            .detach();
        if tree_id == head_tree_id {
            return Err(GitSyncError::NothingToCommit);
        }

        let mut object = gix::objs::Commit {
            message: message.into(),
            tree: tree_id,
            author: identity(repo.author(), "author")?,
            committer: identity(repo.committer(), "committer")?,
            encoding: None,
            parents: parent.into_iter().collect(),
            extra_headers: Vec::new(),
        };
        // Signed before the object is written, let alone before any reference
        // moves, so a signing failure, such as an SSH agent that is locked,
        // leaves the branch where it was. The options are resolved once, since
        // `gpg.ssh.defaultKeyCommand` runs a program to find the key.
        if let Some(options) = repo
            .commit_signing_options_if_enabled()
            .map_err(|e| git_sync_err("read signing configuration", e))?
        {
            object = object
                .sign(options)
                .map_err(|e| git_sync_err("sign commit", e))?;
        }
        let commit_id = repo
            .write_object(&object)
            .map_err(|e| git_sync_err("write commit", e))?
            .detach();

        // As gix's own `commit` does: the branch moves only if it still points
        // at the parent the tree was built on.
        let expected = match parent {
            Some(parent) => gix::refs::transaction::PreviousValue::MustExistAndMatch(
                gix::refs::Target::Object(parent),
            ),
            None => gix::refs::transaction::PreviousValue::MustNotExist,
        };
        let head: gix::refs::FullName = "HEAD"
            .try_into()
            .map_err(|e| git_sync_err("update HEAD", e))?;
        let reflog = gix::reference::log::message(
            "commit",
            message.as_bytes().into(),
            usize::from(parent.is_some()),
        );
        repo.edit_reference(
            gix::refs::transaction::RefEdit::update(head, commit_id, expected, reflog)
                .with_deref(true),
        )
        .map_err(|e| git_sync_err("update HEAD", e))?;

        Ok(CommitId(commit_id.to_string()))
    }

    fn push(&self, repo_root: &Path) -> Result<(), GitSyncError> {
        // Shells out because gix's transport layer (blocking-network-client)
        // doesn't automatically integrate with the user's SSH agent and
        // credential helpers the way the git CLI does.
        //
        // The classification reads the porcelain status lines rather than
        // git's messages, so it holds under any locale and leaves git's own
        // error text, which is relayed to the user, in their language.
        let output = git_output(self.scope, repo_root, PUSH.label, PUSH.args)?;
        if !output.status.success() {
            let stdout = String::from_utf8_lossy(&output.stdout);
            // A detached HEAD has no branch to pull into, so it has no current
            // ref and keeps the generic failure.
            let current_ref =
                run_git(self.scope, repo_root, SYMBOLIC_REF.label, SYMBOLIC_REF.args).ok();
            return Err(
                if current_ref.is_some_and(|current| push_was_rejected(&stdout, &current)) {
                    GitSyncError::PushRejected
                } else {
                    failure(PUSH.label, &output, &failed_ref_lines(&stdout))
                },
            );
        }
        Ok(())
    }

    fn fetch(&self, repo_root: &Path) -> Result<(), GitSyncError> {
        // Shells out for the same reason as push — SSH/credential integration.
        run_git(self.scope, repo_root, FETCH.label, FETCH.args)?;
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

        let old_head = run_git(
            self.scope,
            repo_root,
            REV_PARSE_HEAD.label,
            REV_PARSE_HEAD.args,
        )?;
        run_git(self.scope, repo_root, MERGE_FF.label, MERGE_FF.args)?;
        let new_head = run_git(
            self.scope,
            repo_root,
            REV_PARSE_HEAD.label,
            REV_PARSE_HEAD.args,
        )?;

        let commit_count = run_git(
            self.scope,
            repo_root,
            REV_LIST_COUNT,
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
            DIFF_NAME_STATUS,
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
        let counts = run_git(self.scope, repo_root, AHEAD_BEHIND.label, AHEAD_BEHIND.args)?;
        let mut parts = counts.split_whitespace().map(str::parse::<usize>);
        match (parts.next(), parts.next()) {
            (Some(Ok(ahead)), Some(Ok(behind))) => Ok((ahead, behind)),
            _ => Err(git_sync_err(
                AHEAD_BEHIND.label,
                format!("unexpected output {counts:?}"),
            )),
        }
    }
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
        run_git(ConfigScope::Isolated, path, "git", args)
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

    // Create a temporary git repo with an empty initial commit, so a commit
    // made in it has a parent.
    fn init_repo_for_commits() -> (tempfile::TempDir, PathBuf) {
        let (temp, path) = init_repo();
        // gix, opened isolated, reads no identity from the environment, so the
        // repository carries one for the adapter's commits.
        configure_identity(&path);
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

    // Discovery outside any repository is answered as "not a repository", not
    // as a failed operation.
    #[test]
    fn discovering_outside_a_repository_is_not_a_repo() {
        let temp = tempfile::TempDir::new().unwrap();
        match isolated().discover_repo(temp.path()) {
            Err(GitSyncError::NotARepo { path }) => assert_eq!(path, temp.path()),
            other => panic!("expected NotARepo, got {other:?}"),
        }
    }

    // A path that is missing, or is a file, is not in a repository either.
    #[test]
    fn discovering_from_a_missing_path_or_a_file_is_not_a_repo() {
        let temp = tempfile::TempDir::new().unwrap();
        let file = temp.path().join("file");
        std::fs::write(&file, "x").unwrap();

        for path in [temp.path().join("missing"), file] {
            match isolated().discover_repo(&path) {
                Err(GitSyncError::NotARepo { path: reported }) => assert_eq!(reported, path),
                other => panic!("expected NotARepo for {path:?}, got {other:?}"),
            }
            let status = isolated().status_for_directory(&path).unwrap();
            assert!(!status.in_repo, "{path:?}");
        }
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

    // A new directory of dotfile sources is reported file by file, since a
    // commit records index paths and a directory is never one.
    #[test]
    fn a_new_directory_reports_each_file_untracked() {
        let (_temp, path) = init_repo_for_commits();
        fs::create_dir_all(path.join("packages/nvim/lua")).unwrap();
        fs::write(path.join("packages/nvim/lua/init.lua"), "-- a\n").unwrap();
        fs::write(path.join("packages/nvim/lua/keys.lua"), "-- b\n").unwrap();

        let mut untracked = isolated().repo_status(&path).unwrap().untracked;
        untracked.sort();

        assert_eq!(
            untracked,
            [
                PathBuf::from("packages/nvim/lua/init.lua"),
                PathBuf::from("packages/nvim/lua/keys.lua"),
            ]
        );
    }

    // `git ls-files --deleted` reports a tracked file once its directory is
    // replaced by a link, here to a directory holding a key. Staging it would
    // read through the link, so git refuses, and the index keeps what it had.
    #[test]
    fn a_path_beyond_a_symlink_is_refused_and_nothing_is_staged() {
        let (_temp, path) = init_repo_for_commits();
        fs::create_dir_all(path.join("ssh")).unwrap();
        fs::write(path.join("ssh/config"), "Host a\n").unwrap();
        git(&path, &["add", "ssh/config"]).unwrap();
        git(&path, &["commit", "-m", "add ssh config"]).unwrap();
        let outside = tempfile::TempDir::new().unwrap();
        fs::write(outside.path().join("config"), "k7Qz-private-ssh-config\n").unwrap();
        fs::remove_dir_all(path.join("ssh")).unwrap();
        std::os::unix::fs::symlink(outside.path(), path.join("ssh")).unwrap();

        let result = isolated().stage_files(&path, &[PathBuf::from("ssh/config")]);

        assert!(
            matches!(
                &result,
                Err(GitSyncError::OperationFailed {
                    operation: "git add",
                    ..
                })
            ),
            "{result:?}"
        );
        assert_eq!(
            git(&path, &["cat-file", "-p", ":ssh/config"]).unwrap(),
            "Host a"
        );
    }

    // git reuses an index's cached subtree ids when it writes a tree, so an
    // index whose entries changed under a still-valid cache makes the user's
    // next `git commit` record the old subtree, silently reverting selfie's
    // commit. The fixture's own `git commit` populates that cache.
    #[test]
    fn a_later_git_commit_keeps_what_selfie_committed() {
        let (_temp, path) = nested_repo();
        fs::write(path.join("packages/foo/rc"), "b\n").unwrap();
        stage_and_commit(&path, &["packages/foo/rc"]).unwrap();

        fs::write(path.join("notes.txt"), "note\n").unwrap();
        git(&path, &["add", "notes.txt"]).unwrap();
        git(&path, &["commit", "-m", "the user's own commit"]).unwrap();

        assert_eq!(head_changes(&path), "A\tnotes.txt");
        assert_eq!(git(&path, &["show", "HEAD:packages/foo/rc"]).unwrap(), "b");
    }

    // An intent-to-add entry left in place makes git leave the path out of the
    // next tree it writes, so the user's next commit would delete it.
    #[test]
    fn an_intent_to_add_path_survives_the_users_next_commit() {
        let (_temp, path) = nested_repo();
        fs::write(path.join("packages/foo/new"), "n\n").unwrap();
        git(&path, &["add", "-N", "packages/foo/new"]).unwrap();
        stage_and_commit(&path, &["packages/foo/new"]).unwrap();

        fs::write(path.join("notes.txt"), "note\n").unwrap();
        git(&path, &["add", "notes.txt"]).unwrap();
        git(&path, &["commit", "-m", "the user's own commit"]).unwrap();

        assert_eq!(head_changes(&path), "A\tnotes.txt");
    }

    // The new content is a different size from the old, because git trusts a
    // size mismatch without rehashing: a stale stat then reads as a change.
    #[test]
    fn a_staged_file_does_not_read_as_modified() {
        let (_temp, path) = init_repo_for_commits();
        fs::write(path.join("foo.yml"), "name: foo\n").unwrap();
        git(&path, &["add", "foo.yml"]).unwrap();
        git(&path, &["commit", "-m", "add foo"]).unwrap();
        fs::write(path.join("foo.yml"), "name: foo\n# a longer body\n").unwrap();
        fs::write(path.join("new.yml"), "name: new\n").unwrap();

        isolated()
            .stage_files(&path, &[PathBuf::from("foo.yml"), PathBuf::from("new.yml")])
            .unwrap();

        // Unstaged changes only: anything here means the index disagrees with a
        // working tree that matches it.
        assert_eq!(git(&path, &["diff-files", "--name-only"]).unwrap(), "");
        assert_eq!(
            git(&path, &["status", "--porcelain"]).unwrap(),
            "M  foo.yml\nA  new.yml"
        );
    }

    // The link points outside the repository at bytes that must never reach a
    // blob, as a link to a private key would.
    #[test]
    fn a_symlink_is_staged_as_a_link_not_as_what_it_names() {
        let (_temp, path) = init_repo_for_commits();
        let outside = tempfile::TempDir::new().unwrap();
        let secret = outside.path().join("id_key");
        fs::write(&secret, "k7Qz-private-key-bytes\n").unwrap();
        std::os::unix::fs::symlink(&secret, path.join("link")).unwrap();

        isolated()
            .stage_files(&path, &[PathBuf::from("link")])
            .unwrap();

        let entry = git(&path, &["ls-files", "-s", "link"]).unwrap();
        assert!(entry.starts_with("120000 "), "staged as {entry:?}");
        let blob = git(&path, &["cat-file", "-p", ":link"]).unwrap();
        assert_eq!(blob, secret.to_str().unwrap());
    }

    // A dangling link is still a link: reading through it finds nothing, and a
    // stage that followed it would remove a tracked link from the index.
    #[test]
    fn a_dangling_symlink_stays_staged() {
        let (_temp, path) = init_repo_for_commits();
        std::os::unix::fs::symlink(path.join("nowhere"), path.join("dangling")).unwrap();
        git(&path, &["add", "dangling"]).unwrap();
        git(&path, &["commit", "-m", "add link"]).unwrap();

        isolated()
            .stage_files(&path, &[PathBuf::from("dangling")])
            .unwrap();

        let entry = git(&path, &["ls-files", "-s", "dangling"]).unwrap();
        assert!(entry.starts_with("120000 "), "staged as {entry:?}");
        assert_eq!(git(&path, &["status", "--porcelain"]).unwrap(), "");
    }

    // A fifo has no blob, and git leaves one out rather than wait on it. The
    // regular file beside it is still staged.
    #[test]
    fn a_fifo_is_left_unstaged() {
        let (_temp, path) = init_repo_for_commits();
        fs::write(path.join("a.yml"), "name: a\n").unwrap();
        nix::unistd::mkfifo(&path.join("pipe"), nix::sys::stat::Mode::S_IRWXU).unwrap();

        isolated()
            .stage_files(&path, &[PathBuf::from("a.yml"), PathBuf::from("pipe")])
            .unwrap();

        assert_eq!(git(&path, &["ls-files"]).unwrap(), "a.yml");
    }

    // A clean filter is how a git-crypt repository keeps plaintext out of
    // history. This one upper-cases, so the blob shows whether it ran.
    #[test]
    fn a_clean_filter_decides_the_staged_blob() {
        let (_temp, path) = init_repo_for_commits();
        fs::write(path.join(".gitattributes"), "*.secret filter=shout\n").unwrap();
        git(&path, &["config", "filter.shout.clean", "tr a-z A-Z"]).unwrap();
        git(&path, &["config", "filter.shout.smudge", "cat"]).unwrap();
        fs::write(path.join("token.secret"), "plaintext\n").unwrap();

        isolated()
            .stage_files(&path, &[PathBuf::from("token.secret")])
            .unwrap();

        assert_eq!(
            git(&path, &["cat-file", "-p", ":token.secret"]).unwrap(),
            "PLAINTEXT"
        );
    }

    // `x*.yml` is a file name, not a pattern: `xa.yml` beside it must stay
    // untracked.
    #[test]
    fn a_name_with_a_glob_character_stages_only_itself() {
        let (_temp, path) = init_repo_for_commits();
        fs::write(path.join("x*.yml"), "s\n").unwrap();
        fs::write(path.join("xa.yml"), "t\n").unwrap();

        isolated()
            .stage_files(&path, &[PathBuf::from("x*.yml")])
            .unwrap();

        assert_eq!(git(&path, &["ls-files"]).unwrap(), "x*.yml");
    }

    // `git add --all` with no paths stages the whole tree, so an empty list
    // must never reach git.
    #[test]
    fn staging_no_paths_changes_nothing() {
        let (_temp, path) = init_repo_for_commits();
        fs::write(path.join("a.yml"), "name: a\n").unwrap();

        isolated().stage_files(&path, &[]).unwrap();

        assert_eq!(git(&path, &["ls-files"]).unwrap(), "");
    }

    #[test]
    fn stage_files_works() {
        let (_temp, path) = init_repo_for_commits();
        fs::write(path.join("test.yml"), "name: test\n").unwrap();

        isolated()
            .stage_files(&path, &[PathBuf::from("test.yml")])
            .unwrap();

        let status = isolated().repo_status(&path).unwrap();
        assert_eq!(status.staged, [PathBuf::from("test.yml")]);
        assert!(status.untracked.is_empty(), "{:?}", status.untracked);
    }

    // A packages repository in the nested layout, committed once, as sync
    // push finds it.
    fn nested_repo() -> (tempfile::TempDir, PathBuf) {
        let (temp, path) = init_repo_for_commits();
        fs::create_dir_all(path.join("packages/foo")).unwrap();
        fs::write(path.join("packages/foo.yml"), "name: foo\n").unwrap();
        fs::write(path.join("packages/foo/rc"), "a\n").unwrap();
        fs::write(path.join("packages/foo/old"), "old\n").unwrap();
        fs::write(path.join("packages/bar.yml"), "name: bar\n").unwrap();
        git(&path, &["add", "-A"]).unwrap();
        git(&path, &["commit", "-m", "init"]).unwrap();
        (temp, path)
    }

    // Stages and commits `files` as sync push does, through the adapter alone.
    fn stage_and_commit(path: &Path, files: &[&str]) -> Result<CommitId, GitSyncError> {
        let files: Vec<PathBuf> = files.iter().map(PathBuf::from).collect();
        isolated().stage_files(path, &files)?;
        isolated().commit(path, &files, "chore(foo): update dotfile")
    }

    fn head(path: &Path) -> String {
        git(path, &["rev-parse", "HEAD"]).unwrap()
    }

    // What the commit at HEAD changed, as git reports it.
    fn head_changes(path: &Path) -> String {
        git(path, &["diff", "--name-status", "HEAD~1", "HEAD"]).unwrap()
    }

    // Every path and blob in a commit's tree, so a comparison sees any path
    // that was dropped, not only the ones the diff names.
    fn tree_listing(path: &Path, rev: &str) -> Vec<String> {
        git(path, &["ls-tree", "-r", rev])
            .unwrap()
            .lines()
            .map(String::from)
            .collect()
    }

    // The nested layout is where a commit built from the whole index deletes
    // every file: the commit has to change the one path and leave every other
    // entry, nested or not, exactly as the parent had it.
    #[test]
    fn commit_over_a_nested_path_changes_only_that_path() {
        let (_temp, path) = nested_repo();
        fs::write(path.join("packages/foo/rc"), "b\n").unwrap();

        stage_and_commit(&path, &["packages/foo/rc"]).unwrap();

        assert_eq!(head_changes(&path), "M\tpackages/foo/rc");
        let before = tree_listing(&path, "HEAD~1");
        let after = tree_listing(&path, "HEAD");
        let unchanged = |listing: &[String]| -> Vec<String> {
            listing
                .iter()
                .filter(|line| !line.ends_with("\tpackages/foo/rc"))
                .cloned()
                .collect()
        };
        assert_eq!(after.len(), before.len());
        assert_eq!(unchanged(&after), unchanged(&before));
        assert_eq!(git(&path, &["status", "--porcelain"]).unwrap(), "");
    }

    // The user's own staged work, an addition and a deletion, both of paths
    // selfie does not name. Neither may ride in selfie's commit, and both must
    // still be staged afterwards.
    #[test]
    fn commit_leaves_what_the_user_staged_out() {
        let (_temp, path) = nested_repo();
        fs::write(path.join("notes.txt"), "note\n").unwrap();
        git(&path, &["add", "notes.txt"]).unwrap();
        git(&path, &["rm", "-q", "--cached", "packages/bar.yml"]).unwrap();
        fs::write(path.join("packages/foo/rc"), "b\n").unwrap();

        stage_and_commit(&path, &["packages/foo/rc"]).unwrap();

        assert_eq!(head_changes(&path), "M\tpackages/foo/rc");
        let status = git(&path, &["status", "--porcelain"]).unwrap();
        assert!(status.contains("A  notes.txt"), "status: {status}");
        assert!(status.contains("D  packages/bar.yml"), "status: {status}");
    }

    // A path selfie names and deleted is removed; its sibling in the same
    // directory is not. The group names the unchanged sibling too, as a
    // package's group does, so the removal must come from the named path.
    #[test]
    fn commit_records_a_deletion_selfie_staged() {
        let (_temp, path) = nested_repo();
        fs::remove_file(path.join("packages/foo/old")).unwrap();

        stage_and_commit(&path, &["packages/foo/old", "packages/foo/rc"]).unwrap();

        assert_eq!(head_changes(&path), "D\tpackages/foo/old");
        assert!(
            tree_listing(&path, "HEAD")
                .iter()
                .any(|line| line.ends_with("\tpackages/foo/rc"))
        );
    }

    // `git rm` takes a path out of the index and the working tree alike, so
    // git has nothing to match it against. The group also names a modified
    // file, which must still be staged and committed beside the removal.
    #[test]
    fn a_path_removed_with_git_rm_is_committed_as_removed() {
        let (_temp, path) = nested_repo();
        git(&path, &["rm", "-q", "packages/bar.yml"]).unwrap();
        fs::write(path.join("packages/foo/rc"), "b\n").unwrap();

        stage_and_commit(&path, &["packages/bar.yml", "packages/foo/rc"]).unwrap();

        assert_eq!(
            head_changes(&path),
            "D\tpackages/bar.yml\nM\tpackages/foo/rc"
        );
    }

    // A commit names paths, so a staged rename has to be reported as both of
    // them: grouped by its new path alone, the old one stays in the tree.
    #[test]
    fn a_staged_rename_reports_both_paths() {
        let (_temp, path) = nested_repo();
        git(&path, &["mv", "packages/bar.yml", "packages/baz.yml"]).unwrap();

        let status = isolated().repo_status(&path).unwrap();

        assert_eq!(status.staged, [PathBuf::from("packages/baz.yml")]);
        assert_eq!(status.deleted, [PathBuf::from("packages/bar.yml")]);
    }

    // `git rm` leaves the path in neither the index nor the working tree. It is
    // reported as deleted, not staged, so sync does not try to read it.
    #[test]
    fn a_path_removed_with_git_rm_reports_as_deleted() {
        let (_temp, path) = nested_repo();
        git(&path, &["rm", "-q", "packages/bar.yml"]).unwrap();

        let status = isolated().repo_status(&path).unwrap();

        assert!(status.staged.is_empty(), "{:?}", status.staged);
        assert_eq!(status.deleted, [PathBuf::from("packages/bar.yml")]);
    }

    // Deleting a package's only file leaves no path for git add, which must
    // then not run at all: with no paths it would stage the whole tree.
    #[test]
    fn a_group_of_only_removed_paths_stages_nothing_else() {
        let (_temp, path) = nested_repo();
        git(&path, &["rm", "-q", "packages/bar.yml"]).unwrap();
        fs::write(path.join("notes.txt"), "note\n").unwrap();

        stage_and_commit(&path, &["packages/bar.yml"]).unwrap();

        assert_eq!(head_changes(&path), "D\tpackages/bar.yml");
        assert_eq!(
            git(&path, &["status", "--porcelain"]).unwrap(),
            "?? notes.txt"
        );
    }

    // A directory is never an index path, so a named one has no entry. The
    // commit refuses it rather than remove every file below it.
    #[test]
    fn a_named_directory_is_refused_rather_than_removed() {
        let (_temp, path) = nested_repo();
        let before = head(&path);

        let result = isolated().commit(&path, &[PathBuf::from("packages/foo")], "chore(foo): x");

        assert!(
            matches!(
                &result,
                Err(GitSyncError::OperationFailed {
                    operation: "remove tree entry",
                    ..
                })
            ),
            "{result:?}"
        );
        assert_eq!(head(&path), before);
    }

    // A tracked file replaced by a directory of the same name. The group names
    // the new file first, so the old file's removal must not take it along.
    #[test]
    fn a_file_replaced_by_a_directory_commits_the_file_below_it() {
        let (_temp, path) = init_repo_for_commits();
        fs::write(path.join("x"), "file\n").unwrap();
        git(&path, &["add", "x"]).unwrap();
        git(&path, &["commit", "-q", "-m", "add x"]).unwrap();
        fs::remove_file(path.join("x")).unwrap();
        fs::create_dir(path.join("x")).unwrap();
        fs::write(path.join("x/a"), "a\n").unwrap();

        stage_and_commit(&path, &["x/a", "x"]).unwrap();

        let listing = tree_listing(&path, "HEAD");
        assert_eq!(listing.len(), 1, "{listing:?}");
        assert!(listing[0].ends_with("\tx/a"), "{listing:?}");
    }

    #[test]
    fn the_first_commit_has_no_parent() {
        let (_temp, path) = init_repo();
        configure_identity(&path);
        fs::write(path.join("a.yml"), "name: a\n").unwrap();

        let id = stage_and_commit(&path, &["a.yml"]).unwrap();

        assert_eq!(head(&path), id.0);
        assert_eq!(git(&path, &["rev-list", "--count", "HEAD"]).unwrap(), "1");
        assert_eq!(tree_listing(&path, "HEAD").len(), 1);
    }

    // A real conflicting merge: git's state files mark it in progress, and
    // the index holds the conflict. Neither staging nor committing may write.
    #[test]
    fn a_merge_in_progress_refuses_staging_and_committing() {
        let (_temp, path) = nested_repo();
        git(&path, &["switch", "-q", "-c", "other"]).unwrap();
        fs::write(path.join("packages/foo/rc"), "theirs\n").unwrap();
        git(&path, &["commit", "-q", "-am", "theirs"]).unwrap();
        git(&path, &["switch", "-q", "-"]).unwrap();
        fs::write(path.join("packages/foo/rc"), "ours\n").unwrap();
        git(&path, &["commit", "-q", "-am", "ours"]).unwrap();
        git(&path, &["merge", "other"]).unwrap_err();
        let before = head(&path);
        let index_before = git(&path, &["ls-files", "-s"]).unwrap();
        fs::write(path.join("packages/foo.yml"), "name: foo\n# edited\n").unwrap();

        let staged = isolated().stage_files(&path, &[PathBuf::from("packages/foo.yml")]);
        let committed = isolated().commit(&path, &[PathBuf::from("packages/foo.yml")], "chore: x");

        for result in [staged.map(|()| None), committed.map(Some)] {
            assert!(
                matches!(
                    result,
                    Err(GitSyncError::OperationInProgress {
                        operation: InProgressOperation::Merge
                    })
                ),
                "expected the merge refusal: {result:?}"
            );
        }
        assert_eq!(head(&path), before);
        assert_eq!(git(&path, &["ls-files", "-s"]).unwrap(), index_before);
        assert!(path.join(".git/MERGE_HEAD").exists());
    }

    // A conflict with no operation in progress, as `git stash pop` leaves one.
    #[test]
    fn a_conflicted_path_is_refused() {
        let (_temp, path) = nested_repo();
        fs::write(path.join("packages/foo/rc"), "stashed\n").unwrap();
        git(&path, &["stash", "-q"]).unwrap();
        fs::write(path.join("packages/foo/rc"), "committed\n").unwrap();
        git(&path, &["commit", "-q", "-am", "committed"]).unwrap();
        git(&path, &["stash", "pop", "-q"]).unwrap_err();
        let before = head(&path);

        let result = stage_and_commit(&path, &["packages/foo/rc"]);

        assert!(
            matches!(&result, Err(GitSyncError::Conflicted { path }) if path == Path::new("packages/foo/rc")),
            "expected the conflict refusal: {result:?}"
        );
        assert_eq!(head(&path), before);
    }

    #[test]
    fn an_unchanged_tree_is_not_committed() {
        let (_temp, path) = nested_repo();
        let before = head(&path);

        let result = stage_and_commit(&path, &["packages/foo/rc"]);

        assert!(
            matches!(result, Err(GitSyncError::NothingToCommit)),
            "{result:?}"
        );
        assert_eq!(head(&path), before);
    }

    // A throwaway SSH key configured in the repository alone, as a user's
    // own signing setup would be, with an allowed-signers file so git can
    // verify the result.
    fn configure_ssh_signing(path: &Path, keys: &Path) {
        let key = keys.join("key");
        let status = std::process::Command::new("ssh-keygen")
            .args(["-q", "-t", "ed25519", "-N", "", "-C", "selfie-test", "-f"])
            .arg(&key)
            .status()
            .unwrap();
        assert!(status.success());
        let public = fs::read_to_string(keys.join("key.pub")).unwrap();
        let signers = keys.join("allowed_signers");
        fs::write(&signers, format!("selfie-test@example.invalid {public}")).unwrap();
        for (name, value) in [
            ("gpg.format", "ssh"),
            ("user.signingkey", key.to_str().unwrap()),
            ("gpg.ssh.allowedSignersFile", signers.to_str().unwrap()),
            ("commit.gpgsign", "true"),
        ] {
            git(path, &["config", name, value]).unwrap();
        }
    }

    #[test]
    fn a_commit_is_signed_when_the_repository_says_to() {
        let (_temp, path) = nested_repo();
        let keys = tempfile::TempDir::new().unwrap();
        configure_ssh_signing(&path, keys.path());
        fs::write(path.join("packages/foo/rc"), "b\n").unwrap();

        stage_and_commit(&path, &["packages/foo/rc"]).unwrap();

        // `verify-commit` exits non-zero on an unsigned or unverifiable commit.
        git(&path, &["verify-commit", "HEAD"]).unwrap();
        assert_eq!(head_changes(&path), "M\tpackages/foo/rc");
    }

    // A signer that fails, as `op-ssh-sign` does with its agent locked. The
    // branch must not move to an unsigned commit.
    #[test]
    fn a_failed_signature_leaves_the_branch_where_it_was() {
        let (_temp, path) = nested_repo();
        let keys = tempfile::TempDir::new().unwrap();
        configure_ssh_signing(&path, keys.path());
        git(&path, &["config", "gpg.ssh.program", "false"]).unwrap();
        fs::write(path.join("packages/foo/rc"), "b\n").unwrap();
        let before = head(&path);

        let result = stage_and_commit(&path, &["packages/foo/rc"]);

        assert!(
            matches!(
                &result,
                Err(GitSyncError::OperationFailed {
                    operation: "sign commit",
                    ..
                })
            ),
            "{result:?}"
        );
        assert_eq!(head(&path), before);
    }

    #[test]
    fn diff_commits_detects_changes() {
        let (_temp, path) = init_repo_for_commits();

        // First commit
        fs::write(path.join("a.yml"), "name: a\n").unwrap();
        git(&path, &["add", "a.yml"]).unwrap();
        git(&path, &["commit", "-m", "first"]).unwrap();
        let first = head(&path);

        // Second commit
        fs::write(path.join("b.yml"), "name: b\n").unwrap();
        git(&path, &["add", "b.yml"]).unwrap();
        git(&path, &["commit", "-m", "second"]).unwrap();
        let second = head(&path);

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

        assert_git_really_leaks_the_token(&path, PUSH.args);

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

        let error = failure(PUSH.label, &output, "!\tx:y\t[rejected] (z)");

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
