# Streaming Package List Implementation Plan

> **For agentic workers:** REQUIRED: Use superpowers:subagent-driven-development (if subagents
> available) or superpowers:executing-plans to implement this plan. Steps use checkbox (`- [ ]`)
> syntax for tracking.

**Goal:** Replace the wait-then-render table in `selfie list` with a sorted spinner list that
streams results as status checks complete.

**Architecture:** Library emits a new `PackageListReady` event with sorted package metadata before
checks begin. CLI creates one indicatif spinner per package, then resolves each in-place as
`PackageListItemCompleted` events arrive. Uses `bar.finish_with_message()` to preserve alphabetical
ordering.

**Tech Stack:** Rust 2024, indicatif (MultiProgress/ProgressBar), tokio, selfie event system

**Spec:** `docs/superpowers/specs/2026-03-15-streaming-package-list-design.md`

---

## Chunk 1: Library — Add `PackageListReady` Event

### Task 1: Add `PackageListReady` variant to `PackageEvent`

**Files:**

- Modify: `crates/selfie/src/package/event.rs:1221-1244` (add variant before `PackageListLoaded`)
- Modify: `crates/selfie/src/package/event.rs:338-345` (add `send_package_list_ready` method)

- [ ] **Step 1: Add the `PackageListReady` variant to `PackageEvent`**

In `crates/selfie/src/package/event.rs`, add a new variant before `PackageListLoaded` (line 1221):

```rust
/// Sorted filtered package list ready for display (before status checks begin)
PackageListReady {
    operation_info: OperationInfo,
    packages: Vec<PackageListItem>,
},
```

- [ ] **Step 2: Add `send_package_list_ready` method to `EventSender`**

In `crates/selfie/src/package/event.rs`, add near the other `send_package_list*` methods (after
`send_package_list` around line 345):

```rust
pub(crate) async fn send_package_list_ready(&self, packages: Vec<PackageListItem>) {
    let operation_info = self.touch_operation_info();
    self.send(PackageEvent::PackageListReady {
        operation_info,
        packages,
    })
    .await;
}
```

- [ ] **Step 3: Add `PackageListReady` to `EventProcessor` ignored-by-default list**

In `crates/cli/src/event_processor.rs`, find the match arm that lists structured events ignored by
default (around line 219-227). Add `PackageEvent::PackageListReady { .. }` to that list.

- [ ] **Step 4: Build and verify**

Run: `cargo build` Expected: compiles with no errors (warnings about unused variant are OK for now)

- [ ] **Step 5: Commit**

```
git add crates/selfie/src/package/event.rs crates/cli/src/event_processor.rs
git commit -m "Add PackageListReady event variant for streaming list display"
```

### Task 2: Emit `PackageListReady` and reduce progress steps

**Files:**

- Modify: `crates/selfie/src/package/service/list.rs:35-82` (restructure progress steps)
- Modify: `crates/selfie/src/package/service.rs:552` (change total_steps from 5 to 2)

- [ ] **Step 1: Update total_steps for list operation**

In `crates/selfie/src/package/service.rs`, change the list total_steps (line 552):

```rust
// Before:
5, // Load + process + sort + check status + finalize
// After:
2, // Load packages + check status
```

- [ ] **Step 2: Restructure list.rs to emit `PackageListReady`**

In `crates/selfie/src/package/service/list.rs`, replace the current 5-step flow (lines 35-82) with a
2-step flow:

Step 1 ("Loading packages") covers: list files, process, sort, filter, emit `PackageListReady`. Step
2 ("Checking package status") covers: spawn parallel checks.

