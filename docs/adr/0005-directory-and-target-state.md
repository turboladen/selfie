# 0005. One classification for directory and target state

Date: 2026-09-20

## Status

Accepted

Refines [ADR-0003](0003-no-deploy-state-for-provider-sourced-dotfiles.md), whose secret-bearing
write path is one of the two target classifications unified here, and
[ADR-0004](0004-named-value-substitution-for-dotfiles.md), whose templated entries are
secret-bearing and take that same path.

## Context

selfie asks two questions about the file system over and over. What is at a directory path, and what
is at a dotfile's target. Both are answered in many places, in different words, by code that cannot
see the other answers.

Two directories reach the service as one and the same error: a directory that exists and cannot be
listed, and a directory selfie could not check at all. So no consumer can tell "there may be
dotfiles in here" from "nothing is known". The dotfiles directory is probed for existence and then
listed to find out why the probe failed, in three separate guards. One module names that directory's
states in three sentences, the interactive tracker names them in a fourth, and the MCP tool
descriptions name them again per tool. Whether the directory is expected is decided by one rule in
the service and a different one in each adapter.

At a target, a directory and a file deleted between the existence probe and the read both arrive as
"exists but could not be read", which names the mechanism rather than the state. The secret-bearing
path classifies its target separately from the repository-file path and reaches a different answer
for the same file. The two writers treat a symlinked target oppositely, correctly in each case, with
nothing recording that they should.

Each of these is small. Together they are why a fix here adds a variant or a special case, and why
the same fact ends up classified in more places after a fix than before.

## Decision

### 1. One classification of what is at a directory path

The `FileSystem` port answers one question about a directory path and returns one type. Its states
are **directory**, **absent**, **unlistable** and **unknown**.

Absent covers every way a path holds no directory: nothing is there, something that is not a
directory is there, the final component is a dangling symlink, or a parent is not a directory. The
state carries which, because the remedy is not shared. Only an empty path is fixed by creating the
directory; the `mkdir -p` suggestion fails with "File exists" against a plain file and with "No such
file or directory" against a dangling link. Sentence and remedy both derive from the reason, so only
an empty path is offered the creation hint. Unlistable is a directory whose entries could not be
read, which may be hiding specs. Unknown is a check that failed.

That type travels. `PackageListError` carries it instead of re-deriving it from an error kind, it
replaces the service's separate classification of an unlisted dotfiles directory, and the reason a
package was not found derives from it. One module holds one sentence per state per intent, warning
or refusal, and every adapter renders those rather than writing its own.

Rejected: a general IO error as the catch-all, inspected by each consumer, which is what conflates
unlistable with unknown today. Rejected: a pair of booleans for existence and directoryness, which
cannot express a check that failed and so leaves a caller reading "could not look" as "nothing
there". Rejected: letting each adapter classify, which is where the competing wordings come from.

Reopen when a consumer must act differently on a distinction these states do not draw, or when a
remedy sentence differs between sub-cases the reason does not separate. Telling a permission denial
from a transient failure on an unlistable directory is such a case, since one has a remedy the user
can apply and the other does not.

### 2. When the dotfiles directory is expected

Expected means configured. The service owns the rule as one function of the configuration and the
directory's state, and every adapter asks it.

Expectedness decides only whether an absent directory is mentioned. It does not decide refusals. A
directory that is unlistable or unknown refuses any command that would otherwise claim to have seen
every dotfile, whether or not the user configured it, because either state may be hiding entries. An
absent directory refuses nothing.

Within absence it governs an **empty path** and nothing else. An empty default is the ordinary
condition of a setup that keeps no standalone dotfiles, so it warns when configured and is silent
when not. Every other reason a path holds no directory — a plain file, a dangling symlink, a
component that is not a directory — warns whether or not the path was configured, because none of
them can be reached by leaving the setting out. Staying silent about them hides the reason the
directory is not being read from the only user who did not ask for it to be read.

A failed listing carries the path it read beside the directory's state, so the sentence naming a
directory and the listing that failed come from one value rather than from the configuration on one
side and the repository's own path on the other.

