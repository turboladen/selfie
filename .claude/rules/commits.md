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

## Messages

Conventional-commit subjects, as the log already uses: `fix(scope):`, `docs:`, `refactor:`, `test:`,
`chore:`.

Start the body from the problem, not the change — what was wrong, and what a reader or user would
have hit. Then what was done about it, and what was verified and how, so the claim can be
re-checked. Assume no context and no tracker open; the same goes for a PR description.

Bead IDs belong in a `Refs:` trailer or the body. Bead **state** changes never ride in a feature
commit — see the beads section of `CLAUDE.md`.
