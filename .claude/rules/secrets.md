---
paths:
  - "crates/selfie/src/dotfile_service/**/*.rs"
  - "crates/selfie/src/package/event.rs"
  - "crates/selfie/src/package/service/audit.rs"
  - "crates/selfie/src/commands/runner.rs"
  - "crates/selfie/src/git/**/*.rs"
  - "crates/selfie/src/sync_service/**/*.rs"
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

- **A command whose stdout becomes content goes through `CommandRunner::execute_for_content`, never
  `execute_in_dir`.** The shell writes to the same stdout the command does — a profile banner, a
  background job it started, an exit trap — and `execute_in_dir` returns all of it, which on this
  path is foreign bytes in a credentials file with the deploy reporting success. Enforced by type:
  `execute_for_content` returns `ContentOutput`, `run_capture` accepts nothing else, and
  `ContentOutput` neither wraps nor exposes a `CommandOutput`. Do not restate the guarantee as
  "profile output is discarded" — the mechanism is a descriptor plus two markers, and what it cannot
  account for is on `ContentOutput::tail_verified` and in `docs/package-files.md`.
- **The capture descriptor is an egress: whatever holds it receives the credential.** A startup file
  doing `exec 8>~/debug.log` is handed the content verbatim, and selfie then captures nothing and
  fails closed — the user sees a failed apply, not a silent leak. What this defends against is an
  **accidental** collision, and the write-up must say so: a hostile profile owns the session and can
  read the deployed file or wrap `op` itself, so there is no boundary here to defend. Three
  constraints on the number, all load-bearing: it must be a single digit (`dash`, `zsh` and `ksh`
  reject 10 and above, and `dash` is `/bin/sh` on Linux); it must avoid 3, 4 and 9, which convention
  has already claimed; and it must stay **fixed**, because a collision that happens on every run is
  diagnosable and documentable, while one that happens on a fraction of runs gets retried instead of
  investigated. Do not read this as new exposure: before the descriptor existed, the content
  travelled on stdout, where `exec >somewhere` in a profile collected it every time.
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
- **Git's stderr is untrusted output too, and it has its own bound.** A remote URL can carry a
  credential in its userinfo, and a git that cannot prompt for a password names that URL in the
  failure — `could not read Password for 'http://<token>@host'`. That reaches an AI transcript
  through `selfie_sync_push` / `selfie_sync_pull`. `GitMessage` (`git/message.rs`) is the one place
  that text is cleaned, and like `CommandFailure::ExecutionFailed`'s `stderr` it is
  **compiler-enforced**: its field is private, and both `GitSyncError::OperationFailed`'s fields and
  `GitStatusError::StatusError` are typed as it, so no struct-variant literal anywhere can carry raw
  git output. Three things about it are worth knowing before extending it:
  - **It redacts both halves of the userinfo, not just the password.** A personal access token is
    normally the _username_: `https://ghp_…@host/repo.git`. `gix::Url`'s redacting `Display` blanks
    only the password — verified in `gix-url/src/impls.rs`, and warned about in that crate's own
    docs — so reaching for it here produces a fix that passes a `user:pass` test and leaks every
    token. Do not swap the hand-rolled redaction for it.
  - **Redaction runs before the bound, and the order is load-bearing.** `BoundedText` elides the
    middle; a cut falling inside a URL strands `https://user:TOK` in the kept head with no `@` left
    to anchor on, so bounding first can _manufacture_ a leak.
  - **It covers URLs, not credentials.** A token outside a userinfo — a `GIT_TRACE` header dump, a
    credential-helper echo — is not redacted, and deliberately so: matching known token prefixes is
    an allowlist that fails open for every provider not on it while looking complete. The precise
    uncovered set is on `redact_credentials`; keep it accurate rather than aspirational.
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
  unresolved paths is a known cost and is not a reason to.
