## Your Role

You are an expert in Rust software development across multiple operating systems, system
administration, configuration management, and command-line interfaces. Your job is to help implement
a CLI tool with separate backing library, called "selfie-cli" and "selfie", respectively, written in
Rust, that can help me (and other users) manage packages in environments across multiple machines
and operating systems.

## Commands

```bash
cargo build                    # Build all crates
cargo test                     # Run all tests
cargo test -p selfie           # Test library only
cargo test -p selfie-cli       # Test CLI only
cargo run -- <args>            # Run the CLI (from workspace root)
cargo clippy --all-targets     # Lint
cargo fmt --check              # Check formatting
dprint fmt                     # Format Markdown/YAML (CI checks this)
dprint check                   # Verify Markdown/YAML formatting
```

### Pre-commit checklist

Before every commit (unless instructed otherwise), run all four and fix any issues:

1. `cargo fmt` — auto-fix formatting
2. `dprint fmt` — auto-fix Markdown/YAML formatting
3. `cargo clippy --all-targets` — fix all warnings (zero warnings policy)
4. `cargo test` — all tests must pass

### Documentation rule

When adding user-facing features, update `docs/` before considering the feature complete:

- `docs/package-files.md` — New YAML fields or behaviors
- `docs/configuration.md` — New config settings
- `README.md` — Status section and examples

When testing the CLI crate, enable mocks: the `selfie` dev-dependency already uses
`features = ["with_mocks"]`.

## Guidelines

When generating code, use Rust's `stdlib` when possible, `tokio` when async makes sense, and common
third-party libraries. Use the `console` and `dialoguer` crates for working with stdout/stderr/the
console. Use the `tracing` crate for logging. Use `clap` for CLI and argument parsing. Use `anyhow`
and `thiserror` for error handling. Use `assert_cmd` and `mockall` for unit testing; use
`testcontainers` for integration testing. Always use the latest versions of Rust and libraries.

Don’t implement any backward compatibility when changing existing code. Reuse existing code when
possible. Keep the codebase DRY and lean toward following the KISS principle. Lean towards using
third party libraries for substantial features and functionality, so we can keep the codebase small.

When you write tests for cli commands, lean on the Hexagonal ability to mock out interfaces. We
shouldn't be running commands in tests that alter our development environment.

## Project Organization

There are multiple crates in the repo, all under the `crates/` subdirectory:

1. `selfie-cli` (in `cli/`) which is the main UI (a CLI) for selfie,
2. `selfie` which is the library containing the core logic for selfie,
3. `selfie-mcp` (in `mcp-server/`) which is an MCP server exposing selfie to AI assistants,
4. `test-common` which are helper types and functions to use in tests (since setting up for testing
   often requires the same type of set up).

The CLI and MCP server are both "driving adapters" in hexagonal terms — they consume the same
`PackageService` trait but present results differently (terminal output vs structured JSON). Keep
logic in the `selfie` library; in general, `selfie` shouldn't write to stdout/stderr because it
doesn't know if it will be called from a GUI, a TUI, a CLI app, an MCP server, or even from some
other language.

Additionally, `assets/branding` contains logos and icons can be used in documentation and such.

## Design Patterns

Follow the Hexagonal Architecture design (aka Ports and Adapters), particularly for the core library
(`selfie`); the CLI crate will follow this too, but may also apply other patterns (like Command) as
needed. Hexagonal design usually means using generics and monomorphism in the library (`selfie`),
and generics (`&impl Trait`) in the calling crates (`selfie-cli`). Async trait methods use RPITIT
(`fn method() -> impl Future + Send`) for zero-cost `Send` bounds, which makes traits
non-dyn-compatible; this is an intentional tradeoff — `impl Trait` parameters give the same
flexibility for testing (any concrete type satisfying the bound works) without heap allocation.

Messaging about work that `selfie` does should be communicated via "events" so that the caller can
decide how to display information about that event to the user in the current UI context.

### Key Abstractions

- **Ports (traits):** `PackageService`, `DotfileService`, `PackageRepository`, `CommandRunner`,
  `FileSystem`
- **Adapters:** `PackageServiceImpl<R, CR>`, `YamlPackageRepository<F>`, `ShellCommandRunner`,
  `RealFileSystem`
- **Event system:** Operations return `EventStream` (pinned Stream of `PackageEvent`). The library
  emits events via `EventSender`; the CLI consumes them via `EventProcessor` with custom handlers;
  the MCP server consumes them via `McpEventCollector` which converts events to structured JSON.
- **Progress:** `ProgressTracker` provides step-based progress (e.g., "Installing package (2/5)").
- **Service orchestration:** `PackageServiceImpl::execute_operation_with_deps()` is the standard
  pattern — creates channel, spawns async task, returns stream.
- **Post-save formatting:** `YamlPackageRepository::save_package()` runs `dprint fmt` on the saved
  file as a best-effort post-processing step. Silently skipped if `dprint` is not installed.

### Boundary Rules

- The `selfie` library must never write to stdout/stderr — all output goes through `PackageEvent`.
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

### MCP Server (`selfie-mcp`)

The MCP server (`crates/mcp-server/`) is a second driving adapter alongside the CLI. Key differences
from the CLI:

- Uses `ShellCommandRunner::login_shell()` (not `default_shell()`) to source the user's login
  profile, since GUI-launched processes don't inherit terminal PATH.
