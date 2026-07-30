---
paths:
  - "crates/selfie/src/dotfile_service/**/*.rs"
  - "crates/selfie/src/package/event.rs"
  - "crates/selfie/src/package/service/audit.rs"
  - "crates/selfie/src/commands/runner.rs"
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

- Scan every emitted `PackageEvent`'s `Debug` output for the secret — every event, not one field of
  one variant. A leak added to a warning or a deployed event is how this actually happens.
- **Scan for the secret as text _and_ as a byte array.** A credential renders both ways and the two
  share no characters: `Debug` on `Vec<u8>` prints `[115, 51, ...]`, and `std::process::Output`
  switches to exactly that as soon as the content is not valid UTF-8 — which selfie supports. serde
  does the same in the MCP server's JSON. A scan for the literal alone passes a leak of the whole
  credential, and did, in every leak test in this repository. `test_common::assert_secret_free` is
  the one scan; use it for an event `Debug`, a tracing buffer, or a serialized payload alike. It
  matches a window of the secret rather than the whole value, so a truncating leak fails too. It is
  **two** windows, not one, and they are not the same size: 12 _bytes_ from the secret's first
  non-whitespace byte for the byte form, taken before normalization and keeping interior whitespace;
  and 12 _characters_ for the text form, taken after it. A leak-test secret therefore has to clear
  both — high-entropy, and for a **textual** secret still 12 characters once whitespace is stripped,
  which `"abc def ghi j"` and four emoji both fail. It must also not read like a path, a package
  name, or an environment name. Too short and the helper refuses it rather than scanning weakly.
- Install a **capturing `tracing` subscriber** and assert its output is secret-free too. Event
  scanning catches library logging only because `EventSender` mirrors log calls into events; a bare
  `tracing::debug!` bypasses that entirely. Assert the captured buffer is non-empty, or the test
  passes by capturing nothing.
- Give every leak test a **positive control**: assert the run really did handle the secret (the
  target on disk equals it). A leak test that passes because the secret was never produced is worse
  than no test. A control asserts the secret **is** present, so leave it a plain `contains` —
  `assert_secret_free` is its negation and converting one inverts the test silently, which is easy
  to do when the converted assertion sits a few lines away and the helper looks like the house
  idiom. The two also constrain the secret from opposite directions: a newline-leading value
  satisfies `assert_secret_free` but breaks a control that compares against the target on disk,
  because `Debug` escapes those newlines.

## Specific traps, each of which has bitten

- **Never `#[derive(Debug)]` on a type holding secret bytes.** Hand-write it to print `<N bytes>`.
  No test can see this exit, because nothing formats the struct today — which is the argument for
  removing it by construction rather than testing for it. The exception is `BoundedText`
  (`commands/runner.rs`), which holds text selfie **forwards** rather than content it holds back.
  Its `Debug` is derived on purpose: blinding it would contain nothing, since the text is already
  bound for the terminal, and it would hide forwarded stderr from the event scan above — a secret
  arriving on stderr later would go unseen instead of caught. Apply this rule to types that hold a
  secret, not to types that carry something already on its way out.
- **No command's stdout may reach a failure type.** A provider's stdout _is_ the secret, and the
  general failure path cannot know which commands produce one. This is currently enforced by
  construction at both ends: **no `CommandError` variant carries a `stdout` field**, so
  `OperationFailure::from(CommandError)` has nothing to read, and `CommandFailure::ExecutionFailed`
  has none either, so it would have nowhere to put it. Adding a `stdout` field to either — or a new
  `CommandError` variant that has one — reopens the path and needs a leak test again. Render a
  `CommandError` with `to_string()`, never `{:?}`: `Display` is what each variant's `#[error(...)]`
  curates, while `Debug` prints whatever fields a future variant adds. Still prefer the resolve
  path's own error type over `from(CommandError)` or `command_failed`: those say "a command failed",
  not which entry or which var, and the resolve variants carry that.
- **Forward command stderr on failure only**, bounded. It is content selfie does not control; a
  provider run with a verbose flag can echo secret material there, and a rendered `CommandError`
  embeds the package file's own unbounded `command:` string. `BoundedText` in `commands/runner.rs`
  is the one bound. It is `pub` and re-exported as `selfie::commands::BoundedText`, so the CLI and
  the MCP server can construct one — this rule is followable from every crate it is scoped to. Call
  `BoundedText::bound` rather than deciding a limit per site. What it _enforces_ is uneven, and the
  difference is the part worth knowing:
  - **`CommandFailure::ExecutionFailed`'s `stderr` is the only compiler-enforced site.** Its type is
    `BoundedText`, whose field is private, so no struct-variant literal — library, adapter, or test
    — can put unbounded text there. This is the one place the bound is not a convention.
  - **`AuditResult::Error` and `ResolveError`'s `stderr` fields are `String`.** They build a
    `BoundedText` at the construction site and unwrap it. Each is a rendered sentence rather than a
    stderr field, so typing them would claim the whole message is untrusted when only its tail is.
    The bound there is still something a person has to remember; these are the sites to check when
    adding a failure exit, and `audit.rs` is the one that was already missing it.
  - **The bound counts input bytes, not the length of the string it returns.** Invalid UTF-8 decodes
    lossily and each bad byte becomes a 3-byte `U+FFFD`, so 2000 bytes of binary stderr yield about
    6000 bytes of text. It bounds how much of the command's output survives, not `as_str().len()`.
  - **It keeps both ends, not a prefix.** The surviving 2000 bytes are split between the head and
    the tail, with the elided byte count named in between, because a failing command puts its
    diagnosis last. That means **the last bytes of stderr are always forwarded** — a leak test that
    plants its secret at the end will now see it where a head-only cut would have dropped it.
  - **It bounds forwarded output, not every string in a failure.** `ExecutionFailed`'s `command` and
    `AuditResultData`'s `audit_command` are plain unbounded `String`s that also reach the MCP JSON.
    They are user-authored package-file text rather than process output, which is why they are not
    bounded — do not read the bullet above as a claim that they are.
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
