# Commits

## One commit per problem solved

A branch's commits should map to the **problems it solves**, not to the order the work happened in.
One problem, one commit. Three problems, three commits. Twenty incremental commits that all chip
away at the same problem get squashed to one before the PR merges.

The test: reading the subject alone, would someone know what changed about the software? _"fix the
last two files"_, _"address review"_ and _"more of the same"_ describe the session, not the change.

## What counts as its own problem

- A distinct defect, feature or refactor.
- **A mechanical change whose verification depends on staying separable.** A repo-wide rename, or a
  marker conversion, is one problem — and keeping it apart is what lets someone confirm it changed
  nothing else. Folded into a commit that also rewrites content, `git blame` can no longer tell the
  two apart.
- **A change to user-visible behavior**, kept apart from internal work so a bisect lands on it
  rather than on a 49-file sweep.

## What does not

- Each file, or each batch of files, in one sweep. That is the order you worked in.
- Fixes to your own work from earlier in the same branch. Fold them into the commit that introduced
  the problem.
- Review feedback on the PR. Fold it in too, unless it turns out to fix a genuinely different
  problem from the one under review.

## Squash on the branch, not with the merge button

GitHub's **Squash and merge** always collapses a PR to exactly one commit, which is wrong whenever a
PR solves more than one problem. Do the grouping on the branch with a rebase, then merge normally —
this repo keeps merge commits, and a PR's individual commits stay on `main`.

**After any history rewrite, prove the tree did not move:**

```bash
git diff <old-tip> HEAD   # must be empty
```

That check is what makes reordering safe. A conflict resolved wrongly shows up as a tree difference,
so an empty diff means the only thing that changed is how the work is grouped.

Compare against the right baseline. After folding review fixes in, the tree you started from is the
one **with the fixup commits applied**, not the tip you had before them — those differ by exactly
the fixes, and reporting the second comparison as "the tree did not move" is a claim nobody can
check.

**An empty diff says nothing about the intermediate commits.** It proves the final tree survived; it
is silent on whether each commit still builds, which is the property a bisect needs and the one a
rewrite is most likely to break. So re-run the per-commit check after **any** rewrite, before
pushing — the rewrite's existence is the trigger, not a judgment about its size. That judgment is
what failed on #157: the same check ran after #155's fold and was skipped after #157's because the
fold felt smaller, and two commits reached `main` uncertified.

The per-commit check is `just clippy` on every commit below the tip, each in its own archive and
target directory (below). It type-checks every crate and test target, which is what a bisect needs.
The tests, the docs gate and the rest of `just check` run once, at the tip.

A green tip cannot stand in for it. A commit that fails to build is invisible at the tip whenever a
later commit fixes it, which is the ordinary shape of a fold.

**Give every archived commit its own `CARGO_TARGET_DIR`.** `git archive` stamps files with the
commit's timestamp, which is older than any artifact already in a shared target directory, so cargo
sees nothing to rebuild and runs the tip's binary against the older commit's source. The check then
reports a test that does not exist at that commit as passing. Two reviewers hit this in one session.
A fresh directory per commit is the fix; a run that prints no `Compiling selfie v` or
`Checking selfie v` line never built anything and must be scored as never-ran, not as passed.

## Messages

Conventional-commit subjects, as the log already uses: `fix(scope):`, `docs:`, `refactor:`, `test:`,
`chore:`.

Start the body from the problem, not the change — what was wrong, and what a reader or user would
have hit. Then what was done about it, and what was verified and how, so the claim can be
re-checked. Assume no context and no tracker open; the same goes for a PR description.

Bead IDs belong in a `Refs:` trailer or the body. Bead **state** changes never ride in a feature
commit — see the beads section of `CLAUDE.md`.
