# 0008. A dotfile entry is refused or failed, and its kind names the condition

Date: 2026-10-03

## Status

Accepted 2026-10-03. Refines [0006](0006-command-output-contract.md), whose verdict and exit codes
this record does not change.

## Context

When apply or drift does not deploy or compare a dotfile entry, the library has to tell its
consumers (the CLI and the MCP server) which entry it was and why. Four ways of doing that go wrong:

- **A kind that names the code path, not the condition.** A target below a regular file can be found
  by a check before writing or by the writer meeting `ENOTDIR`. A fifo can be a source or a
  template. If each path files the state under a kind of its own, a consumer branching on the kind
  gives different remedies for one state on disk.
- **One word for two outcomes.** Selfie declining to act on something it found (a symlinked target,
  an escaping source) is a different event from selfie trying and an operation erroring (a resolve
  command that failed, a write the OS denied). Filing both as "refused" hides which happened. It
  also hides that a failed permission fix left content that was already right, and it counts a
  canceled run as a failure of one entry.
- **Catch-all arms.** A `_ =>` arm over filesystem errors files any new error silently, under
  whatever kind the arm names.
- **Labels written by hand.** A table matching every kind to a string can drift from its enum.

Exit codes are not in question: 0006 already scores a refusal and an error alike as Failed. This
record settles what the event says.

## Decision

### (a) Two outcomes, one classifier

An entry that is neither deployed, skipped nor conflicted ends in one of two outcomes:

- **Refused**: selfie found a condition under which it will not write the target, and wrote nothing.
  Decided from the entry, its files and its target, before any write. Most refusals come before
  anything runs. A secret-bearing entry's commands can run first, when the condition appears at its
  target while they run.
- **Failed**: selfie tried, and an operation it ran returned an error: the resolve command, the
  backup, the write, or the permission fix. The event names the operation and carries the error.

A canceled run is neither. The entry was not finished, and the run ends Canceled, as 0006 says.

A refusal's kind is a **condition**, plus **where** it was found:

```rust
pub struct Refusal { pub condition: Condition, pub at: Location, pub message: String }
pub enum Location { Entry, Source, Template, Target, AboveTarget }
pub enum Condition { Collision, InvalidEntry, TargetRule, Escapes, Symlink, Irregular, Directory,
                     NotADirectory, Absent, Unreadable, Undetermined, EarlierProgramFailed }
```

A condition names what is there, wherever it is found. A directory given as a source is `Directory`
at `Source`, as one at the target is `Directory` at `Target`. A missing source or template is
`Absent`. `Unreadable` is for a file that exists and could not be read. `Undetermined` is for a path
whose state could not be found out, at any location. An entry the package file spells wrongly (an
unknown key, a wrong shape, a var name that cannot be substituted) is `InvalidEntry`, since all
three have one remedy: edit the package file. The target rule concerns the spelling of the target,
so `TargetRule` is located at `Entry`.

The classifier's input is the filesystem port's own types: its error, what one read of a target
found, and what is at a path. Each is an enum selfie owns, so the classifier lists every variant
with no `_ =>` arm. It never takes an `io::Error`, whose kind cannot be matched exhaustively. The
port reports the condition at the failure: a read of a target says when a component above it is not
a directory, or when a file is there that could not be read; a read of a source or template says
whether nothing, a directory, or an unreadable file is there; a write says when a component above
the target is not a directory, or a directory is at it. Whatever the port cannot say is
`Undetermined`. Nothing looks at a path a second time to recover what the failure already knew.

`NotADirectory` is reported only for the target, the one path selfie creates a file at. On a path
selfie only reads, nothing can be there, so the condition is `Absent`.

A writer error that the classifier would have refused before writing is a refusal with the same
condition: a symlink the repository writer will not write through, a fifo, a directory at the
target, or a non-directory above it. Any other writer error is a failure. The secret writer replaces
a symlink at the target, so for it a link is never a condition.

**Recommended.** It removes both duplicate classifications by construction and gives a consumer one
remedy per condition. Rejected: keeping one flat enum keyed by condition alone. That still needs
`TemplateIrregular` beside `SourceIrregular`, so the variant count grows with every role.

Rejected: looking at the path again after a failure, to classify what is there now. The second look
races the failure it explains, can find a different state, and follows a symlink at the target that
the failed operation did not, so it names conditions the operation never met.

### (b) `declined` means the resolver answered Skip

`ConflictResolver::resolve` states that returning Skip means the conflict was presented, so a
declined conflict is not shown again. A resolver that gives no answer, such as one that panics,
leaves the conflict undeclined. Both paths ask through one helper.

Rejected: a third return value, `Shown`. No adapter needs to tell "shown and skipped" from
"skipped", and the contract costs nothing.

### (c) An event cannot carry "no drift"

`DriftType` has no `None`. The comparison returns `Option<DriftType>`, and `deploy_decision` takes
`Option<&DriftType>`. **Recommended**: the invariant "never `None` on an event" is a compile error,
not a doc sentence.

### (d) MCP labels come from the enums

Every enum an MCP row labels (`Condition`, `Location`, the failed operation, `DriftType`,
`SkipReason` and the source kind) derives strum's `IntoStaticStr` with snake_case names. The server
takes the label from the value and has no label tables for them. A label is a `&'static str` known
at compile time, so producing one cannot fail. **Recommended**: no table can drift from its enum. A
single test per enum pins the label set, since a rename is a change to the MCP contract.

Rejected: keeping hand-written labels for their independence from Rust names. A rename then becomes
a reviewed contract change, which is the property hand-written tables were supposed to give.

### (e) A repository file is one type

`DotfileSource::File(RepoPath)` and `Template { file: RepoPath, vars }`, where
`RepoPath { base, path }`. What every repository file shares is a method on `RepoPath`. The source
kind is the variant's label, as (d) gives every label. **Recommended**: one place to change a rule
about where a repository file lives.

## Consequences

- MCP `dotfile_refused` rows carry `condition` and `location`. `dotfile_failed` rows carry
  `operation` and `error`. Every apply result that completes carries both counts, `refused` and
  `failed`, whatever its status: `refused` when anything was refused, else `failed` when anything
  failed. A run stopped by `stop_on_error` and a canceled run end without a result's counts, as they
  end without its step count. The apply, drift and sync status descriptions list them.
- The CLI prints one sentence per entry. A failure's sentence names the operation, so a failed
  permission fix reads as one and not as a failed write. The summary names failed entries only when
  there are some.
- A repository-file target below a non-directory is refused before any write, in apply and in drift,
  as a secret-bearing one is. Drift therefore reports it as a refusal (exit 1), not as drift (exit
  3).
- The filesystem port reports why a read or write failed, so an adapter carries the errno mapping
  for its platform. A condition the port cannot name is `Undetermined`.
- Revisit if a consumer needs a remedy that depends on the code path rather than the condition. None
  does today.
