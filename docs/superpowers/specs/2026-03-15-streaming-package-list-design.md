# Streaming Package List with Sorted Spinners

## Context

The `selfie list` command takes ~5 seconds to display results because it waits for all package
status checks to complete before rendering a table. Users see nothing during that time. The goal is
to show a sorted list of packages immediately, with individual spinners that resolve as status
checks complete — giving instant feedback while maintaining alphabetical order.

## Design

### User-facing behavior

**Immediately after running `selfie list`** (~100ms):

```
⠋ bash-language-server    v0.1.0   checking...
⠙ direnv                  v1.0.0   checking...
⠹ docker                  v0.1.0   checking...
⠸ fd                      v0.1.0   checking...
⠼ fzf                     v1.0.0   checking...
```

**As checks complete** (results fill in-place over ~5 seconds):

```
✓ bash-language-server    v0.1.0   Installed
✓ direnv                  v1.0.0   Installed
✗ docker                  v0.1.0   Not installed
⠹ fd                      v0.1.0   checking...
✓ fzf                     v1.0.0   Installed
```

Completed items stay in their original alphabetical position — they don't move. This is achieved by
using `bar.finish_with_message()` to update in-place rather than `finish_and_clear()` +
`mp.println()`.

**With `--all`** (adds environment list after status):

```
✓ bash-language-server    v0.1.0   Installed       (macos, ubuntu)
✓ direnv                  v1.0.0   Installed       (*macos, ubuntu)
✗ docker                  v0.1.0   Not installed   (macos)
```

**After all checks complete**, print the summary and any invalid packages:

```
📁 Package directory: /path/to/packages
72 valid, 25 invalid

⚠ Invalid package files:
  ✗ broken-pkg.yml: missing field `name`
  ✗ bad-config.yml: environments.macos: missing field
```

### Library changes

**New event: `PackageListReady`**

Emitted after loading, filtering, and sorting YAML files but before status checks begin. Contains
the **filtered** (post-environment-filter) sorted list — only packages that will receive
`PackageListItemCompleted` events. This ensures a 1:1 correspondence between spinners and
completions.

Reuses the existing `PackageListItem` struct with `status: None` to avoid introducing a new type:

```rust
PackageListReady {
    operation_info: OperationInfo,
    packages: Vec<PackageListItem>,  // status is None for all items
}
```

**Event flow (revised):**

1. `Started` — operation begins
2. `Progress` — "Loading packages" (step 1/2)
3. `PackageListReady` — sorted filtered package metadata, CLI renders spinner list
4. `Progress` — "Checking package status" (step 2/2)
5. `PackageListItemCompleted` — one per package, as each check finishes (already exists)
6. `PackageListLoaded` — final summary with valid/invalid counts and environment stats
7. `Completed` — operation done

**Ordering guarantee:** `PackageListLoaded` is always emitted after all `PackageListItemCompleted`
events (checks complete before summary is sent). The CLI can safely use raw `println!` for the
summary since all spinners are already finished.

**Progress steps**: reduced from 5 to 2 ("Loading packages", "Checking package status"). The spinner
list itself is the progress indicator.

**`list.rs` handler changes:**

- Remove the intermediate "Processing package information", "Sorting packages for streaming", and
  "Finalizing package list" progress steps
- Emit `PackageListReady` after filtering and sorting, before spawning check tasks
- Handle `JoinError` from spawned check tasks by emitting a `PackageListItemCompleted` with
  `CheckResult::Error` so spinners always resolve (never hang)
- The existing parallel check + `PackageListItemCompleted` streaming stays as-is

### CLI changes

**`list.rs` command handler:**

- On `PackageListReady`: create one `ProgressBar` per package via `DisplayManager`, stored in a
  `HashMap<String, ProgressBar>` keyed by package name. Each spinner shows
  `{name}  v{version}  checking...` (with `--all`: append environment list).
- On `PackageListItemCompleted`: look up the `ProgressBar` by package name, call
  `bar.finish_with_message(...)` to update in-place (preserves alphabetical position). Format the
  final message with `✓`/`✗`/`⚠` prefix based on `CheckResult`.
