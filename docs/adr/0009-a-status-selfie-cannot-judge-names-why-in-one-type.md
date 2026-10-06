# 0009. A status selfie cannot judge names why, in one type

Date: 2026-10-06

## Status

Proposed. Refines [0006](0006-command-output-contract.md), whose verdicts and exit codes this record
does not change, and applies the labeling rule of
[0008](0008-a-dotfile-entry-is-refused-or-failed-and-its-kind-names-the-condition.md) (d) to a
second family of labels.

## Context

`package status` answers whether a package and each of its dependencies and recommendations are
installed. Where it cannot tell, the answer is `EnvironmentStatus::Unknown`. A first typed version
replaced that variant's sentence with an enum, `UnknownStatus`, of seven kinds. Two review rounds on
it found 14 and 15 issues, most of them the same few problems:

- **One kind covered conditions with different remedies.** Every lookup error other than "no such
  spec" became `Unloadable`, documented as a spec that answered and failed to load. That took in two
  files claiming the name, and a package directory selfie could not list, where no spec failed at
  all. `CheckError(String)` took in a canceled check, a shell that could not start, a working
  directory that could not be entered, and an unreadable output pipe.
- **The package and its dependencies were judged in one type and reported in two.** Each dependency
  row carried the new kind and reason. The package's own status, the same type, reached MCP as the
  bare word "unknown".
- **Data was carried and dropped.** `Refused` stored the refusal's kind, and nothing emitted it,
  although `package_refused` rows already label that kind.
- **Each fix added a variant or a label, written by hand** in a match table in the MCP server.

The check failures overlap a separate change that types `WorkingDirectoryUnusable` as a
`CommandFailure`, so the two have to agree on one type.

## Decision

### (a) A lookup error is classified by what it is about

```rust
pub enum UnknownStatus {
    NotFound(PackageRepoError),
    Unloadable(PackageRepoError),
    Ambiguous { conflicting_paths: Vec<PathBuf> },
    PackageDirectoryUnreadable(PackageRepoError),
    Refused { kind: RefusalKind, reason: String },
    NotInEnvironment,
    NoCheckCommand,
    CheckFailed(CheckFailure),
}
```

The lookup error is classified in this order, by predicates on `PackageRepoError` rather than by
matching its variants at the call site:

1. `means_no_such_package()` gives `NotFound`.
2. `MultiplePackagesFound` gives `Ambiguous`, with the paths as a field, as `NoSuchPackageReason`
   carries them.
3. `names_an_unusable_spec()` gives `Unloadable`. Only this kind may carry `failure`.
4. Anything else gives `PackageDirectoryUnreadable`, the safe direction that
   `names_an_unusable_spec` already documents: blaming the directory is recoverable, blaming a spec
   that is fine is not.

**Recommended.** `parse_failure()` and `names_an_unusable_spec()` are written in terms of each
other, so the two lists cannot drift.

Rejected: matching `PackageRepoError` variants in `info.rs`. A variant added later would fall
through to whichever arm the wildcard picked.

### (b) The package and its dependencies share one rendering

The package's own `environment_status.status` and every dependency and recommendation row are built
by one function, which returns `status`, `kind` and `reason`. An unknown root carries a kind and a
reason exactly as a dependency row does. **Recommended**: the duplicate match goes, and the two
cannot disagree.

Rejected: a nested `unknown` object on the root only. It gives one fact two shapes.

### (c) A refused dependency emits its refusal kind

A `Refused` row carries `refusal`, labeled by the same rule as the `kind` of a `package_refused`
row. **Recommended**: an assistant never parses the reason to learn which refusal it was.

### (d) A check that gave no answer says why, by a type shared with command failures

```rust
pub enum CheckFailure { Canceled, CouldNotStart, WorkingDirectoryUnusable, TimedOut(TimedOut),
                        OutputUnreadable }
```

`run_check` maps a `CommandError` to a `CheckFailure` by an exhaustive match, with no `_ =>` arm, in
place of rendering it to text. `CheckResult::Error(String)` and `CheckResult::TimedOut` both become
`CheckResult::Failed(CheckFailure)`. `CommandFailure::WorkingDirectoryUnusable` and
`CheckFailure::WorkingDirectoryUnusable` carry the same fields. `could_not_run` is the one place
that builds a check failure. A canceled check stays `Canceled` all the way to the row, so a consumer
branching on the kind does not tell the user that their check command is broken.

The fallback for "no status was produced for this check" is a broken invariant, not a user's
condition. It is a `debug_assert!` plus `CheckFailure::Canceled` only when the token is canceled,
and otherwise unreachable by construction: statuses are zipped with the checks that produced them.

**Recommended.** Rejected: keeping `CheckError(String)` with a sentence per cause. The label is the
contract, and one label for four causes is the defect.

### (e) Labels come from serde

`UnknownStatus` and `CheckFailure` derive `Serialize` with `rename_all = "snake_case"`, as 0008 (d)
sets for the entry-refusal enums. The MCP server serializes the value and has no label table. One
test per enum pins the label set. The labels are `not_found`, `unloadable`, `ambiguous`,
`package_directory_unreadable`, `refused`, `not_in_environment`, `no_check_command`, and, from
`CheckFailure`, `canceled`, `could_not_start`, `working_directory_unusable`, `timed_out` and
`output_unreadable`. **Recommended**: a rename is a reviewed contract change, and no table can drift
from its enum.

## Consequences

- MCP rows for a package's status and each dependency and recommendation carry `status`, `kind` (the
  label, null unless unknown), `reason` (prose), `failure` (an unloadable spec's parse failure),
  `conflicting_paths` (ambiguous) and `refusal` (refused). `reason` keeps the prose it carries
  today, so a published row's fields keep their meaning. The `selfie_package_status` description
  lists the labels and says to branch on `kind`.
- The CLI prints one sentence per status, as it does now. `Display` on each type is that sentence.
- `package check` and `package list` read `CheckFailure` too, so a canceled check is reported as
  canceled there as well. Exit codes are unchanged: 0006 scores a canceled run 130, and a check
  selfie timed out or could not start as Failed.
- Whether a validation's stored outcome can disagree with its counts is a separate question, and
  this record does not settle it.
- Revisit if a consumer needs to tell two lookup failures apart that this record files under
  `PackageDirectoryUnreadable`. None does today.
