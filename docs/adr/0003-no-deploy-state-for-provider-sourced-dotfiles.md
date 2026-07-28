# 3. No deploy state for provider-sourced dotfiles

Date: 2026-07-24

## Status

Accepted

Refines [ADR-0001](0001-machine-specific-and-secret-dotfiles.md), which established externally
sourced dotfile content and left its deploy-state and drift handling to be specified when built.

## Context

`selfie apply` records what it deployed in a per-machine deploy state file, storing a checksum of
the source and of the content it wrote. Comparing those checksums against current contents on a
later run distinguishes a changed repository file from a target the user has edited, which is what
lets selfie refresh safely and flag genuine conflicts.

A dotfile whose content comes from a provider command breaks both halves of that record. There is no
source file to checksum, and the deployed content is a credential — so the stored deployment
checksum would be a hash of a secret, written to a plain file on disk.

Hashing does not make this safe. An unsalted hash of a credential is a confirmation oracle: anyone
who can read the state file can test candidate values offline and confirm a match. Whether that
matters depends on the entropy of the particular secret, which is not a judgement the deploy path
can make on the user's behalf.

Deriving change detection from filesystem metadata instead was considered. Comparing a target's
modification time against the recorded deployment time requires no secret material, but it fails in
the unsafe direction: tools that preserve modification times — archive restores, `rsync -a`, `cp -p`
— yield a genuinely modified target whose timestamp predates the recorded deployment. selfie would
read that as untouched and overwrite user-authored content without prompting.

The premise underlying both options is also false. Stored checksums are not required to compare a
dotfile's intended content against what is on disk. At apply time selfie holds both values in
memory. A stored checksum answers only the narrower question of which side changed when the two
differ.

## Decision

Provider-sourced dotfiles carry no deploy state. Nothing is recorded when one is deployed, and
nothing is consulted when one is applied.

Their deploy decision is made entirely from values held in memory at apply time: the provider's
output and the target's current contents. Identical content is skipped; an absent target is written;
any difference against an existing target is a conflict requiring explicit confirmation.

selfie deliberately does not distinguish a rotated secret from a user-edited target. Both present as
a difference, and both are treated as a conflict. Editing a credentials file by hand is plausible
enough that silently overwriting it is not acceptable, and no mechanism can separate the two cases
without persisting something derived from the secret.

Secret-bearing content must not reach the event stream, a log line, or an error message. Because the
event stream is the library's only unconditional egress, this is enforced where events are
constructed rather than at each call site. A conflict is reported there with the target path, the
command or template that produced the content, and the number of lines on each side — enough to
distinguish a rotated value from a hand-edited file without revealing either. Commands and variable
names are shown, being references rather than values.

Interactive conflict resolution is a separate channel. The resolver is supplied by the calling
adapter, so an interactive front end may offer to display the two values on explicit request, while
a non-interactive caller supplies no resolver and therefore cannot reach that path at all. An
adapter that offers it must warn first: displayed content persists in terminal scrollback and in any
session capture, which is beyond selfie's control. The alternative — refusing ever to show the
values — leaves the user choosing between overwrite and skip with no basis, which carries its own
risk of discarding a live credential.

Content is written byte for byte, including trailing whitespace. Targets are created readable only
by their owner and put in place atomically, so that no window exists in which the content is
readable by others, and no interrupted write can leave a truncated credential behind.

Provider commands run with their working directory set to the package file's parent directory, the
same base against which repository-file sources resolve.

This decision governs any dotfile whose content is secret-bearing, whether that content arrives as a
command's entire output or as values substituted into a template
([ADR-0004](0004-named-value-substitution-for-dotfiles.md)). Both are resolved in memory at apply
time and neither is recorded.

## Consequences

- No secret, and nothing derived from a secret, is persisted by selfie.
- A provider entry behaves identically on first apply and on every subsequent one, since there is no
  accumulated state to diverge. This is what makes adopting selfie on a machine with pre-existing,
  divergent configuration safe.
- A rotated secret produces a conflict rather than refreshing silently. This is the accepted cost of
  refusing to persist secret-derived data.
- Drift reporting cannot describe provider entries in the same terms as repository-backed ones, and
  identifies them as provider-sourced and unverifiable rather than inventing a drift classification.
- Every apply of a provider entry executes its command, which may prompt for authentication. Caching
  the result would be a secret at rest and is therefore excluded.
- Conflict presentation for these entries cannot reuse the existing unified diff, and needs its own
  non-revealing summary.