Remove the 3 intermediate `progress.next()` calls ("Processing package information", "Sorting
packages for streaming", "Checking package status in parallel") and the final "Finalizing package
list" call. Keep only 2 progress calls total.

After sorting and filtering but before spawning checks, emit:

```rust
// Build PackageListItem entries with status: None for the ready event
let ready_items: Vec<PackageListItem> = packages_to_process
    .iter()
    .map(|package| PackageListItem {
        name: package.name().to_string(),
        version: package.version().to_string(),
        environments: package.environments().keys().cloned().collect(),
        status: None,
    })
    .collect();
sender.send_package_list_ready(ready_items).await;
```

- [ ] **Step 3: Handle JoinError in check futures**

In `crates/selfie/src/package/service/list.rs`, replace the silent `if let Ok` (around line 139):

```rust
for handle in check_futures {
    match handle.await {
        Ok(result) => results.push(result),
        Err(e) => {
            // Task panicked — emit an error item so the spinner resolves
            sender.send_package_list_item(PackageListItem {
                name: format!("<unknown task {e}>"),
                version: String::new(),
                environments: Vec::new(),
                status: Some(crate::package::event::CheckResult::Error(
                    format!("Task failed: {e}"),
                )),
            }).await;
        }
    }
}
```

- [ ] **Step 4: Build and test**

Run: `cargo build && cargo test -p selfie` Expected: compiles and all library tests pass

- [ ] **Step 5: Commit**

```
git add crates/selfie/src/package/service/list.rs crates/selfie/src/package/service.rs
git commit -m "Emit PackageListReady event and reduce list to 2 progress steps"
```

### Task 3: Add library test for event ordering

**Files:**

- Modify: `crates/selfie/src/package/service/list.rs` (add test at bottom)

- [ ] **Step 1: Write test that `PackageListReady` is emitted before `PackageListItemCompleted`**

Add a test at the bottom of `list.rs` (or in the existing test module if one exists). Since the list
handler is an internal function, test via the `PackageService::list()` method using a test service
(like other service tests do). Collect the event stream and verify:

1. A `PackageListReady` event appears
2. All `PackageListItemCompleted` events appear after it
3. `PackageListLoaded` appears last (before `Completed`)

Use `test_common::create_test_service` or similar patterns from existing tests.

- [ ] **Step 2: Run test**

Run: `cargo test -p selfie list` Expected: PASS

- [ ] **Step 3: Commit**

```
git add crates/selfie/src/package/service/list.rs
git commit -m "Add test for PackageListReady event ordering"
```

## Chunk 2: CLI — DisplayManager In-Place Finish

### Task 4: Add in-place finish methods to `DisplayManager`

**Files:**

- Modify: `crates/cli/src/display_manager.rs`

The existing `OperationHandle::finish_success/failure/warning` use `finish_and_clear()` +
`mp.println()`, which moves completed items out of position. For the list command, we need in-place
finishing that preserves the bar's position.

- [ ] **Step 1: Add `finish_success_in_place` to `OperationHandle`**

Add to the `impl OperationHandle` block:

```rust
/// Complete the operation in-place (preserves position in MultiProgress)
///
/// Unlike `finish_success()` which clears the bar and prints above,
/// this updates the bar's message and stops it in its current position.
/// Use this when ordering of multiple bars matters (e.g., sorted lists).
pub(crate) fn finish_success_in_place(&self, message: impl Display) {
    let msg = if self.use_colors {
        format!("{} {}", style("✓").green().bold(), style(message).green())
    } else {
        format!("✓ {message}")
    };
    self.bar.set_style(ProgressStyle::with_template("{msg}").unwrap());
    self.bar.finish_with_message(msg);
}

/// Complete the operation in-place with failure
pub(crate) fn finish_failure_in_place(&self, message: impl Display) {
    let msg = if self.use_colors {
        format!("{} {}", style("✗").red().bold(), style(message).red())
    } else {
        format!("✗ {message}")
    };
    self.bar.set_style(ProgressStyle::with_template("{msg}").unwrap());
    self.bar.finish_with_message(msg);
}

/// Complete the operation in-place with warning
pub(crate) fn finish_warning_in_place(&self, message: impl Display) {
    let msg = if self.use_colors {
        format!("{} {}", style("⚠").yellow().bold(), style(message).yellow())
    } else {
        format!("⚠ {message}")
    };
    self.bar.set_style(ProgressStyle::with_template("{msg}").unwrap());
    self.bar.finish_with_message(msg);
}
```

- [ ] **Step 2: Add `start_list_spinner` to `DisplayManager`**

Add a method for creating list-specific spinners with known column widths:

```rust
/// Create a spinner for a list item (used by package list command)
///
/// Unlike `start_operation()`, this creates a spinner optimized for
/// sorted lists: the spinner resolves in-place to preserve ordering.
pub(crate) fn start_list_spinner(&self, message: impl Display) -> OperationHandle {
    let spinner_style = if self.use_colors {
        ProgressStyle::with_template("{spinner:.cyan} {msg}")
            .unwrap()
            .tick_strings(&["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"])
    } else {
        ProgressStyle::with_template("{spinner} {msg}")
            .unwrap()
            .tick_strings(&["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"])
    };

    let bar = self.mp.add(ProgressBar::new_spinner());
    bar.set_style(spinner_style);
    bar.set_message(message.to_string());
    bar.enable_steady_tick(std::time::Duration::from_millis(80));

    OperationHandle {
        bar,
        use_colors: self.use_colors,
        output_lines: VecDeque::new(),
        max_output_lines: 0,
        mp: self.mp.clone(),
    }
}
```

- [ ] **Step 3: Build and run tests**

Run: `cargo build && cargo test -p selfie-cli` Expected: compiles and tests pass

- [ ] **Step 4: Commit**

```
git add crates/cli/src/display_manager.rs
git commit -m "Add in-place finish methods and list spinner to DisplayManager"
```

## Chunk 3: CLI — Rewrite List Command Handler

### Task 5: Rewrite `list.rs` to use streaming spinners

**Files:**

- Modify: `crates/cli/src/commands/package/list.rs` (major rewrite)

- [ ] **Step 1: Rewrite the `ListCommand` and event handler**

Replace the table-based rendering with spinner-based rendering. The key changes:

1. `ListCommand` stores `DisplayManager` (already does)
2. The `handle_list_event` function now handles `PackageListReady` to create spinners
3. `PackageListItemCompleted` resolves spinners in-place
4. `PackageListLoaded` prints the summary
5. Drop `comfy_table` import for the main list (keep for environment stats fallback)
6. Drop `format_status` function (status formatting moves inline to spinner resolution)
7. Drop `display_packages_table` function

The handler needs a `HashMap<String, OperationHandle>` to track spinners by package name, and the
`show_all` flag to decide whether to include environments.

Key implementation details:

- On `PackageListReady`: compute `max_name_len` from packages, create spinner per package with
  aligned columns: `format!("{:<width$}  v{}  checking...", name, version)`
- On `PackageListItemCompleted`: look up spinner by `package_item.name`, format status string using
  `status_style::*` functions, call `finish_success_in_place` / `finish_failure_in_place`
- On `PackageListLoaded`: print summary via `display.println(...)`, print invalid packages
- On `Progress`: return `true` (suppress — spinners are the progress indicator)
- Keep `display_environment_stats` and `display_invalid_packages_table` for the summary section (but
  simplify invalid packages to a plain list instead of table)

For non-TTY: check `display.is_tty()` — if false, on `PackageListItemCompleted` just `println!` the
result line directly instead of using spinners.

- [ ] **Step 2: Add `is_tty()` accessor to `DisplayManager`**

In `crates/cli/src/display_manager.rs`, add:

```rust
/// Whether the output is a TTY (interactive terminal)
pub(crate) fn is_tty(&self) -> bool {
    self.is_tty
}
```

Remove the `#[allow(dead_code)]` from the `is_tty` field since it's now used.

- [ ] **Step 3: Update tests in `list.rs`**

Update existing tests to work with the new handler. Tests that used `TerminalProgressReporter` were
already updated to use `DisplayManager`. Now update them to handle the new event types:

- Test `PackageListReady` creates the expected number of entries
- Test `PackageListItemCompleted` resolves correctly
- Test `PackageListLoaded` prints summary
- Keep edge case tests (empty list, invalid packages only, etc.)

- [ ] **Step 4: Build, clippy, test**

Run: `cargo fmt && cargo clippy --all-targets && cargo test` Expected: zero warnings, all tests pass

- [ ] **Step 5: Commit**

```
git add crates/cli/src/commands/package/list.rs crates/cli/src/display_manager.rs
git commit -m "Rewrite list command to use streaming spinners with in-place finish"
```

## Chunk 4: Cleanup and Verification

### Task 6: Remove dead code and clean up

**Files:**

- Modify: `crates/cli/src/display_manager.rs` (remove unnecessary `#[allow(dead_code)]`)
- Modify: `crates/cli/src/status_style.rs` (remove `#[allow(dead_code)]` if now used)

- [ ] **Step 1: Remove `#[allow(dead_code)]` annotations that are no longer needed**

After the list command uses `start_list_spinner`, `OperationHandle`, and the in-place finish
methods, several `#[allow(dead_code)]` annotations can be removed. Run clippy to see which ones are
still needed and remove the rest.

- [ ] **Step 2: Run full pre-commit checklist**

```bash
cargo fmt
dprint fmt
cargo clippy --all-targets  # zero warnings
cargo test                  # all tests pass
```

- [ ] **Step 3: Manual smoke test**

```bash
cargo run -- list              # spinners appear, fill in, summary prints
cargo run -- list --all        # environments shown inline
cargo run -- list 2>&1 | cat   # no control characters
```

- [ ] **Step 4: Commit**

```
git add -A
git commit -m "Clean up dead code annotations after list spinner integration"
```

- [ ] **Step 5: Push and update PR**

```bash
git push
```
