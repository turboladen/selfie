---
paths:
  - "crates/selfie/src/dotfile_service/**/*.rs"
  - "crates/selfie/src/package/event.rs"
  - "crates/selfie/src/fs/**/*.rs"
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
  not. Worse, `From<CommandError> for OperationFailure` moves that stdout into
  `CommandFailure::ExecutionFailed`, which reaches `PackageEvent::Completed`, is printed verbatim
  line-by-line by the CLI (`display_manager.rs`), and is serialized by MCP. A provider's stdout _is_
  the secret. Use `to_string()`, never `{:?}`, and never route a resolve failure through
  `OperationFailure::from(CommandError)` or `command_failed`.
- **Forward command stderr on failure only**, truncated. It is content selfie does not control; a
  provider run with a verbose flag can echo secret material there.
- **`auto_accept` must not apply to secret-bearing entries.** Their conflicts are always reported
  and skipped without an interactive resolver. Default-false is not the same as cannot-be-set-true,
  and MCP exposes it as a caller-settable parameter.

## Paths and permissions

- **Never canonicalize a secret target path.** `write_file_private`'s symlink guarantee applies to
  the path _as given_, and canonicalization forfeits it precisely when a symlink is planted, because
  `canonicalize` only succeeds for a path that already exists. The secret path uses its own
  non-canonicalizing `expand_secret_target` for this reason; do not "unify" it with
  `expand_user_path` (selfie-4m9).
- **Secret targets are written with `write_file_private`, never `write_file`** — temp file in the
  target's own directory at mode `0600`, then rename.
- **Owner-only means `& 0o077 == 0`, not `& 0o007`.** Group-readable leaks a credential to exactly
  the people you are hiding it from on a shared machine.
- **All package-relative source paths go through `crate::paths` containment.** This guard has been
  re-established in this repository three times — `fd494e1`, PRs #30/#34, and the dotfiles work. It
  is lexical and does not follow symlinks; say that rather than claiming more.
