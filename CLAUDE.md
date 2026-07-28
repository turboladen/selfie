## Your Role

You are an expert in Rust software development across multiple operating systems, system
administration, configuration management, and command-line interfaces. Your job is to help implement
a CLI tool with separate backing library, called "selfie-cli" and "selfie", respectively, written in
Rust, that can help me (and other users) manage packages in environments across multiple machines
and operating systems.

## Commands

```bash
cargo build                                    # Build all crates
cargo test                                     # Run all tests
cargo test -p selfie --features with_mocks     # Test library only (see below)
cargo test -p selfie-cli                       # Test CLI only
cargo run -- <args>                            # Run the CLI (from workspace root)
cargo clippy --all-targets -- -D warnings      # Lint, CI form (see below)
cargo fmt --check                              # Check formatting
dprint fmt                                     # Format Markdown/YAML (CI checks this)
dprint check                                   # Verify Markdown/YAML formatting
```

Two gate commands differ from the obvious form, and both let work pass locally that CI rejects:

- `cargo clippy --all-targets` **does not fail on warnings**. CI runs it with `-- -D warnings`
  (`.github/workflows/ci.yml`). Use the CI form locally or a warning ships to a red build.
- `cargo test -p selfie` **does not compile** — the mocks are behind `with_mocks`, so it fails on
  unresolved `MockFileSystem` / `MockPackageRepository` imports. Workspace `cargo test` works only
  because `selfie-cli`'s dev-dependencies unify the feature in. Tracked as selfie-4b7.

### Pre-commit checklist

Before every commit (unless instructed otherwise), run all four and fix any issues:

1. `cargo fmt` — auto-fix formatting
2. `dprint fmt` — auto-fix Markdown/YAML formatting
3. `cargo clippy --all-targets -- -D warnings` — zero warnings policy; the bare form under-reports
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

## Rules

Detailed instructions live in `.claude/rules/`, scoped by the files they apply to so they load when
they are relevant rather than in every session:

- `architecture.md` — hexagonal design, key abstractions, boundary rules, MCP server, Rust gotchas
- `domain.md` — what packages, environments and dotfiles are; configuration keys
- `secrets.md` — handling credential-bearing dotfile content; egress, permissions, path rules
- `testing.md` — why a green test proves little here, and the mutation practice that does
- `verification.md` — confirm before asserting; verify in a copy, never in a shared tree

`verification.md` loads every session; the rest load when you touch matching files. Read the
relevant one before working in an area rather than inferring the convention from nearby code.

<!-- BEGIN BEADS INTEGRATION v:1 profile:minimal hash:ca08a54f -->

## Beads Issue Tracker

This project uses **bd (beads)** for issue tracking. Run `bd prime` to see full workflow context and
commands.

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
- A problem found while doing other work goes in a **bead**, not a PR description or a code comment.
  A PR body is read once and buried; a bead is queryable and shows up in `bd ready`.

## Session Completion

**When ending a work session**, you MUST complete ALL steps below. Work is NOT complete until
`git push` succeeds.

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