The name-uniqueness check asks this rule too. It discards every listing error from the dotfiles
repository, so an unlistable directory reads as "the name is free". No spec is created on that
answer, because the track that follows lists the same directory itself and refuses when it cannot;
but the user is told two different things about one directory in one command, and the check that
exists to catch a collision has answered without looking.

A command claims to have seen every dotfile when its result is an answer about all of them. A sync
status does not: it relays a drift check as supplementary information and reports counts. It carries
drift's refusal forward as a warning and marks its counts as covering only what could be listed,
while its own result stays a success. A status reporting nothing because one directory could not be
listed is worth less than one reporting what it saw and what it missed.

Rejected: expected means configured or seen. Nothing remembers a directory selfie merely saw, and
making the answer depend on a run's history gives two machines in the same state different answers.
Rejected: letting expectedness govern refusals, which lets an unset directory that cannot be listed
pass while a configured one fails, though both hide the same entries.

Reopen when selfie gains a durable record of the dotfiles directory itself, or when a command
appears that reads the directory without claiming to have seen all of it.

### 3. What is at a dotfile's target

Apply, drift and track classify a target through one function whose states are **absent**,
**readable**, **directory**, **link**, **irregular** and **unreadable**.

The classifier is a single read. A read that fails because nothing is there is absent, so a file
deleted between a probe and the read deploys instead of being reported as unreadable. A directory is
its own state with its own sentence. The port's irregular-target question keeps excluding
directories, because it exists to stop a read that would block and opening a directory never blocks;
the port's documentation is corrected to say so.

The read never follows a link at the final component or waits on a fifo: a link, dangling or not,
and a fifo, socket or device node are states of their own, which back up the questions ahead of the
read rather than replacing them. A link or fifo the read finds appeared after those questions and is
handled as they would handle it: refused on the repository-file paths, and on the secret path
replaced only once a further look confirms the link is still there. The non-following symlink
question therefore stays a separate call, ordered ahead of the classifier on every path reaching a
target, and it is what the warning below reads the link's destination from. The two questions take
different stats and are not merged.

The secret-bearing path uses the same classifier and the same refusal. The conflict detail handed to
a resolver carries the target's state rather than a byte slice, so a revealed conflict cannot render
an unreadable target as an empty file.

The two writers keep diverging on a symlinked target, and the divergence is recorded here rather
than removed. A repository-file entry refuses: the link is the user's configuration, the content is
recoverable, and destroying the link on a routine update is the common path. A secret-bearing entry
replaces the link, because following it would send a credential to a destination its author chose,
and refusing would leave the credential undeployed with no remedy but deleting the user's own link.
Replacement is the only outcome that completes the write and keeps the content where the user asked
for it.

After the non-following symlink question finds a link at a secret-bearing entry's target, the link's
destination is classified with a following stat before any provider command runs or template
renders. A destination that is a fifo, socket or device node refuses the entry, and nothing is
executed. A directory destination does not: the replacement lands on the link rather than on what it
points at, so it proceeds like any other link.

A directory at the target itself, with no link involved, refuses before any command runs, because a
rename cannot replace a directory with a file. A plain target that selfie cannot classify is refused
as well, since that is where the write lands. Both preserve the rule that nothing runs for a target
that provably cannot be written.

Past that gate, a secret-bearing target that is a link is always replaced with a regular owner-only
file, whether or not the destination already holds the resolved content. The outcome never depends
on the destination's mode.

The warning naming the link and its destination is sent after the write succeeds, worded as what
happened, so it can never precede a refusal or a failed write. A dry run words the same warning in
the conditional.

A dry run reports the same outcome class a real run would reach: refused for an irregular
destination, and otherwise that it would replace the link. It counts a replacement it would make the
way a dry run counts a repository-file deploy it would make, as a skip, so no preview prints a
deployed count. A refused destination is counted as a refusal, as a real run counts it.

