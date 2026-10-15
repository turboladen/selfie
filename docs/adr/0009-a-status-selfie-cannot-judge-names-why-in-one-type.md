# 0009. A status selfie cannot judge names why, in one type

Date: 2026-10-06

## Status

Accepted 2026-10-14. Refines [0006](0006-command-output-contract.md), whose verdicts and exit codes
this record does not change, and applies the labeling rule of ADR 0008, "A dotfile entry is refused
or failed, and its kind names the condition", (d) to a second family of labels.

## Context

`package status` answers whether a package and each of its dependencies and recommendations are
installed. Where it cannot tell, the answer is `EnvironmentStatus::Unknown(String)`, a sentence
built at each site that cannot judge. A consumer has only that sentence:

- **Conditions with different remedies read alike.** A dependency no spec answers to, one whose spec
  does not parse, a name two files claim, and a package directory selfie could not list are told
  apart only by their wording.
- **A check that gave no answer is text.** `CheckResult::Error(String)` covers a canceled check, a
  shell that could not start, a working directory that could not be entered, an output pipe that
  could not be read, and a check task that panicked. `package check`, `package list` and
  `package status` all report it as "error", so an assistant told to branch on it sends the user to
  fix a check command that is fine.
- **The package and its dependencies are reported in two shapes.** A dependency row carries the
  sentence as `reason`. The package's own status reaches MCP as the bare word "unknown".
- **A refused dependency loses its refusal's kind**, although `package_refused` rows label it.

## Decision

### (a) A failed lookup is classified once, by what it is about

`PackageRepoError::lookup_kind` is the one classification of a failed lookup. It matches every error
variant, with no wildcard, and answers not found, ambiguous name, unloadable, or package directory
unreadable. For an error only another operation raises (a save, a track, a create, or a step after
the spec loaded) it answers `None`, so a new error variant is a compile error there.
`means_no_such_package` and `names_an_unusable_spec` answer from it.

`into_lookup_failure` turns the error into the `LookupFailure` that kind names, or returns it
unchanged, as `Err`, when it has none. A caller then reports that error as itself, with no label:

```rust
pub enum LookupFailure {
    NotFound(NotFound),            // its sentence names a missing package directory
    AmbiguousName(AmbiguousName),  // the files claiming the name
    Unloadable(UnloadableSpec),    // the parse failure
    PackageDirectoryUnreadable(UnreadableDirectory),
}
```

Each wrapper holds the facts its kind needs, copied out once by the classifier, beside the original
error, and only `into_lookup_failure` builds one, so a variant cannot carry facts its kind does not
name. Its sentence is the error's own. The classifier lists every error variant with no wildcard,
and `None` is the safe answer for an error that is not about one spec: a caller losing the package's
name is recoverable, a caller blaming the wrong file is not. **Recommended.**

Rejected: classifying by predicates at each call site. Two call sites gave two answers for one
error.

### (b) An unknown status is one type, for a package and its dependencies

```rust
pub enum UnknownStatus {
    LookupFailed(LookupFailure),
    Refused { kind: RefusalKind, reason: String },
    NotInEnvironment,
    NoCheckCommand,
    CheckFailed(CheckFailure),
    Error(PackageRepoError),       // an error a lookup does not return, shown as itself
}
```

The package's own status and every dependency and recommendation row are rendered by one function,
which gives `status`, `kind` (the label), `reason` (prose), and the fields a kind adds: `failure`
for an unloadable spec, `conflicting_paths` for an ambiguous name, and `refusal`, `{kind, reason}`,
for a refused spec. **Recommended**: the two shapes cannot disagree.

Rejected: a nested `unknown` object on the root only. It gives one fact two shapes.

### (c) A check that gave no answer says why

```rust
pub enum CheckFailure { Canceled { message }, CouldNotStart { message },
                        WorkingDirectoryUnusable(UnusableDirectory),
                        TimedOut(TimedOut), ResultUnreadable { message }, Internal { message } }
```