- Recovers `HOME` env var via `getpwuid` if not set (macOS GUI apps may not set it).
- Uses `McpEventCollector` (in `event_collector.rs`) to convert `EventStream` into structured JSON.
- Status labels are AI-friendly (`"installed"`, `"not installed"`, `"error"`) rather than CLI log
  phrases (`"successfully"`, `"with failures"`).
- Bulk tools (`get_all_specs`, `validate_all`) bypass the service layer for fast file reads.
- Tool descriptions are written to guide AI assistants — be specific about what's returned and when
  to use each tool (e.g., "Use this instead of calling selfie_spec_info repeatedly").

## Gotchas

- **`selfie-cli` is a binary crate**: No lib target, so `cargo test -p selfie-cli --lib` won't work.
  Use `cargo test -p selfie-cli` instead.
- **`uuid` is a workspace dep**: Declared in the root `Cargo.toml` with `serde` + `v4` features.
- **Rust 2024 edition**: All crates use `edition = "2024"`. This affects import syntax and some
  trait behavior.
- **`with_mocks` feature flag**: The `selfie` crate exposes `mockall`-generated mocks behind
  `features = ["with_mocks"]`. The CLI's dev-dependencies already enable this.
- **Workspace dependencies**: Common deps (`tokio`, `console`, `tracing`, etc.) are defined in the
  root `Cargo.toml` under `[workspace.dependencies]` and referenced with `.workspace = true`.
- **`which` crate vs shell builtins**: `is_command_available` uses the `which` crate for native PATH
  lookup. It finds filesystem executables only — not shell builtins like `cd` or `test`. This is
  intentional: selfie checks for package manager binaries (`brew`, `npm`, `apt`).

## `selfie` Concepts

We're incrementally implementing this functionality. selfie is a personal meta-package manager: it
doesn't install packages directly, it runs whatever commands the user configures per package. It's a
glorified command runner, scoped to user-defined environments.

### Packages

Package files are YAML, represented by `selfie::package::Package`. Each package file defines
per-environment install and check commands. Packages may also declare `dotfiles` (dotfile mappings
deployed via `selfie apply`), `post_install_note` (first-install guidance), and per-environment
`recommends` (soft dependencies that warn on failure instead of failing the parent). Example:
`bash-language-server` might use Homebrew on macOS and `npm` on Ubuntu -- the user decides per
environment, then just runs `selfie install bash-language-server` regardless of which machine
they're on.

Package operations:

- **Validate**: Check that a package file follows the spec.
- **Check**: Run the user-defined check command to see if a package is installed.
- **Audit**: Run the user-defined audit command to detect installation sources and conflicts.
- **List**: List all YAML files in the configured package directory.
- **Create / Edit / Info / Update / Remove**: CRUD for package files in `package_directory`.
- **Apply**: Deploy dotfiles defined in a package's `dotfiles` field to their target locations.

### Environments

An environment is an arbitrary user-chosen label (typically per OS/distro). Package files have
`environment` sections tying install/check commands to these labels. The user sets their current
environment in config so selfie knows which commands to run.

### Configuration

Config file: `~/.config/selfie/config.yml`. Also settable via CLI flags.

Core settings (top-level, read by `SelfieConfig`):

- `environment`: The current environment label.
- `package_directory`: Directory containing selfie package files.
- `dotfiles_directory`: Directory containing dotfile source files for `selfie apply`.
- `state_directory`: Directory for deploy state tracking (checksums, drift detection).
- `command_timeout`, `stop_on_error`, `max_concurrency`: Execution settings.

CLI settings (under `cli:` section, read by `CliConfig`):

- `verbose`: Enable debug logging.
- `use_colors`: Enable colored terminal output.

<!-- BEGIN BEADS INTEGRATION v:1 profile:minimal hash:ca08a54f -->
## Beads Issue Tracker

This project uses **bd (beads)** for issue tracking. Run `bd prime` to see full workflow context and commands.

### Quick Reference

```bash
bd ready              # Find available work
bd show <id>          # View issue details
bd update <id> --claim  # Claim work
bd close <id>         # Complete work
```

### Rules

- Use `bd` for ALL task tracking — do NOT use TodoWrite, TaskCreate, or markdown TODO lists
- Run `bd prime` for detailed command reference and session close protocol
- Use `bd remember` for persistent knowledge — do NOT use MEMORY.md files

## Session Completion

**When ending a work session**, you MUST complete ALL steps below. Work is NOT complete until `git push` succeeds.

**MANDATORY WORKFLOW:**

1. **File issues for remaining work** - Create issues for anything that needs follow-up
2. **Run quality gates** (if code changed) - Tests, linters, builds
3. **Update issue status** - Close finished work, update in-progress items
4. **PUSH TO REMOTE** - This is MANDATORY:
   ```bash
   git pull --rebase
   bd dolt push
   git push
   git status  # MUST show "up to date with origin"
   ```
5. **Clean up** - Clear stashes, prune remote branches
6. **Verify** - All changes committed AND pushed
7. **Hand off** - Provide context for next session

**CRITICAL RULES:**
- Work is NOT complete until `git push` succeeds
- NEVER stop before pushing - that leaves work stranded locally
- NEVER say "ready to push when you are" - YOU must push
- If push fails, resolve and retry until it succeeds
<!-- END BEADS INTEGRATION -->
