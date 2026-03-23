# Git Sync Design

Phase 4 of the environment manager design: git-backed sync for selfie package specs and dotfiles.

## Problem

Selfie manages package specs and dotfiles on disk, but has no built-in way to sync them across
machines. Users must manually run git commands in their packages repo. This friction discourages
keeping specs up to date and makes the new-machine workflow clunky.

## Goals

- Provide `selfie sync push/pull/status` as convenience wrappers over git operations.
- Generate useful per-package conventional commits automatically.
- Keep the abstraction thin — selfie handles the common case, users drop to git for edge cases.

## Non-Goals

- Full git client (merge conflict resolution, rebase, branch management).
- Multi-repo sync (syncing `package_directory` and `dotfiles_directory` when they're in separate
  repos).
- Network auth management (users configure SSH keys / credential helpers themselves).

## Design

### Repository Discovery

Selfie discovers the git repo by walking up from `package_directory` (via `gix::discover`). No new
config fields are needed. If `dotfiles_directory` is a sibling inside the same repo (the recommended
layout), it gets synced automatically.

If `package_directory` is not inside a git repo, all sync commands error with a helpful message
suggesting the user initialize one.

### `sync status`

Displays two sections:

**Repo status** — uncommitted changes and remote tracking state:

```
ℹ Repository: ~/selfie-packages (main)
  2 files modified, 1 untracked
  Up to date with origin/main
```

**Drift summary** — delegates to `DotfileService::check_drift()`, lists drifted package names:

```
⚠ Dotfile drift: 2 drifted out of 5 deployed
  starship, alacritty
  Run 'selfie apply' to redeploy or 'selfie dotfiles drift' for details
```

Clean state:

```
✓ Repository: ~/selfie-packages (main)
  No uncommitted changes, up to date with remote

✓ No dotfile drift (5 deployed)
```

Untracked files are reported as informational (warning), not errors.

### `sync push`

**Default behavior: one commit per package.**

1. Check repo status. If no changes, print "Nothing to push" and exit.
2. Group changed files by package name. Association rules:
   - YAML files (`*.yml`) → package name is the file stem (e.g., `starship.yml` → `starship`).
   - Files in a subdirectory → package name is the subdirectory name (e.g., `starship/starship.toml`
     → `starship`). This covers dotfile source files colocated with their package YAML in both
     `packages/` and `dotfiles/` directories.
   - Files that don't match either pattern are "ungrouped."
3. Ungrouped files are warned about: "2 files not associated with any package — use
   `--include-untracked` to commit them." Skipped by default.
4. For each package group, generate a conventional commit message:
   - New YAML file → `feat(<name>): add package spec`
   - Deleted YAML file → `chore(<name>): remove package spec`
   - Modified YAML file → `chore(<name>): update package spec`
   - New/modified dotfile source → `chore(<name>): update dotfile`
   - Both YAML and dotfile changes → `chore(<name>): update spec and dotfiles`
5. Prompt the user to confirm/edit each message via `dialoguer::Input` (generated message as
   default).
6. Commit each package group, then push all commits at once.
7. If push fails (remote has new commits), tell the user to run `sync pull` first. The local commits
   remain intact — the next `sync push` will detect no uncommitted changes and skip straight to
   pushing the existing commits.

**Flags:**

| Flag                  | Effect                                               |
| --------------------- | ---------------------------------------------------- |
| `--batch`             | Single commit for all changes instead of per-package |
| `--message "..."`     | Override commit message (only valid with `--batch`)  |
| `--yes` / `-y`        | Accept all generated messages without prompting      |
| `--include-untracked` | Include non-package files in a housekeeping commit   |

**MCP behavior:** No interactive prompting. Per-package commits by default using generated messages.
The AI can provide per-commit messages via a `messages: HashMap<String, String>` parameter mapping
package names to custom messages. For batch mode, use the `message` parameter (singular string).

### `sync pull`

