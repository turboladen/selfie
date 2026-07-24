# 2. Git, not CRDTs, for reconciling divergent configuration

Date: 2026-07-23

## Status

Accepted

## Context

selfie replicates configuration across a user's machines. The canonical form of each config lives in
a git package repository; `selfie apply` deploys it to its target, and drift detection compares the
deployed target against that source. Configuration can diverge in two places: between different
machines' clones of the repository, and between a deployed target and its source after a local edit.

An earlier design for selfie considered conflict-free replicated data types (CRDTs) as a way to
reconcile divergent configuration automatically. selfie's model has since settled on deploying files
by copying them, with git as the transport between machines, which changes whether CRDTs are a good
fit.

## Decision

selfie uses git as its synchronization substrate for configuration and does not adopt CRDTs.

- Divergence between machines' clones of the package repository is reconciled with git; the package
  repository is a git repository and syncing is push / pull / merge.
- Divergence between a deployed target and its source is a two-way "which wins" decision governed by
  the overwrite-safety rules in ADR 0001, not a concurrent-operation merge.

## Consequences

- selfie deploys opaque files that users edit with external editors; it observes file _states_, not
  edit _operations_. Without operation capture, a CRDT reduces to a state-based merge, which for
  text is equivalent to a three-way textual merge — already provided by git — at far lower
  complexity.
- Automatic convergence is undesirable for configuration: two independently-merged edits can produce
  a file that no longer parses or behaves as intended. A visible conflict that a person (or an
  assistant) resolves is preferable to silent convergence.
- Adopting CRDTs would require selfie to own a replicated document store and a replication
  transport, well beyond its role of orchestrating user-defined commands.
- Richer reconciliation of a genuine conflict — an assistant proposing a semantic merge, or a
  format-aware three-way merge using existing tooling — remains available as a future enhancement
  and does not depend on CRDTs.

## Alternatives considered

- **A CRDT-backed configuration store** (for example Automerge-style local-first replication):
  rejected. It brings a runtime and storage layer selfie does not otherwise need, cannot capture
  edit operations made through external editors, and offers automatic convergence whose result is
  unsafe for structured configuration.
