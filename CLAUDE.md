## What this is

A CLI (`selfie-cli`) over a backing library (`selfie`) for managing packages and dotfiles across
machines and operating systems.

## Commands

```bash
just check                                     # Run the pre-commit gates (see below)
just test-lib                                  # Test library only, the canonical form
cargo build                                    # Build all crates
cargo test                                     # Run all tests
cargo test -p selfie-cli                       # Test CLI only
cargo run -- <args>                            # Run the CLI (from workspace root)
cargo clippy --all-targets -- -D warnings      # Lint, CI form (see below)
cargo fmt --check                              # Check formatting
dprint fmt                                     # Format Markdown/YAML (CI checks this)
dprint check                                   # Verify Markdown/YAML formatting
```

Two gate commands come with a catch:

- `cargo clippy --all-targets` **does not fail on warnings**, so it lets work pass locally that CI
  rejects. CI runs it with `-- -D warnings` (`.github/workflows/ci.yml`). Use the CI form when
  running clippy directly; `just check` already does.
- `cargo test -p selfie` takes no feature flag — `Justfile:31` (`just test-lib`) is the canonical
  form. It compiles only because `crates/test-common/Cargo.toml:7` requests
  `features = ["with_mocks"]`. **Do not drop that line**, or the build fails inside `test-common`
  with an error naming a crate you did not touch. `.claude/rules/architecture.md` explains the
  mechanism. The command compiles today. It did not before PR #67, which is why older instructions
  prescribe a `--features with_mocks` flag that is now unnecessary.

### Pre-commit checklist

Before every commit (unless instructed otherwise), run `just check` and fix any issues. It runs
`cargo fmt`, `dprint fmt`, clippy with `-D warnings`, and the test suite, in that order, stopping at
the first failure. `Justfile` is the source of truth for these gates — do not retype the commands.

Passing it means the checklist passed, not that CI will be green — CI also runs `typos`,
`cargo build`, and every feature combination of `selfie` via `cargo hack`.

`dprint fmt` reformats every Markdown and YAML file in the repo, not just the ones you edited.
Commit that: unformatted files anywhere are a miss, and the fix belongs in whatever PR finds it.

### Documentation rule

When adding user-facing features, update `docs/` before considering the feature complete:

- `docs/package-files.md` — New YAML fields or behaviors
- `docs/configuration.md` — New config settings
- `README.md` — Status section and examples

Prose uses **US spelling** — behavior, serialized, normalization, judgment. `typos` in CI does not
catch British forms, because they are real words.

## Guidelines

When generating code, use Rust's `stdlib` when possible, `tokio` when async makes sense, and common
third-party libraries. Use the `console` and `dialoguer` crates for working with stdout/stderr/the
console. Use the `tracing` crate for logging. Use `clap` for CLI and argument parsing. Use `anyhow`
and `thiserror` for error handling. Use `assert_cmd` and `mockall` for testing. Always use the
latest versions of Rust and libraries.

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

A problem found while doing other work goes in a **bead**, not a PR description or a code comment. A
PR body is read once and buried; a bead is queryable and shows up in `bd ready`.

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
- A bead's DESIGN block is a note from a past session, not authority. Verify its claims against the
  tree and against this file before planning around them — one carried a dependency constraint that
  was wrong on both counts

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