1. Check repo status. If dirty (uncommitted changes), refuse: "Uncommitted changes detected. Run
   `selfie sync push` first, then try again."
2. Fetch from remote.
3. If no incoming changes, print "Already up to date."
4. Fast-forward merge. If not fast-forwardable (remote has diverged), error: "Remote has diverged.
   Resolve manually with git."
5. Show what changed by diffing old HEAD vs new HEAD, grouped by package:
   ```
   ✓ Pulled 3 commits from origin/main
     updated: starship, alacritty
     added: fnm
   ```
6. If pulled changes include dotfile source files, suggest applying:
   ```
   ℹ Dotfile sources changed — run 'selfie apply' to deploy updates
   ```

### Two-Phase Push Architecture

The per-commit prompt requires the service to pause and let the caller decide on each message. This
is handled via a two-phase API:

1. **`prepare_push(options) -> Result<Vec<PendingCommit>>`** — analyzes changes, groups by package,
   generates messages. No git mutations. Returns `Result` directly (not an event stream) because
   this is a query/preparation step, not a long-running operation. Errors (e.g., not in a repo, no
   remote) are returned as `anyhow::Error` for the caller to display.
2. **`execute_push(confirmed_commits) -> EventStream`** — stages, commits, and pushes. Emits
   progress events per commit.

This keeps the service non-interactive. The CLI iterates `PendingCommit`s and prompts for each. The
MCP server passes messages through directly.

```rust
pub struct PendingCommit {
    /// Package name (or "housekeeping" for ungrouped files)
    pub name: String,
    /// Generated conventional commit message
    pub message: String,
    /// Files to stage for this commit
    pub files: Vec<PathBuf>,
}

/// A commit confirmed by the caller, with a potentially edited message.
pub struct ConfirmedCommit {
    /// Files to stage for this commit
    pub files: Vec<PathBuf>,
    /// Final commit message (may have been edited by the user)
    pub message: String,
}

pub struct PushOptions {
    /// Single commit for everything
    pub batch: bool,
    /// Override message (only with batch)
    pub message: Option<String>,
    /// Skip per-commit prompts (use generated messages)
    pub auto_accept: bool,
    /// Include non-package files
    pub include_untracked: bool,
}
```

## Library Architecture

### New Module: `sync_service/`

```
crates/selfie/src/sync_service/
├── mod.rs              # re-exports
├── port.rs             # SyncService trait + GitOperations trait
├── service.rs          # SyncServiceImpl<G, D>
└── gix_adapter.rs      # GixGitOperations (implements GitOperations)
```

This is a new service (separate from `PackageServiceImpl`) because it has different dependencies —
git operations instead of a command runner. It follows the same hexagonal pattern as
`DotfileServiceImpl`.

### Ports

**`SyncService`** — the main service trait consumed by CLI and MCP:

```rust
pub trait SyncService {
    fn status(&self) -> impl Future<Output = EventStream> + Send;
    fn prepare_push(&self, options: PushOptions)
        -> impl Future<Output = Result<Vec<PendingCommit>>> + Send;
    fn execute_push(&self, commits: Vec<ConfirmedCommit>)
        -> impl Future<Output = EventStream> + Send;
    fn pull(&self) -> impl Future<Output = EventStream> + Send;
}
```

**`GitSyncProvider`** — port for git write/sync operations, enabling testable mocks. This is a
separate trait from the existing `GitStatusProvider` (which provides read-only status for
`PackageServiceImpl`). Both traits are implemented by a single concrete adapter (`GixGitAdapter`),
following interface segregation — each consumer only sees the methods it needs:

```rust
pub trait GitSyncProvider: Send + Sync {
    fn discover_repo(&self, path: &Path) -> Result<RepoInfo>;
    fn repo_status(&self, repo_root: &Path) -> Result<RepoStatus>;
    fn stage_files(&self, repo_root: &Path, files: &[PathBuf]) -> Result<()>;
    fn commit(&self, repo_root: &Path, message: &str) -> Result<CommitId>;
    fn push(&self, repo_root: &Path) -> Result<()>;
    fn fetch(&self, repo_root: &Path) -> Result<()>;
    fn fast_forward(&self, repo_root: &Path) -> Result<FastForwardResult>;
    fn diff_commits(&self, repo_root: &Path, from: &CommitId, to: &CommitId)
        -> Result<Vec<ChangedFile>>;
}
```

