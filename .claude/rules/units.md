# Units of work

These rules set how a session scopes, bounds and finishes a unit of work. They exist because two
months of bug fixing filed more beads than it closed, and the three largest data-loss risks were
never filed at all. Each rule names the failure it prevents.

## A unit is a theme slice, not a bead

A unit names the beads it closes and the code paths it owns. Anything found on an owned path is in
scope. Scoping to one bead is what pushed in-subsystem findings out of scope and into the backlog.

## Fix on sight, within a bound

While working a unit, fix a defect rather than filing it when all three hold:

1. It lies on a code path the unit already changes.
2. It needs no design decision: the correct behavior is already documented, or follows from the
   surrounding contract.
3. The fix is one extra commit of at most about 150 changed lines, test included.

Otherwise file it, with the label of the phase it belongs to, and list it in the PR body. The size
bound is what stops the loop: a large find is filed by rule, so a unit cannot grow past its plan.

## Design before code when the model is unsettled

The trigger: two consecutive review rounds on one PR each find about as many issues as the last,
concentrated in one subsystem. Stop. Write the model as an ADR under `docs/adr/`, get it agreed in
conversation, then build once. The signs that show up early: each fix adds an enum variant or a
special case, the same fact is classified in more places after the fix than before, and reviewers
start naming findings about which layer a check lives in.

## Two review rounds per PR

`/code-review` runs at most twice on a PR. A third round is the design trigger above, not a third
fix pass. Run mutation checks before requesting review, not after: a PR that ran them first has gone
through review with zero findings, and one that ran them after took six rounds. After a fold, re-run
only the mutations the fold touched (`testing.md`).

## Priority order is fixed

Data loss, then security, then a wrong exit code or a false success, then everything else. A finding
in a higher tier preempts a unit working a lower one.

## Dogfood findings outrank reviewer findings

A bead labeled `dogfood` came from the maintainer using the binary. It is worked at the start of the
next session, before any other bead of the same tier. Reviewers find what reading finds; use finds
what matters.

## Every phase ends with the maintainer running the binary

The last step of a phase is `cargo install --path crates/cli`, a rebuild of `selfie-mcp`, and a
numbered list of real commands for the maintainer to run against their real config, with the
expected result beside each. What they find becomes `dogfood` beads.

## Behavior-affecting PRs carry a binary diff

Build the merge-base binary into a scratch target directory, run both binaries over the same
sandbox, diff output and exit codes, and list the fixtures the diff covered in the PR body. A diff
covers the inputs it was given and nothing else, so the fixture list is the claim.

## Bead hygiene

`.beads/issues.jsonl` is ignored in this repo, so closing a bead is `bd close` plus `bd dolt push`,
with no commit. Labels drive `bd ready`: every open bead carries `mvp` or `post-mvp`, and one phase
or roadmap label. Bead writes are serialized through one agent; reads are free.

## Pick each agent's model by what a wrong answer costs

The lead's own model is the ceiling, not the default. Choose per spawn, by how much judgment the
task needs and how expensive a mistake is:

- **Sonnet** for search, inventory, counting, and gate watching: an Explore agent surveying files, a
  script that tallies beads, a watcher that reports when a log ends.
- **Sonnet** for a docs-only or single-file mechanical unit, and for a reviewer of one.
- **Opus** for an author of a substantive unit (several commits, tests with mutations, a binary
  diff) and for every plan reviewer and final code reviewer of such a unit.
- **The lead's model** only for a final reviewer of a data-loss or secrets unit, where a missed
  defect destroys a user's file or leaks a credential, and for the lead itself.

State the choice in the spawn prompt's first line so the transcript shows it. When unsure between
two tiers, take the cheaper one for search and the dearer one for review.

## What the lead reports at the end of a session

Beads closed against beads filed this session, with a target of at most 1.5 filed per closed. The
open `mvp` count. Review rounds per PR. The open `dogfood` count.