- **`TargetPath` (`fs/target.rs`) is what enforces that, and it enforces exactly one half.**
  `write_file_private`, `write_file_no_follow`, `symlink_refusal` and `is_owner_only` take a
  `TargetPath` rather than a `&Path`, so a call site that canonicalizes and then writes fails to
  build. Be precise about what that buys, because the wrong summary invites the wrong fix:
  - The guarantee is **you cannot resolve a `TargetPath` once you hold one** — there is no `Deref`,
    so `Path`'s own `canonicalize`, `exists` and `metadata` are not _on_ the type. They are still
    one explicit `.path()` away; what the newtype removes is reaching them by autoderef. It is _not_
    "the path inside was never resolved" either: `expand_target_path` is `pub` and takes a `&str`,
    so feeding it a canonicalized path mints a `TargetPath` around one. Both gaps take deliberate
    steps instead of one accidental token, which is the whole gain.
  - It does **not** stop someone editing `expand_target_path` itself. That half is held by the
    expansion and refusal tests, and by the expander taking a `HomeDir` — a trait with one method,
    `home()` — instead of a `FileSystem`, so neither `canonicalize` nor `expand_path` is reachable
    from inside it. There is no longer a resolving call in that function to defend.
  - **A deploy or track path must obtain its target from `deploy_target`; `expand_target_path` is
    for callers that only compare or display one.** Three functions hand out a `TargetPath` and only
    two construct one — the count is of **functions**, not of `TargetPath { … }` literals, of which
    `state_file_path` holds two — and the three promise different things, so do not collapse them
    into a list:
    - `deploy_target` is the **strongest** and the only one a deploy or track path may use. It
      refuses `~user/…` and anything not absolute _before_ expanding, and re-checks absoluteness
      after — so a `~/…` that could not be expanded for want of a home directory is refused rather
      than written relative to the current directory. It mints nothing: it calls
      `expand_target_path` and adds the rule.
    - `expand_target_path` **mints**, and is unresolved by construction — it takes a `HomeDir`
      rather than a `FileSystem`, so neither `canonicalize` nor `expand_path` is reachable from
      inside it. It says nothing about whether the path can be _deployed to_: `~alice/.gemrc` comes
      back as the literal relative path. Callers that only compare or display a target use it
      deliberately.
    - `state_file_path` is the **weakest** — its configured branch joins `state_directory` exactly
      as given, so what comes back is only as unresolved as the caller was configured with — and it
      is documented as such where it lives.
- **Secret targets are written with `write_file_private`, never `write_file`,
  `write_file_no_follow`, or any other writer** — temp file in the target's own directory at mode
  `0600`, then rename. Symlink-safe is **not** the same as owner-only: `write_file_no_follow` also
  refuses to follow a link, and creates at `0666 & ~umask`. Refusing to follow a link is the weaker
  of the two properties a credential needs.
- **Owner-only means `& 0o077 == 0`, not `& 0o007`.** Group-readable leaks a credential to exactly
  the people you are hiding it from on a shared machine.
- **All package-relative source paths go through `crate::paths` containment.** This guard has been
  re-established in this repository three times — `fd494e1`, PRs #30/#34, and the dotfiles work. It
  is lexical and does not follow symlinks; say that rather than claiming more. All four call sites
  now state the limit, `is_within` carries the reasoning, and
  `a_symlinked_source_escapes_the_containment_guard` pins it as an executable fact — it asserts the
  escape **succeeds**, so strengthening the guard must delete the test and the prose together.
- **A symlinked dotfile target is refused, and every command says so the same way.** `apply` refuses
  the write, `dotfiles drift` reports the refusal wherever `apply` would refuse — gated on
  `deploy_decision`, the same function `apply` calls, so the parity is structural rather than
  asserted — and `dotfiles track` refuses to create the entry at all. Track's refusal sits ahead of
  every write it performs: tracking reads _through_ a link, so accepting one copies the destination
  into the dotfiles repository, where `sync push` commits it. Do not relax that ordering.
