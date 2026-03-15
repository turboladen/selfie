# Display Layer Redesign with indicatif

## Context

The CLI's current output layer (`TerminalProgressReporter`) grew organically and feels cobbled
together — inconsistent formatting across commands, emoji clutter, no animated progress, errors lost
in noise, and no parallel-safe output. The library's event system (`EventStream`/`PackageEvent`) is
solid, but the rendering side needs a ground-up replacement.

This redesign replaces the rendering layer with an `indicatif`-powered `DisplayManager` that
provides spinners, progress bars, parallel-safe output via `MultiProgress`, and structured error
summaries — while leaving the library crate completely untouched.

**Goal:** Cargo-level information density with richer, modern presentation. Errors should be
debuggable — a user should be able to copy the output and report a bug.

## Design

### Architecture

The event flow stays the same:

```
Library emits PackageEvent → EventProcessor routes → DisplayManager renders
```

**`DisplayManager`** replaces `TerminalProgressReporter`. It has two layers of API:

1. **Dynamic output** (event-driven commands): spinners, progress bars, rolling command output
2. **Static output** (non-event commands like `edit`, `remove`): simple `print_info()`,
   `print_error()`, `print_success()`, etc. — styled text lines with no spinners.

Internal structure:

- Owns an `indicatif::MultiProgress` instance (already thread-safe / `Clone`)
- Creates/manages spinners and progress bars per operation via `OperationHandle`
- Handles style templates for consistent look
- Collects errors via `ErrorCollector` for end-summary
- Two modes: **normal** (compact, spinners, progress bars) and **verbose** (stream everything)
- Non-TTY detection: when stdout is not a TTY, uses `ProgressDrawTarget::hidden()` and falls back to
  plain `eprintln!` for status lines (no control characters in piped/CI output)

**`EventProcessor`** changes minimally:

- Holds `DisplayManager` instead of `TerminalProgressReporter`
- Default handlers call `DisplayManager` methods
- Custom per-command handler pattern (`FnMut(&PackageEvent) -> bool`) unchanged
- Custom handlers receive a `&DisplayManager` reference (safe because `MultiProgress` and
  `ProgressBar` are internally `Arc`-wrapped — no borrow conflicts)

**`StatusStyle`** (small module) replaces static formatting methods:

- Contains `format_installed()`, `format_not_installed()`, `format_no_check()`, etc.
- Used by table-rendering code in `list.rs`, `info.rs`, `validate.rs`, `tables.rs`
- Pure functions — no spinner/progress dependency

### Rendering Behavior

**Operation lifecycle on screen (event-driven commands):**

1. **Started** — spinner appears: `⠋ Installing bash-language-server...`
2. **Progress** — spinner updates: `⠙ Installing bash-language-server (2/7) Checking if installed…`
3. **Command output** (normal) — indented below spinner, last 3 lines visible, older lines scroll
   away. Verbose mode streams all lines.
4. **Success** — spinner clears → `✓ bash-language-server installed`
5. **Failure** — spinner clears → `✗ bash-language-server failed (see summary below)`

**Non-event commands (edit, remove, create):**

- Use `DisplayManager::print_info()`, `print_error()`, `print_success()`, etc.
- These are simple styled `eprintln!`/`println!` calls — no spinners involved
- Same visual style as spinner finish lines for consistency

**Parallel operations** (designed for now, used when parallel installs land):

- `MultiProgress` stacks spinners/bars vertically
- Each operation gets its own spinner line
- No interleaving — indicatif manages cursor

**Styling:**

- `indicatif::ProgressStyle` templates for consistency
- Minimal color: green (success), red (failure), yellow (warning), default (info)
- Status markers: `✓`, `✗`, `⚠` — no emoji overload
- Respects `use_colors` config (controls whether styles include color codes)
- Non-TTY: no ANSI codes, no spinner characters — plain prefixed text

**stdout vs stderr policy:**

- All spinner/progress output goes to **stderr** (via `MultiProgress` default)
- Structured data output (tables from `list`, `info`) goes to **stdout** via
  `MultiProgress::println()` to avoid visual corruption during active spinners
- Error summary goes to **stderr**

### Error Collection & Summary

**`ErrorCollector`** (owned by `DisplayManager`):

- Accumulates structured errors during operation
- Each error captures: package name, operation type, command run, exit code, stderr, stdout
- After `process_events` completes, if errors exist, `DisplayManager::finish()` prints summary
- `EventProcessingResult` still tracks `exit_code` and `had_errors` (unchanged) — `ErrorCollector`
  is purely for the display summary, not for control flow

```
── Errors ─────────────────────────────────────
✗ bash-language-server
  Command: brew install bash-language-server
  Exit code: 1
  stderr:
    Error: No available formula with the name "bash-language-server"
───────────────────────────────────────────────
```

