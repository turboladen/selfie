# Verification

These are cheap, they apply everywhere, and each one is here because skipping it cost real work.

## Confirm before asserting

- **Never reference a function, method, field or module without confirming it exists.** Grep for it.
  A plan in this repository called `CommandRunner::execute_in_dir` and cited "see its use in the
  install path"; neither existed.
- **Check a dependency's behavior against its source**, not its reputation or its README —
  `~/.cargo/registry/src/index.crates.io-*/<crate>-<version>/`. Three claims about `tempfile`'s
  platform behavior were wrong, one of them in the unsafe direction.
- **An invariant you can state is not an invariant the code enforces.** A doc comment saying every
  consumer must refuse a value explicitly did not stop the next consumer from guessing. If it
  matters, make it a type, a guard in a shared module, or a compile error.

## Your instrument can lie

- `git show --stat` **truncates long paths**; a grep over it reports false negatives.
- `grep -A N` windows **cut off** before the thing you are looking for.
- A build or test run in a working tree **another agent is editing** measures their work, not yours.
- `gh pr checks` returns an **empty list** before CI registers, so a loop waiting for "zero pending"
  exits immediately and reports no checks. Wait for the expected count first.
- **Issue IDs must be looked up, never recalled.** Four of four bead IDs cited from memory in a PR
  description were wrong. `bd list` or `bd search`, then grep the output.
- **Your context's copy of `CLAUDE.md` and `.claude/rules/*` is a snapshot from session start**, not
  the tree, and subagents inherit it. Four agents reported a CLAUDE.md claim corrected hours earlier
  by a merged PR. Before calling any doc or rule stale, read it: `git show <ref>:<path>`.

Before reporting a negative result — "it isn't there", "the gate fails", "no test covers this" —
confirm it a second way. Two of this session's accusations were withdrawn after doing so.

## Verify in a copy, never in a shared tree

```bash
git archive <sha> | tar -x -C "$SCRATCH"
cd "$SCRATCH" && CARGO_TARGET_DIR="$SCRATCH/target" cargo test
```

Never run `git stash`, `git reset`, or `git checkout --` in a working tree someone else may be
using. Read-only verification of a shared workspace has to be genuinely read-only, and `git stash`
is a write.

## Staging

**Stage explicit paths. Never `git add -A` or `git add .`.** Check `git diff --cached --name-only`
before committing. A tree can change between reading a diff and committing it, and `-A` is what lets
a stale read authorize a commit — that is how unreviewed code got committed here under someone
else's name.

Better still, `git commit -- <paths>`. It commits exactly what you name and leaves another writer's
index alone; plain staging does not, and a concurrently staged deletion will otherwise ride your
commit.

`git commit --amend -- <paths>` rebuilds the commit from the parent tree plus only those paths, so
it **silently drops deletions** the original commit made. Use `git add -- <paths>` then a bare
`--amend`.

Never `|| true` a `git switch`/`checkout`. It turns a failed branch switch into silently operating
on the wrong branch, and a following `reset --hard` then lands there.

To commit to a different branch while someone else holds the working tree, use
`git worktree add <scratch> <branch>` rather than switching branches under them.

Commit before running any mutation or experiment, so reverting it cannot destroy work.