- On `PackageListLoaded`: print summary line and invalid packages list via `println!` (safe because
  all spinners are already finished).
- On `Progress`: suppress (spinners are the progress indicator).
- Drop the `comfy_table` table for the main package list. Keep it for the environment stats fallback
  (when no packages match the current environment).

**In-place spinner finish (critical for alphabetical ordering):**

The `OperationHandle` API needs a new method (or the list handler can work directly with
`ProgressBar`). The key is using `bar.finish_with_message(formatted_result)` instead of
`finish_and_clear()` + `mp.println()`. This keeps each bar in its original position so the final
output preserves alphabetical order. The spinner style template must include `{msg}` to support
this.

Use a **two-style approach**:

- Spinner style: `"{spinner:.cyan} {msg}"` — used while checking
- Finished style: `"{msg}"` — used after `finish_with_message()`, no spinner character

**Column alignment:**

Use fixed-width formatting based on the longest package name in the `PackageListReady` payload. This
is easy since we know all names upfront.

**`DisplayManager` changes:**

The static output methods (`print_info`, `print_error`, etc.) currently use `println!`/`eprintln!`
directly. Since `PackageListLoaded` (summary) always arrives after all spinners are finished, raw
`println!` is safe for this specific use case. However, to be safe for future use cases where
spinners may be active during static output, the methods should route through `MultiProgress` when
spinners are active. This can be done in a follow-up.

**Non-TTY / piped output:**

When not a TTY, `DisplayManager` already sets `ProgressDrawTarget::hidden()`. Spinners don't render,
but `finish_with_message()` produces no visible output with a hidden target. The summary output via
`println!` still works. For non-TTY, the handler should fall back to printing each result line
directly (no spinners), then the summary — providing clean text output suitable for piping.

### What changes

| Component                       | Change                                                                |
| ------------------------------- | --------------------------------------------------------------------- |
| `selfie` lib: `event.rs`        | Add `PackageListReady` variant to `PackageEvent`                      |
| `selfie` lib: `event.rs`        | Add `send_package_list_ready()` method to `EventSender`               |
| `selfie` lib: `service/list.rs` | Emit `PackageListReady`, reduce to 2 progress steps, handle JoinError |
| `selfie` lib: `service.rs`      | Update list total_steps from 5 to 2                                   |
| CLI: `list.rs`                  | Replace table rendering with spinner-per-package                      |
| CLI: `display_manager.rs`       | Add method to create bare `ProgressBar` for list use case             |
| CLI: `event_processor.rs`       | Add `PackageListReady` to the ignored-by-default structured events    |

### What stays

- `PackageListItemCompleted` event (already exists and streams correctly)
- `PackageListItem` struct (reused for `PackageListReady` with `status: None`)
- `PackageListLoaded` summary event
- `--all` flag behavior (filter vs show all environments)
- Environment stats table (fallback when no packages match current env)
- Invalid packages display (reformatted as simple list instead of table)

### Testing

- **Library**: test that `PackageListReady` is emitted before any `PackageListItemCompleted` events
- **Library**: test that `PackageListReady` contains only filtered packages (matching environment)
- **Library**: test that `JoinError` produces `CheckResult::Error` in `PackageListItemCompleted`
- **CLI**: test spinner creation count matches package count from `PackageListReady`
- **CLI**: test that `PackageListItemCompleted` resolves the correct spinner
- **Integration**: `cargo run -- list` displays spinners that resolve to results
- **Edge cases**: empty package directory, all invalid packages, `--all` with multiple environments,
  non-TTY output

## Verification

1. `cargo build` — compiles cleanly
2. `cargo clippy --all-targets` — zero warnings
3. `cargo test` — all tests pass
4. Manual: `cargo run -- list` — spinner list appears immediately, fills in over seconds
5. Manual: `cargo run -- list --all` — environments shown inline
6. Manual: pipe output — `cargo run -- list 2>&1 | cat` — no control characters in final output
7. Manual: verify alphabetical order preserved as checks complete out-of-order