- Summary is plain text — no control characters, copy-pasteable
- Prints in both normal and verbose modes
- `stop_on_error` config controls whether other operations continue after a failure

### What Changes

| Current                                            | Replacement                                     |
| -------------------------------------------------- | ----------------------------------------------- |
| `terminal_progress_reporter.rs` (entire file)      | `display_manager.rs` (new file)                 |
| `MessageType` enum                                 | Absorbed into `DisplayManager` methods          |
| Emoji constants + fallback logic                   | `indicatif` style templates + `✓`/`✗`/`⚠`       |
| Per-command ad-hoc formatting                      | Standardized through `DisplayManager`           |
| `EventProcessor.reporter` field                    | `EventProcessor.display` field                  |
| Scrolling `VecDeque` in install handler            | `DisplayManager` rolling output window          |
| Status formatting methods (format_installed, etc.) | `StatusStyle` module (used by tables/list/info) |
| `report_status()` in `common.rs`                   | `DisplayManager::print_status()`                |
| `create.rs` manual event loop                      | Normalized to use `EventProcessor`              |

### What Stays

- `EventProcessor` structure and routing logic
- Custom handler pattern (`FnMut(&PackageEvent) -> bool`)
- `EventStream`, `PackageEvent`, `EventSender` — library side untouched
- `ProgressTracker` — library side untouched
- `CliConfig` / `SelfieConfig` split
- `EventProcessingResult` (exit_code, had_errors)
- All library crate code — zero changes
- `formatters.rs` — `format_key()` is independent and small
- `comfy_table` usage — tables still render via `comfy_table`

### New Dependency

- `indicatif` added to workspace dependencies, used only by CLI crate
- Same author as `console` (mitsuhiko) — guaranteed compatible

### Testing Strategy

- **`DisplayManager` unit tests:** Use `indicatif::ProgressDrawTarget::hidden()` for tests —
  spinners/bars run but produce no output. Test state transitions (started → progress → completed)
  by verifying method calls don't panic and return expected state.
- **`StatusStyle` unit tests:** Pure formatting functions — test output strings directly (same
  pattern as current `TerminalProgressReporter` tests).
- **`EventProcessor` tests:** Existing pattern preserved — construct `EventStream` via
  `stream::iter(vec![...])`, inject `DisplayManager::new(false)` (colors off). Verify
  `EventProcessingResult` values.
- **`ErrorCollector` tests:** Verify error accumulation and summary formatting with plain text
  assertions.
- **Existing tests:** Port tests from `terminal_progress_reporter.rs` to `StatusStyle` and
  `DisplayManager`. Command handler tests update to use `DisplayManager` instead of
  `TerminalProgressReporter`.

## Files to Modify

**New files:**

- `crates/cli/src/display_manager.rs` — replaces `terminal_progress_reporter.rs`
- `crates/cli/src/status_style.rs` — static status formatting for tables

**Modified files:**

- `Cargo.toml` (workspace) — add `indicatif` to workspace deps
- `crates/cli/Cargo.toml` — add `indicatif = { workspace = true }`
- `crates/cli/src/main.rs` — update module declarations
- `crates/cli/src/event_processor.rs` — swap reporter for display manager
- `crates/cli/src/tables.rs` — use `StatusStyle` instead of `TerminalProgressReporter`
- `crates/cli/src/commands/package/install.rs` — use `DisplayManager`
- `crates/cli/src/commands/package/check.rs` — use `DisplayManager`
- `crates/cli/src/commands/package/list.rs` — use `DisplayManager` + `StatusStyle`
- `crates/cli/src/commands/package/validate.rs` — use `DisplayManager` + `StatusStyle`
- `crates/cli/src/commands/package/info.rs` — use `DisplayManager` + `StatusStyle`
- `crates/cli/src/commands/package/create.rs` — normalize to use `EventProcessor`
- `crates/cli/src/commands/package/edit.rs` — use `DisplayManager` static methods
- `crates/cli/src/commands/package/remove.rs` — use `DisplayManager` static methods
- `crates/cli/src/commands/package/common.rs` — use `DisplayManager`, remove `report_status()`

**Deleted files:**

- `crates/cli/src/terminal_progress_reporter.rs`

## Verification

1. `cargo build` — compiles cleanly
2. `cargo clippy --all-targets` — zero warnings
3. `cargo test` — all existing tests pass
4. Manual test: `cargo run -- install <package>` — observe spinner, progress, success/failure output
5. Manual test: `cargo run -- check <package>` — observe spinner and result
6. Manual test: trigger a failure — verify inline notice + error summary at end
7. Manual test: `--verbose` flag — verify full output streaming
8. Verify `use_colors: false` config disables color in output
9. Manual test: pipe output (`cargo run -- list 2>&1 | cat`) — verify no control characters
10. Manual test: `cargo run -- edit <package>` — verify static output still works