### Supporting Types

```rust
/// Information about the discovered git repo.
pub struct RepoInfo {
    pub root: PathBuf,
    pub branch: Option<String>,
    pub remote_name: Option<String>,
}

/// Working tree status summary.
pub struct RepoStatus {
    pub modified: Vec<PathBuf>,
    pub staged: Vec<PathBuf>,
    pub untracked: Vec<PathBuf>,
    pub deleted: Vec<PathBuf>,
    pub ahead: usize,
    pub behind: usize,
}

/// Opaque commit identifier (avoids leaking gix types through the port).
pub struct CommitId(pub String);

/// Result of a fast-forward attempt.
pub enum FastForwardResult {
    /// Successfully fast-forwarded, with old and new commit IDs.
    Advanced { from: CommitId, to: CommitId, commit_count: usize },
    /// Already up to date.
    AlreadyUpToDate,
    /// Cannot fast-forward (diverged histories).
    Diverged,
}

/// A file that changed between two commits.
pub struct ChangedFile {
    pub path: PathBuf,
    pub change_type: ChangeType,
}

pub enum ChangeType {
    Added,
    Modified,
    Deleted,
}
```

### Service Implementation

```rust
pub struct SyncServiceImpl<G, D> {
    git: G,
    dotfile_service: D,
    config: SelfieConfig,
}
```

Generic over:

- `G: GitSyncProvider` — for testable git mocking
- `D: DotfileService` — for drift checking in `sync status`

The service uses `config.package_directory()` as the discovery starting point for all git
operations.

### New Event Variants

New `OperationType` variants:

- `SyncStatus`
- `SyncPush`
- `SyncPull`

New `PackageEvent` variants, using `operation_info: OperationInfo` where applicable:

- `SyncRepoStatus { repo_root, branch, modified_count, staged_count, untracked_count, ahead, behind }`
  — emitted by `status()`, carries repo state for the CLI/MCP to render.
- `SyncDriftSummary { drifted_packages, total_deployed }` — emitted by `status()` after drift check.
- `SyncCommitCreated { operation_info, package_name, message }` — after each commit in
  `execute_push()`.
- `Completed` with new `OperationSuccess` variants:
  - `SyncPushComplete { commits_pushed }` — after successful push.
  - `SyncPullComplete { commits_pulled, packages_updated, packages_added, packages_removed }` —
    after successful pull.
  - `SyncPullUpToDate` — when pull finds nothing new.
  - `SyncNothingToPush` — when push finds no changes.

### Adapter: `GixGitAdapter`

A single concrete struct that implements **both** `GitStatusProvider` and `GitSyncProvider`. This
replaces the existing `GixGitStatusProvider` (renamed). The adapter lives in a shared location
accessible to both `PackageServiceImpl` (which only needs `GitStatusProvider`) and `SyncServiceImpl`
(which needs `GitSyncProvider`).

The adapter uses the `gix` crate for local operations (discover, status, stage, commit, diff) and
shells out to the `git` binary for push and fetch only, leveraging the user's existing
SSH/credential configuration. This is a pragmatic hybrid: `gix` for fast local operations, `git` CLI
for battle-tested networking.

**Migration:** `GixGitStatusProvider` → `GixGitAdapter`. All existing call sites that reference
`GixGitStatusProvider` are updated. The `git_adapter.rs` file moves from `package/` to a shared
location (e.g., `crates/selfie/src/git/adapter.rs`) so both services can access it without
cross-module coupling.

**Additional `gix` features needed:** The workspace `Cargo.toml` currently enables only `status`.
Staging and committing will require `index` and `revision` features at minimum. The exact feature
set will be determined during implementation.