Rejected: unify both writers on refusal, which leaves a credential permanently undeployed. Rejected:
unify both on replacement, which destroys legitimate links during ordinary updates. Rejected:
leaving the secret path its own target handling, which is how one function came to answer the same
question two ways. Rejected: classify the link but leave the destination unclassified until a
provider command runs or a template renders. A credential fetch, possibly with a biometric prompt,
would then run for a target the writer goes on to refuse. Rejected: word a dry run as a skip for a
symlinked secret target. A preview would then say nothing about a link a real run will replace.
Rejected: keep the rule that leaves an already-matching destination's link alone. An outcome that
depends on the destination's mode is what this decision removes.

Reopen when a third content class appears whose writer is neither of these two, or when a write mode
exists that completes a write without replacing the link. Reopen also when the secret writer gains a
way to write through a link safely, or when drift gains a write.

### 4. Unrecognized top-level keys in a package file

One rule decides what a package's top level may hold, and it refuses the whole package on any key
that rule does not accept. The key's value plays no part, so a scalar left over from an earlier
format refuses exactly as a misspelled list does, and apply and drift answer alike.

The rule is one function, which judges a key against the level it was found at and words the
complaint in the same breath. Membership and wording are not separable choices, which is what
stopped a call site testing a key against one level from explaining it in terms of another. An
underscore-prefixed key naming no real field is an anchor definition and is accepted; one naming a
real field is refused, since an anchor cannot be told from a misspelling of that field. The sentence
names the key, and then either the field it cannot be told apart from or the fields the level does
accept.

Every consumer asks that one function. Validation reports its answer as an issue, a save refuses to
rewrite the file, and apply and drift refuse the package before deploying any part of it. A top
level that could not be read back at all refuses too, since no key of it was examined.

Rejected: warn and carry on when the key cannot affect what deploys, judged from the value's shape
so that a mapping or a sequence refuses and a scalar warns. A typo in a scalar key is as much a sign
of a hand-edited file the user has not finished as a typo in a list key, and a warning on an
otherwise green run is the diagnostic they do not see. Rejected: warn on every unrecognized key,
which lets a misspelled dotfiles list deploy nothing under a successful run.

Reopen when something other than selfie reads these files, since a key belonging to that reader is
not a mistake and a rule serving both may have to be stricter for one than the other. Reopen also if
the spec format itself gains a version field, which turns the leftover key most likely to motivate a
warning into a legal one.

### 5. Two entries for one target, and the scope of track's scan

Two entries whose targets expand to one path are refused where they sit in one package file, by spec
validation and by every command that would deploy it. One function in the library decides it, which
validation reports as an issue and a deploying command as a refusal.

The two ask at different scopes, because validation reads a file's fields and must not depend on
which environment a run is in. Validation compares each scope's own list against itself, then the
effective set for every environment the file declares. Apply compares the effective set for the
current environment. An environment-scoped entry overriding a shared one for the same target is
legal under both, which is what overrides are for.

Across packages within one run, apply deploys neither occurrence: it warns once, naming both
sources, skips both, and counts the target as refused so the run's exit code carries it. Drift
reports the same refusal. Precedence between two packages is deliberately not defined, and
"whichever package came first" would be a precedence: a deploy state keyed by target holds one
source, so any tie-break that depends on enumeration order flips the recorded source between runs.
The user resolves it by removing one entry.

Track's already-tracked scan reads every scope. A target claimed by an environment-scoped entry is
already tracked whichever environment the run is in, because appending a shared entry for it creates
exactly that collision. The library's short-circuit reads only the shared list while the interactive
pre-check reads every scope, and that disagreement is what this clause removes.

Rejected: define precedence between packages, for the record-flipping reason above. Rejected: refuse
the whole run on a cross-package collision, which blocks an apply over two unrelated packages.
Rejected: scanning only the current environment, which appends an entry colliding only on another
machine.

Reopen when the deploy state can hold more than one source per target, or when entries gain explicit
ordering.

### 6. What `stop_on_error` stops

`stop_on_error` governs every dotfile failure in an apply, and its default is off.

