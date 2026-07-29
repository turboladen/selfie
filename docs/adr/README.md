# Architecture Decision Records

Decisions worth explaining to someone who was not there. Each one records what was chosen, what it
was chosen over, and why — so a later reader can tell a deliberate decision from an accident, and
knows what would have to change for the decision to be worth revisiting.

## Numbering

- Four-digit zero-padded ID, allocated in order: `0001`, `0002`, …
- The filename is `<id>-<kebab-case-slug>.md`.
- The title carries the **same** ID as the filename, padded identically, so the two can never
  disagree.
- IDs are never reused, including for an ADR that is later superseded.

```
docs/adr/0003-no-deploy-state-for-provider-sourced-dotfiles.md
# 0003. No deploy state for provider-sourced dotfiles
```

## Titles

State the decision, not the topic. "No deploy state for provider-sourced dotfiles" tells a reader
what was decided; "Deploy state" only tells them what it is about, which is what the body is for.

## Structure

Each record has:

- **Status** — Accepted, Superseded, or Deprecated. When one ADR refines or supersedes another, say
  so here in both records and link them by filename, so the relationship is visible from either end.
- **Context** — what made the decision necessary, including the constraints and the options that
  were seriously considered. An option rejected for a reason is worth more here than one never
  named.
- **Decision** — what was chosen, stated as what is true rather than as what changed.
- **Consequences** — what follows, including the costs. A record listing only benefits is not a
  decision record; the accepted downsides are the part a future reader most needs.

## Writing

Timeless and maintainer-facing. No development narrative ("we first tried…", "before this change…"),
no working-doc scaffolding, no references to the plan or pull request that produced it. Those belong
in commit messages, which describe a change; an ADR describes a state.

Write it when the decision is made rather than afterwards. The reasoning is hardest to reconstruct
precisely when it is most worth having.