## CLI Commands

```
selfie sync status
selfie sync push [--batch] [--message "..."] [--yes] [--include-untracked]
selfie sync pull
```

New subcommand group added to `ClapCommands`:

```rust
Sync(SyncCommands),
```

With `SyncSubcommands::Status`, `Push { ... }`, `Pull`.

### Command Handlers

- **`sync status`** — calls `sync_service.status()`, custom event handler for the combined repo +
  drift display.
- **`sync push`** — calls `prepare_push()`. If it returns an error, display it and exit. Otherwise,
  iterate `PendingCommit`s with `dialoguer::Input` for each (unless `--yes`), convert to
  `ConfirmedCommit`s, then call `execute_push()` and process the event stream.
- **`sync pull`** — calls `sync_service.pull()`, custom handler to suggest `selfie apply`.

## MCP Tools

| Tool                 | Parameters                                          | Notes                                           |
| -------------------- | --------------------------------------------------- | ----------------------------------------------- |
| `selfie_sync_status` | none                                                | Returns structured JSON: repo + drift           |
| `selfie_sync_push`   | `batch`, `message`, `messages`, `include_untracked` | Per-package by default, uses generated messages |
| `selfie_sync_pull`   | none                                                | Returns structured JSON: what changed           |

MCP `sync_push` parameters:

- `batch: bool` — single commit mode.
- `message: String` — override message for batch mode.
- `messages: HashMap<String, String>` — per-package message overrides (package name → message). Only
  used in per-package mode (default). Packages not in this map use the generated default.
- `include_untracked: bool` — include non-package files.

## Testing Strategy

### Unit Tests (library)

- **Commit message generation** — given file changes, assert correct conventional commit format.
- **File grouping** — given changed paths, assert correct package grouping.
- **Edge cases** — no remote, dirty repo detection, nothing to push, non-fast-forward pull.
- **`GitSyncProvider` mocked** via `mockall` behind `with_mocks` feature flag.

### CLI Tests

- **Event handler tests** — construct `EventStream` directly, verify output formatting for status,
  push progress, pull results.
- **No actual git operations** — all git behavior mocked through the service interface.

### Integration Tests

- **Real `gix` operations** against temp directories with `gix::init()`.
- **Full status/push/pull cycle** — init repo, add files, commit, verify status.
- Follow the pattern established in `git_adapter.rs` tests.
- **No network operations** — push/pull tested against local bare repos only.

## File Changes

### New Files

- `crates/selfie/src/git/mod.rs` — shared git module (re-exports)
- `crates/selfie/src/git/sync_provider.rs` — `GitSyncProvider` trait
- `crates/selfie/src/sync_service/mod.rs`
- `crates/selfie/src/sync_service/port.rs` — `SyncService` trait
- `crates/selfie/src/sync_service/service.rs` — `SyncServiceImpl<G, D>`
- `crates/cli/src/commands/sync.rs` (or `sync/` module with subcommands)

### Moved/Renamed Files

- `crates/selfie/src/package/git_adapter.rs` → `crates/selfie/src/git/adapter.rs`
  (`GixGitStatusProvider` → `GixGitAdapter`, now implements both `GitStatusProvider` and
  `GitSyncProvider`)
- `crates/selfie/src/package/git.rs` → `crates/selfie/src/git/status_provider.rs`
  (`GitStatusProvider` trait + types stay as-is)

### Modified Files

- `crates/selfie/src/lib.rs` — add `git` and `sync_service` modules
- `crates/selfie/src/package/mod.rs` — re-export git types from new location
- `crates/selfie/src/package/event.rs` — new event variants + `OperationType` variants
- `crates/cli/src/cli.rs` — `Sync` command group + subcommands
- `crates/cli/src/commands.rs` — dispatch + `sync` module
- `crates/mcp-server/src/server.rs` — three new tools + updated git adapter import
- `README.md` — status section
- `docs/configuration.md` — mention sync commands
