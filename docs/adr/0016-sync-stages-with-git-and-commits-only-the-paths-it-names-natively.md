# 0016. Sync stages with git and commits only the paths it names, natively

## Status

Accepted. Staging, the commit and its refusals, and signing are implemented. The deadline on
shell-outs and the preconditions in the last two sections of the decision are decided here and not
implemented yet.

## Context

`selfie sync push` stages the files it groups by package and commits each group. Its commit was
built from the whole index: it upserted every index entry into HEAD's tree, then removed every path
it found in HEAD that the index lacked. That design fails in two independent ways.

- **It commits what the user staged.** A file the user had staged for their own reasons lands in
  selfie's commit under a package's message, including a file selfie reported as ungrouped and
  excluded. An unresolved merge is flattened into a single-parent commit, and a sparse index loses
  its excluded trees.
- **It deletes whole directories.** gix's breadth-first tree traversal records directories as well
  as files. A directory such as `packages` is never an index path, so the deletion pass removes it,
  and with it every file below. In the usual nested layout, one changed dotfile produced a commit
  that deleted every tracked file, and the push sent it to the remote.

Building the commit from the whole index also skips everything `git commit` does around the object:
hooks, signing, and the refusal to make a partial commit mid-merge.

Staging was native too, and it reproduced `git add` only in part. It read through a symlink and
staged the bytes of whatever the link named, a key outside the repository included. It left the
entry's stat and the index's cached trees stale, so git reported a committed file as modified and
the user's next `git commit` reverted selfie's. And it applied none of the repository's attributes:
no clean filter, so a git-crypt repository's plaintext would be pushed and an LFS file committed
whole, no eol conversion, no `core.fileMode`, no clearing of an intent-to-add entry. Each of those
is a behavior `git add` already has.

Three ways to create the commit were considered, and two ways to stage.

- **Shell out to `git commit --only -- <paths>`.** git limits the commit to the named paths, refuses
  mid-merge, and runs hooks and signing as the user configured them. It was rejected because it
  moves the one write selfie must get right outside the library's control. Its behavior then depends
  on the git binary on `PATH`, on its version, and on whatever hooks the repository carries, all
  running inside an MCP tool call.
- **Repair the native builder, but keep the whole-index model.** Skipping directories in the
  deletion walk fixes the data loss. It leaves the user's staged work, the merge state, and signing
  as they are.
- **A native commit over named paths.** The commit starts from HEAD's tree and changes only the
  paths selfie names. Preconditions and signing are explicit steps of the commit, not side effects
  of a model that reads the whole index.

For staging, a native implementation means reimplementing `git add`: the filter pipeline, attribute
lookup, eol conversion, file modes, symlinks and the index's extensions. Every part left out is a
way to commit something other than what git would, and the clean-filter case leaks a secret. Running
`git add` for the named paths takes all of it from git, and it stays limited to those paths, so it
reintroduces none of the whole-index problems.

## Decision

**Staging runs `git add` for the named paths. The commit is native gix. Network operations shell out
to git.**

### Staging is `git add` over the named paths

`GitSyncProvider::stage_files` runs `git --literal-pathspecs add --all -- <paths>` for exactly the
paths of one group, non-interactively and reading the user's git configuration, as push and fetch
do. Literal pathspecs keep a file name such as `*.yml` from matching other files. `--all` records a
named path that is gone from the working tree as removed. What a path's blob, mode and index entry
are is git's decision, so attributes, filters, symlinks and paths beyond a symlink are handled as
`git add` handles them, and git maintains the index's extensions. Staging runs no hook.

### A commit changes only the paths it names

`GitSyncProvider::commit` takes the paths to commit. The tree starts from HEAD's tree, or the empty
tree for a first commit. For each named path, it records the index's stage-0 entry, or removes the
path when the index has none. No other path is read or written, so nothing is ever deleted by
inference from what the index lacks. Every other index entry stays staged and uncommitted.

A path the user staged belongs to selfie's commit only when selfie groups it into that commit. The
grouping already reads staged changes, and that does not change.

### A commit refuses rather than guesses

Staging and the commit are both refused, and nothing is written, when:

- a merge, rebase, cherry-pick, revert, bisect or `git am` is in progress;
- a named path has conflict stages in the index;
- the resulting tree equals the parent's tree, which only the commit can see.

`sync push` treats the last of these as a package with nothing to commit, not as a failure: it warns
by the package's name, makes no commit for it, and goes on with the others. A group whose files,
staged from disk, match the last commit is what a user leaves behind by staging an edit and then
reverting the file. Stopping the whole push for it would hold back every other package for a change
that does not exist. The other refusals stop the push.

### Signing follows the user's git configuration

When `commit.gpgSign` is set, the commit is signed with gix's signing support, configured from the
same keys git reads: `gpg.format`, that format's program (`gpg.ssh.program` for SSH), and
`user.signingKey` or `gpg.ssh.defaultKeyCommand`. The commit object is written without moving any
reference, signed, and then HEAD's branch is moved by an update that requires it to still point at
the parent the commit was built on. A signing failure therefore leaves HEAD where it was, and the
error says why.

### Hooks are not run

A native commit runs no `pre-commit`, `commit-msg` or other hook. Running them later means running
them from the library under a deadline, with their output treated as foreign text. That is follow-up
work.

### Network operations and the fast-forward shell out

`push`, `fetch` and the fast-forward in `sync pull` keep running the git CLI. That gives SSH-agent
and credential-helper integration exactly as the user's own git has it, and git's own guards when a
fast-forward would overwrite local changes in the working tree. Each shell-out runs under a
deadline, so a remote that accepts a connection and never answers cannot hang an MCP tool call. A
fetch legitimately takes longer than a package check, so the deadline is a setting of its own rather
than `command_timeout`.

### Preconditions are checked before anything is written

`sync push` refuses with `NoRemote` before staging anything when the current branch has no upstream.
A push that cannot happen does not leave local commits behind as a side effect. `sync pull` keeps
its refusal when changes are staged. `DirtyWorkingTree` and `RepoStatus::has_uncommitted_changes`
are removed, because the fast-forward's own guard covers what they would check.

## Consequences

- A sync commit can never contain a path selfie did not name, and can never delete a path it did not
  name. Both are properties of the tree builder, each pinned by a test and a mutation that breaks
  it.
- Signed repositories keep getting signed commits. That depends on gix's signing code, which is
  young. If it fails with a signing program git accepts, the object and reference step can move to
  `git commit-tree -S` and `git update-ref` without changing the tree builder.
- Hooks are bypassed. A repository that relies on a `pre-commit` or `commit-msg` hook gets selfie
  commits that the hook never saw. That is documented in `docs/sync.md` until hook support lands.
- Enabling gix's `command` feature adds `gix-command` to the dependency tree.
- Staging depends on the git CLI, as push and fetch already do, and on its version. A group's files
  stay staged when its commit then fails, for example on a signing error.
- The library now reproduces git's commit semantics instead of borrowing them. Any later commit
  behavior, such as `commit.cleanup` or `core.hooksPath`, is work selfie has to do, rather than
  something it inherits from git.
- Push and fetch keep depending on the git CLI. Moving them to gix waits until gix's push protocol
  is complete enough to replace it.
