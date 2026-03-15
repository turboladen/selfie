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
```

### Pre-commit checklist

Before every commit (unless instructed otherwise), run all three and fix any issues:

1. `cargo fmt` — auto-fix formatting
2. `cargo clippy --all-targets` — fix all warnings (zero warnings policy)
3. `cargo test` — all tests must pass

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
3. `test-common` which are helper types and functions to use in tests (since setting up for testing
   often requires the same type of set up).

Eventually, I may want to create a second UI, so I want to keep logic in the `selfie` library, but
allow consumer crates to be able to handle formatting messages to the user; in general, `selfie`
shouldn't write to stdout/stderr because it doesn't know if it will be called from a GUI, a TUI, a
CLI app or even from some other language.

Additionally, `assets/branding` contains logos and icons can be used in documentation and such.

## Design Patterns

Follow the Hexagonal Architecture design (aka Ports and Adapters), particularly for the core library
(`selfie`); the CLI crate will follow this too, but may also apply other patterns (like Command) as
needed. Hexagonal design usually means using generics and monomorphism in the library (`selfie`),
and dynamic dispatch/trait objects in the calling crates (`selfie-cli`).

Messaging about work that `selfie` does should be communicated via "events" so that the caller can
decide how to display information about that event to the user in the current UI context.

### Key Abstractions

- **Ports (traits):** `PackageService`, `PackageRepository`, `CommandRunner`, `FileSystem`
- **Adapters:** `PackageServiceImpl<R, CR>`, `YamlPackageRepository<F>`, `ShellCommandRunner`,
  `RealFileSystem`
- **Event system:** Operations return `EventStream` (pinned Stream of `PackageEvent`). The library
  emits events via `EventSender`; the CLI consumes them via `EventProcessor` with custom handlers.
- **Progress:** `ProgressTracker` provides step-based progress (e.g., "Installing package (2/5)").
- **Service orchestration:** `PackageServiceImpl::execute_operation_with_deps()` is the standard
  pattern — creates channel, spawns async task, returns stream.

### Boundary Rules

- The `selfie` library must never write to stdout/stderr — all output goes through `PackageEvent`.
- **Config is split by concern:** `SelfieConfig` (library) holds operational settings (`environment`,
  `package_directory`, `command_timeout`, `stop_on_error`, `max_parallel_installations`). `CliConfig`
  (CLI crate) wraps `SelfieConfig` and adds presentation settings (`verbose`, `use_colors`). The
  config file uses top-level keys for core settings and a `cli:` section for CLI-specific ones.
  Each frontend reads only its own section; the library ignores unknown keys.
- CLI commands should call `PackageService` methods, not use `PackageRepository` directly. This
  applies to both production code and tests — CLI tests should exercise the same service interface
  that production code uses, with mocked repositories injected into `PackageServiceImpl`.
- CLI command handlers accept `&CliConfig`, which delegates core getters to `SelfieConfig`. Pass
  `config.selfie_config()` when calling into library service methods.
- **Event consumer tests** (e.g., `EventProcessor`) should construct `EventStream` directly via
  `stream::iter(vec![...])`, not spin up a real service. This avoids adapter dependencies.

## Gotchas

- **`selfie-cli` is a binary crate**: No lib target, so `cargo test -p selfie-cli --lib` won't
  work. Use `cargo test -p selfie-cli` instead.
- **`uuid` is not a workspace dep**: It's declared directly in `crates/selfie/Cargo.toml`. If CLI
  tests need to construct `OperationInfo`, add `uuid` as a dev-dep to the CLI crate.
- **Rust 2024 edition**: All crates use `edition = "2024"`. This affects import syntax and some
  trait behavior.
- **`with_mocks` feature flag**: The `selfie` crate exposes `mockall`-generated mocks behind
  `features = ["with_mocks"]`. The CLI's dev-dependencies already enable this.
- **Workspace dependencies**: Common deps (`tokio`, `console`, `tracing`, etc.) are defined in the
  root `Cargo.toml` under `[workspace.dependencies]` and referenced with `.workspace = true`.
- **No circular dependency detection**: Install follows deps linearly without cycle detection.
  Tracked in beads.
- **`which` crate vs shell builtins**: `is_command_available` uses the `which` crate for native
  PATH lookup. It finds filesystem executables only — not shell builtins like `cd` or `test`.
  This is intentional: selfie checks for package manager binaries (`brew`, `npm`, `apt`).

## `selfie` Concepts

We're incrementally implementing this functionality. selfie is a personal meta-package manager: it
doesn't install packages directly, it runs whatever commands the user configures per package. It's
a glorified command runner, scoped to user-defined environments.

### Packages

Package files are YAML, represented by `selfie::package::Package`. Each package file defines
per-environment install and check commands. Example: `bash-language-server` might use Homebrew on
macOS and `npm` on Ubuntu -- the user decides per environment, then just runs
`selfie install bash-language-server` regardless of which machine they're on.

Package operations:
- **Validate**: Check that a package file follows the spec.
- **Check**: Run the user-defined check command to see if a package is installed.
- **List**: List all YAML files in the configured package directory.
- **Create / Edit / Info / Remove**: CRUD for package files in `package_directory`.

### Environments

An environment is an arbitrary user-chosen label (typically per OS/distro). Package files have
`environment` sections tying install/check commands to these labels. The user sets their current
environment in config so selfie knows which commands to run.

### Configuration

Config file: `~/.config/selfie/config.yml`. Also settable via CLI flags.

Core settings (top-level, read by `SelfieConfig`):
- `environment`: The current environment label.
- `package_directory`: Directory containing selfie package files.
- `command_timeout`, `stop_on_error`, `max_parallel_installations`: Execution settings.

CLI settings (under `cli:` section, read by `CliConfig`):
- `verbose`: Enable debug logging.
- `use_colors`: Enable colored terminal output.
