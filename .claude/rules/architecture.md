---
paths:
  - "crates/**/*.rs"
  - "crates/**/Cargo.toml"
---

# Architecture and Rust gotchas

## Design patterns

Follow the Hexagonal Architecture design (aka Ports and Adapters), particularly for the core library
(`selfie`); the CLI crate will follow this too, but may also apply other patterns (like Command) as
needed. Hexagonal design usually means using generics and monomorphism in the library (`selfie`),
and generics (`&impl Trait`) in the calling crates (`selfie-cli`). Async trait methods use RPITIT
(`fn method() -> impl Future + Send`) for zero-cost `Send` bounds, which makes traits
non-dyn-compatible; this is an intentional tradeoff — `impl Trait` parameters give the same
flexibility for testing (any concrete type satisfying the bound works) without heap allocation.

Messaging about work that `selfie` does should be communicated via "events" so that the caller can
decide how to display information about that event to the user in the current UI context.

## Key abstractions

- **Ports (traits):** `PackageService`, `DotfileService`, `PackageRepository`, `CommandRunner`,
  `FileSystem`
- **Adapters:** `PackageServiceImpl<R, CR, G>`, `YamlPackageRepository<F>`, `ShellCommandRunner`,
  `RealFileSystem`
- **Event system:** Operations return `EventStream` (pinned Stream of `PackageEvent`). The library
  emits events via `EventSender`; the CLI consumes them via `EventProcessor` with custom handlers;
  the MCP server consumes them via `event_collector::collect_events`, which converts a stream into
  an `EventCollectorResult` for structured JSON.
- **Progress:** `ProgressTracker` provides step-based progress (e.g., "Installing package (2/5)").
- **Service orchestration:** `PackageServiceImpl::execute_operation_with_deps()` is the standard
  pattern — creates channel, spawns async task, returns stream.

## Boundary rules

- The `selfie` library must never write to stdout/stderr — all output goes through `PackageEvent`.
- The library must not shell out to a tool it does not declare. `save_package` ran `dprint fmt` on
  the file it had just written, which made formatting depend on the caller's `PATH` and mutated the
  file outside the `FileSystem` port it had just used — after `canonicalize` had resolved the very
  symlinks `write_file_no_follow` refused to follow.
- **Config is split by concern:** `SelfieConfig` (library) holds operational settings
  (`environment`, `package_directory`, `command_timeout`, `stop_on_error`, `max_concurrency`).
  `CliConfig` (CLI crate) wraps `SelfieConfig` and adds presentation settings (`verbose`,
  `use_colors`). The config file uses top-level keys for core settings and a `cli:` section for
  CLI-specific ones. Each frontend reads only its own section; the library ignores unknown keys.
- CLI and MCP server commands should call `SpecService` or `PackageService` methods, not use
  `PackageRepository` directly. Tests should exercise the same service interface that production
  code uses, with mocked repositories injected into `PackageServiceImpl`.
- CLI command handlers accept `&CliConfig`, which delegates core getters to `SelfieConfig`. Pass
  `config.selfie_config()` when calling into library service methods.
- **Event consumer tests** (e.g., `EventProcessor`) should construct `EventStream` directly via
  `stream::iter(vec![...])`, not spin up a real service. This avoids adapter dependencies.
- **Validation reads an entry's fields directly (`source()`, `command()`, `vars()`), never
  `content_source()`.** That answers apply's question — may this deploy — so gating a check on it
  lets one defect suppress every other diagnostic for the same entry.

## MCP server (`selfie-mcp`)

The MCP server (`crates/mcp-server/`) is a second driving adapter alongside the CLI.

**Not a difference: the shell.** Both adapters use `ShellCommandRunner::login_shell()` to source the
user's login profile — the MCP server because a GUI-launched process doesn't inherit terminal PATH,
the CLI because provider commands and install commands need the same environment however they are
invoked. This was listed here as an MCP-vs-CLI difference and never was one:
`create_package_service` has always used a login shell. The CLI builds its runner in
`commands::common::create_command_runner`, and `crates/cli/clippy.toml` disallows `new` and
`default_shell` crate-wide with **no** exemptions — so a non-login runner is a build error anywhere
in `selfie-cli`, including inside that function. `login_shell` is deliberately _not_ disallowed:
listing it would force an `#[expect]` on `create_command_runner`, and a function-wide `#[expect]`
would also suppress the lint on `new`/`default_shell` inside it, letting the deciding function
silently revert to `/bin/sh`. The enforced invariant is the shell, not the number of call sites.
`selfie`'s own tests and `test-common` still use `default_shell()` deliberately, wanting a
deterministic `/bin/sh` rather than the developer's profile.

Key differences from the CLI:

- Recovers `HOME` env var via `getpwuid` if not set (macOS GUI apps may not set it).
- Uses `event_collector::collect_events` to convert an `EventStream` into an `EventCollectorResult`
  for structured JSON.
- Status labels are AI-friendly (`"installed"`, `"not installed"`, `"error"`) rather than CLI log
  phrases (`"successfully"`, `"with failures"`).
- Tools call `SpecService`/`PackageService` as the CLI does — `selfie_spec_validate_all` goes
  through `SpecService::validate_all`. `selfie_dotfiles_list` reads the repository directly; that is
  a known deviation from the boundary rule above, not a pattern to copy.
- Tool descriptions are written to guide AI assistants — be specific about what's returned and when
  to use each tool (e.g., "Use this instead of calling selfie_spec_info repeatedly").

## Gotchas

- **`selfie-cli` is a binary crate**: No lib target, so `cargo test -p selfie-cli --lib` won't work.
  Use `cargo test -p selfie-cli` instead.
- **`uuid` is a workspace dep**: Declared in the root `Cargo.toml` with `serde` + `v4` features.
- **Rust 2024 edition**: All crates use `edition = "2024"`. This affects import syntax and some
  trait behavior.
- **`with_mocks` feature flag**: **Do not drop `features = ["with_mocks"]` from
  `crates/test-common/Cargo.toml:7`.** `cargo test -p selfie` compiles without a flag only because
  of that line; remove it and the build fails inside `test-common`, naming a crate you did not
  touch: `no associated function or constant named from_parts found for struct CommandOutput`. The
  feature gates both the `mockall`-generated mocks and `CommandOutput::from_parts`
  (`crates/selfie/src/commands/runner.rs:190`). `test-common` needs it for `from_parts`, which its
  fake runner calls (`crates/test-common/src/runner.rs:194`), not for the mocks, which it never
  uses. Because `test-common` is in turn a `[dev-dependencies]` entry of `selfie` itself
  (`crates/selfie/Cargo.toml:41`), Cargo unifies the feature into `selfie`'s own test build.
  `selfie-cli` enables it separately for its own tests. **This cannot be replaced by `cfg(test)`:
  `test-common` is a separate crate, and `cfg(test)` is not set when it is compiled as a
  dependency.** `from_parts` is deliberately gated on the feature alone, where most `automock` sites
  use `any(test, ...)`: widening it would let production code fabricate the result of a command that
  never ran (see its doc comment).
- **Workspace dependencies**: Common deps (`tokio`, `console`, `tracing`, etc.) are defined in the
  root `Cargo.toml` under `[workspace.dependencies]` and referenced with `.workspace = true`.
- **`which` crate vs shell builtins**: `is_command_available` uses the `which` crate for native PATH
  lookup. It finds filesystem executables only — not shell builtins like `cd` or `test`. This is
  intentional: selfie checks for package manager binaries (`brew`, `npm`, `apt`).