A failure is an entry selfie could not carry out: a source it could not read, a target it refused or
could not classify, a write that failed, a provider command that failed. A conflict is not a failure
and never stops a run, because a conflict is the designed answer to a divergent target rather than
an error. A warning is not a failure.

The default moves with the scope, because the two halves are one decision. A setting governing every
failure that defaults to stopping lets the first failure hide every other one in the same run: a
user with two broken entries fixes one, runs again, and only then learns of the second. Proceeding
does not let a failure pass unnoticed, since the exit code and the summary both carry it. So
continuing costs nothing and gains the whole list.

Continuing is not free for provider commands, whose failure is usually shared by the same program's
later commands and costs a prompt or a timeout each, so once one fails, later entries running that
program are refused without running, while other programs' entries still run.

This is the only decision here that changes a setting's documented default, and the only one a user
notices without editing their configuration.

Rejected: keep the setting governing only secret-resolution failures and document the narrowing. The
key's name promises a halt on the first problem, and a document that narrows it still leaves a user
who set it expecting one. Rejected: widen the scope and keep the default on, which is the
defect-hiding run above. Rejected: let it stop on conflicts, which ends most first runs on a second
machine.

Reopen when a dotfile outcome appears that is none of a completed entry, a conflict, or a failure,
or when a failure class appears after which continuing is unsafe rather than merely noisy.

### 7. A tracked entry whose target became a symlink

Apply and drift both report a tracked entry whose target is a symlink, on every run, whether or not
the content still matches. Apply warns and skips it. Drift reports it as its own state rather than
as content drift, since the content may be identical. Both word it through the one function that
words a refused deploy.

The recorded deployment is left in place. Deleting it would report the entry as never deployed,
which is false, and would offer to overwrite the link on the next run.

Rejected: refuse the run, which stops an apply of everything else over a link the user created
deliberately. Rejected: stay silent while content matches, which leaves a record asserting a
deployment selfie cannot perform; after the next repository edit the entry never advances and
nothing says why.

Reopen when selfie can record that a target is intentionally a link, at which point the report is
noise for entries the user has acknowledged.

### 8. Three settled questions about durability and state

#### Both writers flush the parent directory

Three things take the owner-only writer: the deploy state, the copy taken before an overwrite, and
every secret-bearing target. The copy is the argument. It is written before the overwrite lands, so
a crash keeping the overwrite and losing the copy's directory entry destroys the only remaining copy
of what the user had. Nothing else here outranks that.

The ordering the existing rule protects survives. A record lies only when it outlives the write it
describes, and equal durability on both sides cannot produce that. The rule is restated as that
ordering rather than as a prohibition on flushing.

The cost is accepted rather than avoided. Both parent flushes are best effort, since opening a
directory needs read permission and a write-only parent would otherwise fail a deploy that works. So
a run whose target's parent flush fails while the state's succeeds is a window this opens and the
current asymmetry closes. It is narrower than the case it removes, needing both a parent selfie
cannot open for reading and a crash inside that window. Secret-bearing targets take the same flush
at the same best-effort cost; their owner-only mode is unchanged.

Rejected: keeping the asymmetry and naming it in the writer's mode, which leaves the pre-overwrite
copy the least durable write selfie makes. Reopen if the deploy state or the backup moves to a
writer that does not flush its parent, because the ordering would then depend on the writers
differing.

#### The state directory is created whether or not it is configured

A run is refused only when something unusable is already at that path. Writing a setting's default
value into the configuration file must not change what selfie does. The parity argument with the
package directory does not carry: a package directory holds files the user authored and selfie must
not invent one, while the state directory holds only selfie's own record and selfie is its sole
author.

Rejected: exempting the default value, which makes behavior turn on string equality with a computed
path. Reopen when the state directory holds anything a user authors or that another tool shares.

#### An unusable deploy state refuses only a run that writes

Drift and a dry run warn and carry on against an empty state, so every entry shows as untracked and
nothing is concealed; both already describe themselves that way. Among runs that write, a package
whose entries are all secret-bearing records nothing and consults nothing, so an unusable state
cannot mislead it. One repository-file entry is enough to refuse the whole run before any write.

