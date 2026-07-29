---
paths:
  - "crates/selfie/src/dotfile_service/**/*.rs"
  - "crates/selfie/src/package/event.rs"
  - "crates/selfie/src/fs/**/*.rs"
  - "crates/cli/src/commands/track.rs"
  - "crates/cli/src/display_manager.rs"
  - "crates/cli/src/event_processor.rs"
  - "crates/mcp-server/src/**/*.rs"
---

# Handling secret-bearing content

A dotfile whose content comes from a `command`, or from a `source` with `vars`, is secret-bearing:
its content is a credential fetched at apply time. ADR-0003 and ADR-0004 are the contract.

## Egress cannot be enumerated by reading the code

Three separate exits for the same secret were each found by a different reviewer, none of them by
inspection: the event stream, the failure path, and `tracing`. Assume there is a fourth.

Test egress at the **boundary**, not by listing known paths:

- Scan every emitted `PackageEvent`'s `Debug` output for the secret literal — every event, not one
  field of one variant. A leak added to a warning or a deployed event is how this actually happens.
- Install a **capturing `tracing` subscriber** and assert its output is secret-free too. Event
  scanning catches library logging only because `EventSender` mirrors log calls into events; a bare
  `tracing::debug!` bypasses that entirely. Assert the captured buffer is non-empty, or the test
  passes by capturing nothing.
- Give every leak test a **positive control**: assert the run really did handle the secret (the
  target on disk equals it). A leak test that passes because the secret was never produced is worse
  than no test.

## Specific traps, each of which has bitten

- **Never `#[derive(Debug)]` on a type holding secret bytes.** Hand-write it to print `<N bytes>`.
  No test can see this exit, because nothing formats the struct today — which is the argument for
  removing it by construction rather than testing for it.
- **`CommandError::NonZeroExit` carries stdout.** Its `Display` omits the field; its `Debug` does
  not, so a `{:?}` on any `Result` holding one prints the command's whole output. Use `to_string()`,
  never `{:?}`. `CommandFailure::ExecutionFailed` deliberately has **no** `stdout` field so that the
  conversion has nowhere to put it, and forwards stderr only, truncated — do not add one back. A
  provider's stdout _is_ the secret, and the general failure path cannot know which commands produce
  one. Still prefer the resolve path's own error type over `OperationFailure::from(CommandError)` or
  `command_failed`: those say "a command failed", not which entry or which var, and the resolve
  variants carry that.
- **Forward command stderr on failure only**, truncated. It is content selfie does not control; a
  provider run with a verbose flag can echo secret material there. `truncate_stderr` in
  `commands/runner.rs` is the one bound; call it rather than deciding a limit per site.
- **Streamed command output is unconditional egress to every adapter.** `execute_command_streaming`
  sends each line of an install command's stdout **and** stderr as `PackageEvent::Info`, on success
  and on failure alike. The CLI prints those verbatim and the MCP server serializes them into its
  JSON. Nothing on that path is truncated, redacted, or conditional, which makes it broader than any
  other exit named in this file. A dotfile provider does not use it — the resolve path runs its own
  non-streaming execution — but any install command that echoes a credential is exposed by it.
- **`auto_accept` must not apply to secret-bearing entries.** Their conflicts are always reported
  and skipped without an interactive resolver. Default-false is not the same as cannot-be-set-true,
  and MCP exposes it as a caller-settable parameter.

## Paths and permissions

- **Never canonicalize a target path.** `write_file_private`'s symlink guarantee applies to the path
  _as given_. Canonicalization forfeits it exactly when it **succeeds**: `canonicalize` resolves the
  link and hands the writer the destination, so the writer sees an ordinary file and the guarantee
  is gone. A path that fails to canonicalize — a dangling link — is the _safe_ input, because it
  reaches the writer unresolved. Do not read this backwards; it has been stated backwards before.
- **`expand_target_path` is on the credential path.** Both repository-file and secret-bearing
  entries go through it (selfie-4m9 unified them by bringing the repository path up to the secret
  path's behavior, never the reverse). It must never resolve the **final component**, and never
  canonicalize the path as a whole — for any caller, for any reason. Duplicate detection comparing
  unresolved paths is a known cost and is not a reason to. It does call `expand_path` on a leading
  `~` by itself, which is deliberate: for any target with a component after the `~`, that cannot
  reach the last one. (A bare `~` or `~/` is the whole path, so it does — those are directories and
  fail the write anyway.) Read that call as the boundary, not as license to widen it. Only prose
  separates the two paths now; selfie-zv4b tracks making it a `TargetPath` newtype so the compiler
  does instead.
- **Secret targets are written with `write_file_private`, never `write_file`,
  `write_file_no_follow`, or any other writer** — temp file in the target's own directory at mode
  `0600`, then rename. Symlink-safe is **not** the same as owner-only: `write_file_no_follow` also
  refuses to follow a link, and creates at `0666 & ~umask`. Refusing to follow a link is the weaker
  of the two properties a credential needs.
- **Owner-only means `& 0o077 == 0`, not `& 0o007`.** Group-readable leaks a credential to exactly
  the people you are hiding it from on a shared machine.
- **All package-relative source paths go through `crate::paths` containment.** This guard has been
  re-established in this repository three times — `fd494e1`, PRs #30/#34, and the dotfiles work. It
  is lexical and does not follow symlinks; say that rather than claiming more.