A check gives `CheckResult::Success` or `CheckResult::Failed` when it ran and answered, and
`CheckResult::CouldNotRun(CheckFailure)` when it did not. One function maps a `CommandError` to a
`CheckFailure`, by an exhaustive match with no `_ =>` arm, and one test pins every arm:

| `CommandError`                                        | `CheckFailure`             |
| ----------------------------------------------------- | -------------------------- |
| `Cancelled`                                           | `Canceled`                 |
| `SpawnFailed`, `StdoutSpawn`, `StderrSpawn`           | `CouldNotStart`            |
| `WorkingDirectoryUnusable`                            | `WorkingDirectoryUnusable` |
| `Timeout`                                             | `TimedOut`                 |
| `OutputReadFailed`, `ContentMarkersAbsent`, `IoError` | `ResultUnreadable`         |

`CouldNotStart` means selfie could not start the check or attach to its output, so the check may not
have run. `ResultUnreadable` means the check started and selfie could not read its result to the
end: its output, or its exit status, which a failed wait loses. `Internal` is a check task that
panicked, which only `package list`'s own tasks can do; its sentence says it is a bug in selfie.
Each sentence is the `CommandError`'s own, bounded as forwarded text is, so one that embeds a long
command is elided in the middle. The types that carry the sentence apply the bound themselves.

`CommandError::WorkingDirectoryUnusable` carries an `UnusableDirectory`: the command, the directory
and the cause, with its sentence derived from them. A check carries that value as
`CheckFailure::WorkingDirectoryUnusable`. An operation that fails on it, from a check or from an
install command, fails as `CommandFailure::WorkingDirectoryUnusable`. A dotfile `command:` source
reports it through its own resolve error.

A check that could not run reaches its row as `CouldNotRun` in `package check`, `package list` and
`package status`. `package check` then fails as the error does. `package list` and `package status`
report the row and exit 0, as for any row they cannot judge. The CLI shows a canceled check as
canceled. `package status` turns each dependency's lookup into its status in one pass, so every
dependency has exactly one. **Recommended.**

Rejected: one `CheckResult::Error(String)` with a sentence per cause. The label is the contract, and
one label for five causes is the defect.

### (d) Labels are derived from the enums

Each label of `LookupFailure` and `CheckFailure` is the variant's name in snake_case, from
`strum::IntoStaticStr` with `serialize_all = "snake_case"`, so the MCP server has no label table.
serde would serialize these data-carrying enums as objects, not labels. `UnknownStatus::kind` takes
the carried failure's label for a failed lookup or check, names its other three kinds, and is `None`
for `Error`. One test per enum pins the label set, and one checks that each tool description lists
every label: from `LookupFailure`, `not_found`, `ambiguous_name` (the label `package_refused` rows
use for the same condition), `unloadable` and `package_directory_unreadable`; `refused`,
`not_in_environment` and `no_check_command`; and from `CheckFailure`, `canceled`, `could_not_start`,
`working_directory_unusable`, `timed_out`, `result_unreadable` and `internal`. **Recommended**: a
rename is a reviewed contract change, and no table can drift from its enum.

## Consequences

- MCP status rows carry `status`, `kind`, `reason`, `failure`, `conflicting_paths` and `refusal`,
  each null where it does not apply. `reason` is prose, so a published row's fields keep their
  meaning. The `selfie_package_status` description lists the labels and says to branch on `kind`.
- `package check` and `package list` rows keep their published `status` values, `"error"` and
  `"timed out"`, and carry `kind`, `reason` and `working_directory`, and a listing row also
  `command`, for a check that gave no answer.
- The CLI prints one sentence per status. `Display` on each type is that sentence. An operation that
  fails on an unusable working directory reads as the error's own sentence, with no `Command error:`
  prefix.
- Exit codes are unchanged: 0006 scores a canceled run 130, and a check selfie timed out or could
  not start as Failed.
- Whether a validation's stored outcome can disagree with its counts is a separate question, and
  this record does not settle it.
- Revisit if a consumer needs to tell apart two lookup failures this record files under
  `PackageDirectoryUnreadable`. None does today.