Rejected: refusing every run that meets an unusable state, which refuses a check that writes
nothing. Rejected: deciding per entry inside a mixed run, which is a partial apply that records
nothing for the entries that needed a record. Reopen if secret-bearing entries gain a persisted
record, which ADR-0003 forbids, or if drift gains a write.

### 9. Capability-based file system access

selfie does not adopt a capability-based file system library.

The invariant such a library would make structural already holds by construction. The port offers
two writers and no following one, so no call site can write through a link at the final component by
forgetting a flag. What a capability API would add beyond that is containment against a symlinked
**parent** directory, which neither writer covers and which no current threat model requires.

Reopen when selfie must write somewhere whose parent directory is under someone else's control, or
when a third writer is proposed. Adoption then needs a cost measured against the library's own
source, covering what the port's methods become and what the mock-based tests have to be rewritten
to.

## Consequences

- The `FileSystem` port gains one directory-state method and loses the probe-then-list pair used to
  work out why a listing failed. In the YAML repository, three existence guards ahead of three
  listings become one classification each.
- `PackageListError`'s two variants and the service's three-variant classification of an unlisted
  dotfiles directory collapse into the one state type, and the reason a package was not found stops
  being derived from an error kind. Every sentence about that directory then lives once: the
  service's warning, its two track refusals, the interactive tracker's wording and the per-tool
  wordings in the MCP server all render from one set.
- A repository's listing error carries the directory's path and state, so attaching a repository and
  naming that directory stop being unrelated facts.
- The target classifier gains a directory state and loses its preceding existence probe. The
  secret-bearing apply path stops classifying its own target, and the conflict detail handed to a
  resolver changes shape to carry the state.
- The name-uniqueness check stops discarding the dotfiles repository's listing errors, so a track
  from either adapter refuses rather than creating a spec whose name a directory selfie could not
  list already holds.
- Seven inline irregular-target checks in the dotfile service's own module keep their positions,
  since each one guards a read that would otherwise block, but they stop deciding what a target is.
  Two more, in the resolve and state-file modules, do the same. The six symlink decisions in that
  service reduce to the classifier plus the two writers' own refusals.
- The rule file describing secret handling states what decision 3 does: a symlinked target is
  refused for repository-file content and always replaced, without being followed, for
  secret-bearing content. The destination is classified before any provider command runs or template
  renders, and the warning is sent after the replacement succeeds.
- docs/package-files.md's "Deploy behavior and permissions" paragraph is corrected to state the
  always-replace rule, dropping the case where the destination's mode decides whether the link
  survives.
- The unknown-key rule costs no code in apply and drift. At two other sites it does. Validation
  reports a top level it could not read back as an error, in the refusal's own words, so the package
  is invalid wherever it is judged. A save stops walking environments itself and asks the same
  function the listing asks, which differs from apply's only in taking every environment rather than
  one named one; entry-level keys come from a third function, shared with validation, that returns
  each key with the path naming it. The decision also settles that a key left over from an earlier
  format is removed from the file rather than tolerated, which is work in whichever package
  repository carries one.
- One duplicate-target rule in the library serves a cross-entry check in validation and a within-run
  check in apply, asked at the scopes decision 5 sets. Track's short-circuit widens from the shared
  list to every scope.
- `stop_on_error` changes user-visible behavior twice over: it begins governing repository-file
  entries, and its default becomes off, so an apply no longer halts at the first failure for anyone
  who has not set it. The configuration documentation states the new default, what counts as a
  failure, and that a conflict is not one. Both example blocks that set the key need revisiting.
- A tracked entry whose target is a symlink produces a line from apply and from drift where both are
  silent, including when the content has not changed.
- A configured state directory equal to the default stops refusing, and the configuration
  documentation loses the note beside the example that said it would.
- The owner-only writer gains a parent flush, making the pre-overwrite copy as durable as the target
  that replaces it and giving every secret-bearing target the same durability, and the code's
  directive against syncing harder becomes the ordering it protects.
