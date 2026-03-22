# Dotfile Deployment (Phase 2) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development
> (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use
> checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add dotfile management to selfie so packages can declare associated dotfiles (shell
configs, app configs) that are deployed separately via `selfie apply`, with conflict detection,
diffing, and checksum-based drift tracking.

**Architecture:** Extends the hexagonal architecture with a new `DotfileService` trait (port) and
`DotfileServiceImpl` (adapter). Dotfile deployment uses the existing `FileSystem` trait for all I/O
and the `PackageEvent` enum for communication. A new `DeployState` struct manages per-machine
checksum tracking in `~/.config/selfie/deploy-state.yml`. The `similar` crate provides diffing.

**Tech Stack:** Rust, serde_yaml, similar (diffing), sha2 (checksums), dialoguer (conflict prompts),
existing selfie infrastructure (FileSystem trait, EventSender, ProgressTracker)

**Design Spec:** `docs/superpowers/specs/2026-03-18-environment-manager-design.md`

---

## File Structure

### New files

| File                                            | Responsibility                                         |
| ----------------------------------------------- | ------------------------------------------------------ |
| `crates/selfie/src/dotfile_service.rs`          | Module root — re-exports                               |
| `crates/selfie/src/dotfile_service/port.rs`     | `DotfileService` trait (hexagonal port)                |
| `crates/selfie/src/dotfile_service/service.rs`  | `DotfileServiceImpl` — orchestrates apply operations   |
| `crates/selfie/src/dotfile_service/deploy.rs`   | Core deployment logic: copy, checksum, conflict detect |
| `crates/selfie/src/dotfile_service/state.rs`    | `DeployState` — reads/writes `deploy-state.yml`        |
| `crates/selfie/src/dotfile_service/diff.rs`     | Thin wrapper around `similar` for unified diffs        |
| `crates/cli/src/commands/apply.rs`              | CLI `selfie apply` command handler                     |
| `crates/selfie/src/dotfile_service/semantic.rs` | Shell rc file content scanning for related dotfiles    |
| `crates/selfie/tests/dotfile_service_tests.rs`  | Integration tests for dotfile service                  |
| `crates/cli/tests/apply_tests.rs`               | CLI integration tests for apply command                |

### Modified files

| File                                           | Change                                                                                 |
| ---------------------------------------------- | -------------------------------------------------------------------------------------- |
| `Cargo.toml` (workspace)                       | Add `similar`, `sha2`, `chrono` workspace deps                                         |
| `crates/selfie/Cargo.toml`                     | Add `similar`, `sha2`, `chrono` deps                                                   |
| `crates/selfie/src/lib.rs`                     | Add `pub mod dotfile_service`                                                          |
| `crates/selfie/src/package.rs`                 | Add `dotfiles: Vec<DotfileEntry>` and `post_install_note: Option<String>` to `Package` |
| `crates/selfie/src/package/builder.rs`         | Add builder methods for `dotfiles` and `post_install_note`                             |
| `crates/selfie/src/package/event.rs`           | Add dotfile deployment event variants + `PostInstallNote`                              |
| `crates/selfie/src/package/validate.rs`        | Validate dotfile entries (source exists, target is absolute)                           |
| `crates/selfie/src/config.rs`                  | Add `dotfiles_directory` to `SelfieConfig`                                             |
| `crates/selfie/src/config/yaml.rs`             | Deserialize `dotfiles_directory` with default derivation                               |
| `crates/cli/src/cli.rs`                        | Add `Apply` command to `ClapCommands`                                                  |
| `crates/cli/src/commands.rs`                   | Route `Apply` command to handler                                                       |
| `crates/cli/src/event_processor.rs`            | Handle new dotfile event variants                                                      |
| `crates/mcp-server/src/event_collector.rs`     | Convert dotfile events to JSON                                                         |
| `crates/mcp-server/src/server.rs`              | Add `selfie_apply` tool                                                                |
| `crates/selfie/src/package/service/install.rs` | Emit `PostInstallNote` after fresh install                                             |
| `crates/selfie/src/package/service/remove.rs`  | Emit `ConfigCleanupInfo` during package removal                                        |
| `crates/cli/src/commands/spec/remove.rs`       | Display dotfile cleanup info to user                                                   |

---

## Task 1: Add workspace dependencies

**Files:**

- Modify: `Cargo.toml` (workspace root, `[workspace.dependencies]` section)
- Modify: `crates/selfie/Cargo.toml` (add deps)

- [ ] **Step 1: Add `similar`, `sha2`, and `chrono` to workspace deps**

In root `Cargo.toml` under `[workspace.dependencies]`, add:

```toml
similar = "2"
sha2 = "0.10"
chrono = { version = "0.4", features = ["serde"] }
```

In `crates/selfie/Cargo.toml` under `[dependencies]`, add:

```toml
similar.workspace = true
sha2.workspace = true
chrono.workspace = true
```

- [ ] **Step 2: Verify it compiles**

Run: `cargo check -p selfie` Expected: compiles with no errors

- [ ] **Step 3: Commit**

```bash
git add Cargo.toml crates/selfie/Cargo.toml Cargo.lock
git commit -m "deps: add similar, sha2, and chrono for dotfile deployment"
```

---

## Task 2: Add `dotfiles` and `post_install_note` to Package schema

**Files:**

- Modify: `crates/selfie/src/package.rs`
- Modify: `crates/selfie/src/package/builder.rs`
- Test: existing tests in `crates/selfie/tests/package_validation_tests.rs`

This task adds two new fields to `Package`:

- `dotfiles: Vec<DotfileEntry>` — list of dotfile mappings (source → target)
- `post_install_note: Option<String>` — text shown after fresh install

`DotfileEntry` is a new struct with `source: String` and `target: String`.

`dotfiles` is top-level on `Package` (not per-environment) because dotfiles apply regardless of
environment — if a dotfile truly differs per-environment, use separate packages.

- [ ] **Step 1: Write failing test for DotfileEntry parsing**

In `crates/selfie/tests/package_validation_tests.rs`, add:

```rust
#[test]
fn test_parse_package_with_dotfiles() {
    let yaml = r#"
name: fnm
version: "1.0.0"
environments:
  macos:
    install: brew install fnm
dotfiles:
  - source: fnm/fish-conf.fish
    target: ~/.config/fish/conf.d/fnm.fish
  - source: fnm/zsh-conf.zsh
    target: ~/.config/zsh/conf.d/fnm.zsh
post_install_note: |
  Configure your shell for fnm.
"#;
    let package: selfie::package::Package = serde_yaml::from_str(yaml).unwrap();
    assert_eq!(package.dotfiles().len(), 2);
    assert_eq!(package.dotfiles()[0].source(), "fnm/fish-conf.fish");
    assert_eq!(package.dotfiles()[0].target(), "~/.config/fish/conf.d/fnm.fish");
    assert_eq!(package.post_install_note().unwrap(), "Configure your shell for fnm.\n");
}

#[test]
fn test_parse_package_without_dotfiles_defaults_to_empty() {
    let yaml = r#"
name: basic
version: "1.0.0"
environments:
  linux:
    install: apt install basic
"#;
    let package: selfie::package::Package = serde_yaml::from_str(yaml).unwrap();
    assert!(package.dotfiles().is_empty());
    assert!(package.post_install_note().is_none());
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p selfie --test package_validation_tests test_parse_package_with_dotfiles`
Expected: FAIL — `dotfiles()` and `post_install_note()` don't exist

- [ ] **Step 3: Implement DotfileEntry struct and Package fields**

In `crates/selfie/src/package.rs`, add the `DotfileEntry` struct (above `Package`):

```rust
/// A dotfile mapping from repo source to deployment target.
///
/// Source paths are relative to the parent directory of the YAML file
/// (e.g., `fnm/fish-conf.fish` in `packages/fnm.yaml` resolves to
/// `<packages_dir>/fnm/fish-conf.fish`).
/// Target paths are absolute (with `~` expansion supported).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DotfileEntry {
    source: String,
    target: String,
}

impl DotfileEntry {
    pub fn new(source: impl Into<String>, target: impl Into<String>) -> Self {
        Self {
            source: source.into(),
            target: target.into(),
        }
    }

    pub fn source(&self) -> &str {
        &self.source
    }

    pub fn target(&self) -> &str {
        &self.target
    }
}
```

Add to `Package` struct (after `description`):

```rust
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    dotfiles: Vec<DotfileEntry>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    post_install_note: Option<String>,
```

Add getters to `Package` impl:

```rust
    pub fn dotfiles(&self) -> &[DotfileEntry] {
        &self.dotfiles
    }

    pub fn post_install_note(&self) -> Option<&str> {
        self.post_install_note.as_deref()
    }
```

Update `new_template()` to include `dotfiles: Vec::new()` and `post_install_note: None`.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p selfie --test package_validation_tests` Expected: all PASS

- [ ] **Step 5: Update builder**

In `crates/selfie/src/package/builder.rs`, add to `PackageBuilder`:

```rust
dotfiles: Vec<DotfileEntry>,
post_install_note: Option<String>,
```

Add builder methods:

```rust
    pub fn dotfiles(mut self, dotfiles: Vec<DotfileEntry>) -> Self {
        self.dotfiles = dotfiles;
        self
    }

    pub fn post_install_note(mut self, note: impl Into<String>) -> Self {
        self.post_install_note = Some(note.into());
        self
    }
```

Update `build()` to include `dotfiles: self.dotfiles` and
`post_install_note: self.post_install_note`.

- [ ] **Step 6: Fix any compilation errors across the codebase**

New fields with `#[serde(default)]` won't break deserialization, but any code that constructs
`Package` directly (not via builder or serde) may need updating. Search for direct `Package`
construction:

Run: `cargo build --all-targets 2>&1 | head -30`

Fix any errors by adding `dotfiles: Vec::new(), post_install_note: None` where needed.

- [ ] **Step 7: Run full test suite**

Run: `cargo test` Expected: all tests pass

- [ ] **Step 8: Run pre-commit checks and commit**

```bash
cargo fmt && dprint fmt
cargo clippy --all-targets
cargo test
git add -A && git commit -m "feat: add dotfiles and post_install_note fields to Package schema"
```

---

## Task 3: Add `dotfiles_directory` to SelfieConfig

**Files:**

- Modify: `crates/selfie/src/config.rs`

The `dotfiles_directory` defaults to a sibling of `package_directory`. If `package_directory` is
`~/selfie-packages/packages`, then `dotfiles_directory` defaults to `~/selfie-packages/dotfiles`.
Users can override in config.yaml.

- [ ] **Step 1: Write failing test**

In the `tests` module of `crates/selfie/src/config.rs`:

```rust
#[test]
fn test_dotfiles_directory_defaults_to_sibling_of_package_directory() {
    let config = SelfieConfigBuilder::default()
        .package_directory(PathBuf::from("/home/user/selfie-packages/packages"))
        .build();
    assert_eq!(
        config.dotfiles_directory(),
        Path::new("/home/user/selfie-packages/dotfiles")
    );
}

#[test]
fn test_dotfiles_directory_can_be_overridden() {
    let config = SelfieConfigBuilder::default()
        .package_directory(PathBuf::from("/home/user/selfie-packages/packages"))
        .dotfiles_directory(PathBuf::from("/custom/dotfiles"))
        .build();
    assert_eq!(
        config.dotfiles_directory(),
        Path::new("/custom/dotfiles")
    );
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p selfie config::tests::test_dotfiles_directory` Expected: FAIL — method doesn't
exist

- [ ] **Step 3: Implement**

Add to `SelfieConfig`:

```rust
#[serde(default)]
dotfiles_directory: Option<PathBuf>,
```

Add getter that derives default from `package_directory`:

```rust
pub fn dotfiles_directory(&self) -> PathBuf {
    self.dotfiles_directory.clone().unwrap_or_else(|| {
        // Default: sibling of package_directory
        // e.g., /foo/packages → /foo/dotfiles
        self.package_directory
            .parent()
            .map(|p| p.join("dotfiles"))
            .unwrap_or_else(|| self.package_directory.join("dotfiles"))
    })
}
```

Add to `SelfieConfigBuilder`:

```rust
    dotfiles_directory: Option<PathBuf>,

    pub fn dotfiles_directory(mut self, path: PathBuf) -> Self {
        self.dotfiles_directory = Some(path);
        self
    }
```

Update `build()` to include `dotfiles_directory: self.dotfiles_directory`.

- [ ] **Step 4: Run tests**

Run: `cargo test -p selfie config::tests` Expected: all PASS

- [ ] **Step 5: Pre-commit checks and commit**

```bash
cargo fmt && cargo clippy --all-targets && cargo test
git add -A && git commit -m "feat: add dotfiles_directory to SelfieConfig with smart default"
```

---

## Task 4: Add dotfile deployment events to PackageEvent

**Files:**

- Modify: `crates/selfie/src/package/event.rs`
- Modify: `crates/cli/src/event_processor.rs`
- Modify: `crates/mcp-server/src/event_collector.rs`

Add event variants that the dotfile service will emit and that CLI/MCP consumers will handle.

- [ ] **Step 1: Add event variants**

Add to the `PackageEvent` enum in `event.rs`:

```rust
/// A dotfile is about to be deployed
ConfigDeploying {
    operation_info: OperationInfo,
    source: String,
    target: String,
},
/// A dotfile was deployed successfully
ConfigDeployed {
    operation_info: OperationInfo,
    source: String,
    target: String,
},
/// A dotfile was skipped (already current or user declined)
ConfigSkipped {
    operation_info: OperationInfo,
    source: String,
    target: String,
    reason: String,
},
/// A conflict was detected between repo and deployed version
ConfigConflict {
    operation_info: OperationInfo,
    source: String,
    target: String,
    diff: String,
},
/// Drift detected between deployed file and repo source
ConfigDriftDetected {
    operation_info: OperationInfo,
    target: String,
    drift_type: String,
},
/// Post-install note to display to user
PostInstallNote {
    operation_info: OperationInfo,
    package_name: String,
    note: String,
},
```

Add helper methods on `EventSender` for emitting these events (follow the pattern of
`send_recommend_started` etc.).

- [ ] **Step 2: Add event handlers to CLI event_processor.rs**

Add match arms in the main `handle_event` function for each new variant:

```rust
PackageEvent::ConfigDeploying { source, target, .. } => {
    self.display.print_info(format!("  Deploying {source} → {target}"));
}
PackageEvent::ConfigDeployed { source, target, .. } => {
    self.display.print_success(format!("  ✓ {source} → {target}"));
}
PackageEvent::ConfigSkipped { source, reason, .. } => {
    self.display.print_info(format!("  ⊘ {source} skipped: {reason}"));
}
PackageEvent::ConfigConflict { source, target, diff, .. } => {
    self.display.print_warning(format!("  ⚠ Conflict: {source} → {target}"));
    self.display.print_info(format!("{diff}"));
}
PackageEvent::ConfigDriftDetected { target, drift_type, .. } => {
    self.display.print_warning(format!("  ⚠ Drift in {target}: {drift_type}"));
}
PackageEvent::PostInstallNote { note, .. } => {
    self.display.print_info(format!("\n📋 {note}"));
}
```

- [ ] **Step 3: Add event handling to MCP event_collector.rs**

Add JSON conversion for each new event variant in `event_to_json()`.

- [ ] **Step 4: Verify compilation and tests**

Run: `cargo build --all-targets && cargo test` Expected: compiles and all tests pass

- [ ] **Step 5: Commit**

```bash
cargo fmt && cargo clippy --all-targets
git add -A && git commit -m "feat: add dotfile deployment event variants"
```

---

## Task 5: Implement deploy state tracking

**Files:**

- Create: `crates/selfie/src/dotfile_service/state.rs`
- Create: `crates/selfie/src/dotfile_service.rs`

The deploy state tracks which dotfiles have been deployed, with checksums for drift detection.
Stored at `~/.config/selfie/deploy-state.yml` (per-machine, NOT in the repo).

- [ ] **Step 1: Create module root**

Create `crates/selfie/src/dotfile_service.rs`:

```rust
pub mod state;
```

Add `pub mod dotfile_service;` to `crates/selfie/src/lib.rs`.

- [ ] **Step 2: Write failing tests for DeployState**

In `crates/selfie/src/dotfile_service/state.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_empty_state() {
        let state = DeployState::empty();
        assert!(state.entries().is_empty());
    }

    #[test]
    fn test_record_deployment() {
        let mut state = DeployState::empty();
        state.record_deployment(
            "fnm/fish-conf.fish",
            "/home/user/.config/fish/conf.d/fnm.fish",
            "abc123",
        );
        let entry = state.get("fnm/fish-conf.fish").unwrap();
        assert_eq!(entry.target(), "/home/user/.config/fish/conf.d/fnm.fish");
        assert_eq!(entry.source_checksum(), "abc123");
        assert_eq!(entry.deployed_checksum(), "abc123");
    }

    #[test]
    fn test_roundtrip_serialization() {
        let mut state = DeployState::empty();
        state.record_deployment("a/b.txt", "/home/user/b.txt", "hash1");
        let yaml = serde_yaml::to_string(&state).unwrap();
        let loaded: DeployState = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(loaded.entries().len(), 1);
    }

    #[test]
    fn test_detect_drift_no_change() {
        let mut state = DeployState::empty();
        state.record_deployment("a/b.txt", "/target/b.txt", "hash1");
        let drift = state.detect_drift("a/b.txt", "hash1", "hash1");
        assert_eq!(drift, DriftType::None);
    }

    #[test]
    fn test_detect_drift_repo_changed() {
        let mut state = DeployState::empty();
        state.record_deployment("a/b.txt", "/target/b.txt", "hash1");
        let drift = state.detect_drift("a/b.txt", "hash2", "hash1");
        assert_eq!(drift, DriftType::RepoChanged);
    }

    #[test]
    fn test_detect_drift_target_changed() {
        let mut state = DeployState::empty();
        state.record_deployment("a/b.txt", "/target/b.txt", "hash1");
        let drift = state.detect_drift("a/b.txt", "hash1", "hash_different");
        assert_eq!(drift, DriftType::TargetChanged);
    }

    #[test]
    fn test_detect_drift_both_changed() {
        let mut state = DeployState::empty();
        state.record_deployment("a/b.txt", "/target/b.txt", "hash1");
        let drift = state.detect_drift("a/b.txt", "hash2", "hash3");
        assert_eq!(drift, DriftType::BothChanged);
    }

    #[test]
    fn test_detect_drift_not_tracked() {
        let state = DeployState::empty();
        let drift = state.detect_drift("unknown.txt", "hash1", "hash2");
        assert_eq!(drift, DriftType::NotTracked);
    }
}
```

- [ ] **Step 3: Run tests to verify they fail**

Run: `cargo test -p selfie dotfile_service::state::tests` Expected: FAIL — module doesn't exist

- [ ] **Step 4: Implement DeployState**

```rust
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Tracks which dotfiles have been deployed and their checksums.
/// Persisted at `~/.config/selfie/deploy-state.yml` (per-machine state).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct DeployState {
    #[serde(default)]
    deployed: HashMap<String, DeployEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeployEntry {
    target: String,
    source_checksum: String,
    deployed_checksum: String,
    deployed_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DriftType {
    /// No change — source and target match last deployment
    None,
    /// Source (repo) has changed since last deployment
    RepoChanged,
    /// Target (deployed file) has been edited since deployment
    TargetChanged,
    /// Both source and target have changed — conflict
    BothChanged,
    /// File not previously tracked
    NotTracked,
}

impl DeployState {
    pub fn empty() -> Self {
        Self::default()
    }

    pub fn entries(&self) -> &HashMap<String, DeployEntry> {
        &self.deployed
    }

    pub fn get(&self, source: &str) -> Option<&DeployEntry> {
        self.deployed.get(source)
    }

    pub fn record_deployment(
        &mut self,
        source: &str,
        target: &str,
        checksum: &str,
    ) {
        self.deployed.insert(
            source.to_string(),
            DeployEntry {
                target: target.to_string(),
                source_checksum: checksum.to_string(),
                deployed_checksum: checksum.to_string(),
                deployed_at: chrono::Utc::now().to_rfc3339(),
            },
        );
    }

    /// Detect drift by comparing current checksums against last deployment.
    ///
    /// `current_source_checksum` = hash of the file in the repo now.
    /// `current_target_checksum` = hash of the deployed file on disk now.
    pub fn detect_drift(
        &self,
        source: &str,
        current_source_checksum: &str,
        current_target_checksum: &str,
    ) -> DriftType {
        let Some(entry) = self.deployed.get(source) else {
            return DriftType::NotTracked;
        };
        let repo_changed = entry.source_checksum != current_source_checksum;
        let target_changed = entry.deployed_checksum != current_target_checksum;
        match (repo_changed, target_changed) {
            (false, false) => DriftType::None,
            (true, false) => DriftType::RepoChanged,
            (false, true) => DriftType::TargetChanged,
            (true, true) => DriftType::BothChanged,
        }
    }
}

impl DeployEntry {
    pub fn target(&self) -> &str {
        &self.target
    }

    pub fn source_checksum(&self) -> &str {
        &self.source_checksum
    }

    pub fn deployed_checksum(&self) -> &str {
        &self.deployed_checksum
    }
}
```

- [ ] **Step 5: Run tests**

Run: `cargo test -p selfie dotfile_service::state::tests` Expected: all PASS

- [ ] **Step 6: Commit**

```bash
cargo fmt && cargo clippy --all-targets && cargo test
git add -A && git commit -m "feat: implement DeployState for dotfile checksum tracking"
```

---

## Task 6: Implement dotfile diffing

**Files:**

- Create: `crates/selfie/src/dotfile_service/diff.rs`
- Modify: `crates/selfie/src/dotfile_service.rs`

Thin wrapper around the `similar` crate for producing human-readable unified diffs.

- [ ] **Step 1: Write failing tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_identical_files_produce_no_diff() {
        let diff = unified_diff("hello\nworld\n", "hello\nworld\n", "source", "target");
        assert!(diff.is_empty() || diff.trim().is_empty());
    }

    #[test]
    fn test_different_files_produce_diff() {
        let diff = unified_diff("hello\nworld\n", "hello\nearth\n", "source", "target");
        assert!(diff.contains("-world"));
        assert!(diff.contains("+earth"));
    }

    #[test]
    fn test_diff_includes_file_labels() {
        let diff = unified_diff("a\n", "b\n", "repo/file.txt", "~/.config/file.txt");
        assert!(diff.contains("repo/file.txt"));
        assert!(diff.contains("~/.config/file.txt"));
    }
}
```

- [ ] **Step 2: Implement**

```rust
use similar::{ChangeTag, TextDiff};

/// Produce a unified diff between two strings, labeled with source/target paths.
pub fn unified_diff(old: &str, new: &str, old_label: &str, new_label: &str) -> String {
    let diff = TextDiff::from_lines(old, new);
    let mut output = String::new();

    if diff.ratio() == 1.0 {
        return output; // Identical
    }

    output.push_str(&format!("--- {old_label}\n"));
    output.push_str(&format!("+++ {new_label}\n"));

    for hunk in diff.unified_diff().context_radius(3).iter_hunks() {
        output.push_str(&hunk.to_string());
    }

    output
}
```

- [ ] **Step 3: Run tests and commit**

```bash
cargo test -p selfie dotfile_service::diff::tests
cargo fmt && cargo clippy --all-targets && cargo test
git add -A && git commit -m "feat: add unified diff wrapper using similar crate"
```

---

## Task 7: Implement core deployment logic

**Files:**

- Create: `crates/selfie/src/dotfile_service/deploy.rs`
- Modify: `crates/selfie/src/dotfile_service.rs`

This is the core logic: given a dotfile entry, resolve paths, compute checksums, detect conflicts,
and copy the file. All I/O goes through `FileSystem` trait.

- [ ] **Step 1: Write failing tests for checksum computation**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_compute_checksum() {
        let checksum = compute_checksum(b"hello world");
        // SHA-256 of "hello world"
        assert_eq!(
            checksum,
            "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9"
        );
    }

    #[test]
    fn test_compute_checksum_different_content() {
        let a = compute_checksum(b"hello");
        let b = compute_checksum(b"world");
        assert_ne!(a, b);
    }
}
```

- [ ] **Step 2: Implement checksum**

```rust
use sha2::{Digest, Sha256};

pub fn compute_checksum(data: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data);
    format!("{:x}", hasher.finalize())
}
```

- [ ] **Step 3: Write failing tests for source path resolution**

```rust
#[test]
fn test_resolve_source_path() {
    let base_dir = Path::new("/home/user/selfie-packages/packages");
    let source = "fnm/fish-conf.fish";
    let resolved = resolve_source_path(base_dir, source);
    assert_eq!(
        resolved,
        PathBuf::from("/home/user/selfie-packages/packages/fnm/fish-conf.fish")
    );
}
```

- [ ] **Step 4: Implement source path resolution**

```rust
use std::path::{Path, PathBuf};

/// Resolve a dotfile source path relative to the base directory.
/// Source paths in YAML are relative to the parent directory of the YAML file.
pub fn resolve_source_path(base_dir: &Path, source: &str) -> PathBuf {
    base_dir.join(source)
}
```

- [ ] **Step 5: Write failing tests for deployment decision logic**

```rust
    #[test]
    fn test_deploy_decision_target_does_not_exist() {
        let decision = deploy_decision(
            &DriftType::NotTracked,
            false, // target_exists
        );
        assert_eq!(decision, DeployDecision::Deploy);
    }

    #[test]
    fn test_deploy_decision_already_current() {
        let decision = deploy_decision(
            &DriftType::None,
            true,
        );
        assert_eq!(decision, DeployDecision::Skip("already up to date".into()));
    }

    #[test]
    fn test_deploy_decision_repo_changed() {
        let decision = deploy_decision(
            &DriftType::RepoChanged,
            true,
        );
        assert_eq!(decision, DeployDecision::Deploy);
    }

    #[test]
    fn test_deploy_decision_target_changed() {
        let decision = deploy_decision(
            &DriftType::TargetChanged,
            true,
        );
        assert_eq!(decision, DeployDecision::Conflict);
    }

    #[test]
    fn test_deploy_decision_both_changed() {
        let decision = deploy_decision(
            &DriftType::BothChanged,
            true,
        );
        assert_eq!(decision, DeployDecision::Conflict);
    }
```

- [ ] **Step 6: Implement deployment decision logic**

```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeployDecision {
    /// Safe to deploy (target doesn't exist or repo is newer)
    Deploy,
    /// Skip deployment (already up to date)
    Skip(String),
    /// Conflict detected — needs user input
    Conflict,
}

pub fn deploy_decision(drift: &DriftType, target_exists: bool) -> DeployDecision {
    if !target_exists {
        return DeployDecision::Deploy;
    }
    match drift {
        DriftType::None => DeployDecision::Skip("already up to date".into()),
        DriftType::RepoChanged => DeployDecision::Deploy,
        DriftType::TargetChanged | DriftType::BothChanged => DeployDecision::Conflict,
        DriftType::NotTracked => DeployDecision::Conflict, // unknown state, be cautious
    }
}
```

- [ ] **Step 7: Run all tests and commit**

```bash
cargo fmt && cargo clippy --all-targets && cargo test
git add -A && git commit -m "feat: implement core dotfile deployment logic (checksum, resolve, decision)"
```

---

## Task 8: Implement DotfileService trait and service

**Files:**

- Create: `crates/selfie/src/dotfile_service/port.rs`
- Create: `crates/selfie/src/dotfile_service/service.rs`
- Modify: `crates/selfie/src/dotfile_service.rs`
- Test: `crates/selfie/tests/dotfile_service_tests.rs`

This is the hexagonal port (trait) and adapter (impl) for dotfile operations. Follows the same
pattern as `PackageService`/`PackageServiceImpl`.

- [ ] **Step 1: Define the DotfileService trait**

In `crates/selfie/src/dotfile_service/port.rs`:

```rust
use crate::package::EventStream;
use std::future::Future;

/// Options for dotfile apply operations
#[derive(Debug, Clone, Default)]
pub struct ApplyOptions {
    /// Show what would change without writing
    pub dry_run: bool,
    /// Auto-accept overwrite for conflicts (--yes flag)
    pub auto_accept: bool,
}

/// Port for dotfile deployment operations (Hexagonal Architecture)
#[cfg_attr(any(test, feature = "with_mocks"), mockall::automock)]
pub trait DotfileService: Send + Sync {
    /// Deploy all dotfiles from all packages and standalone dotfiles
    fn apply_all(&self, options: ApplyOptions) -> impl Future<Output = EventStream> + Send;

    /// Deploy dotfiles for a specific package or dotfile
    fn apply(&self, name: &str, options: ApplyOptions) -> impl Future<Output = EventStream> + Send;

    /// Check for drift between deployed files and repo sources
    fn check_drift(&self) -> impl Future<Output = EventStream> + Send;
}
```

- [ ] **Step 2: Implement DotfileServiceImpl**

In `crates/selfie/src/dotfile_service/service.rs`, implement the service using the same
`execute_operation_with_deps()` pattern from `PackageServiceImpl`:

- Takes `PackageRepository`, `FileSystem`, and `SelfieConfig` as constructor params
- `apply_all`: scans both `package_directory` and `dotfiles_directory` for YAML files, extracts
  `dotfiles` entries, deploys each one
- `apply`: loads a single package/dotfile by name, deploys its dotfiles
- `check_drift`: loads deploy state, checks each entry for drift

The service should:

1. Create an event channel
2. Spawn an async task that does the work
3. Return the event stream

For conflict handling in the library layer: emit `ConfigConflict` events with the diff. The library
does NOT prompt — the CLI handles prompting. In `--dry-run` mode or non-interactive mode, conflicts
are reported but not resolved. In the initial implementation, conflicts are always reported (the
interactive prompt can be added as a follow-up).

- [ ] **Step 3: Write integration tests**

In `crates/selfie/tests/dotfile_service_tests.rs`, test the service with mock filesystem:

- Test `apply_all` with a package that has dotfiles
- Test `apply_all` when target doesn't exist (fresh deploy)
- Test `apply` for a specific package
- Test that dry_run doesn't write files
- Test drift detection

Follow the pattern in `crates/selfie/tests/package_service_tests.rs` for service test setup.

- [ ] **Step 4: Run tests and commit**

```bash
cargo fmt && cargo clippy --all-targets && cargo test
git add -A && git commit -m "feat: implement DotfileService trait and service for dotfile deployment"
```

---

## Task 9: Add `selfie apply` CLI command

**Files:**

- Create: `crates/cli/src/commands/apply.rs`
- Modify: `crates/cli/src/cli.rs`
- Modify: `crates/cli/src/commands.rs`
- Test: `crates/cli/tests/apply_tests.rs`

- [ ] **Step 1: Add Apply command to clap CLI**

In `crates/cli/src/cli.rs`, add to `ClapCommands`:

```rust
/// Deploy dotfiles from repo to target locations
Apply(ApplyCommands),
```

Define `ApplyCommands`:

```rust
#[derive(Debug, Subcommand)]
pub enum ApplySubcommands {
    // For now, apply is a single command — but using subcommand structure
    // allows future expansion (e.g., `selfie apply drift`)
}

#[derive(Debug, Args)]
pub struct ApplyCommands {
    /// Specific package or dotfile name (deploys all if omitted)
    pub name: Option<String>,

    /// Show what would change without writing
    #[arg(long)]
    pub dry_run: bool,

    /// Auto-accept overwrite for conflicts (non-interactive mode)
    #[arg(long, short)]
    pub yes: bool,
}
```

- [ ] **Step 2: Add command handler**

Create `crates/cli/src/commands/apply.rs`:

```rust
use crate::config::CliConfig;
use crate::display_manager::DisplayManager;
use crate::event_processor::EventProcessor;
use selfie::dotfile_service::DotfileService;

pub async fn handle_apply(
    service: &impl DotfileService,

    name: Option<&str>,
    dry_run: bool,
    auto_accept: bool,
    config: &CliConfig,
    display: &DisplayManager,
) -> i32 {
    let options = ApplyOptions { dry_run, auto_accept };
    let stream = match name {
        Some(n) => service.apply(n, options).await,
        None => service.apply_all(options).await,
    };

    let processor = EventProcessor::new(display.clone());
    let result = processor
        .process_events(stream, |_event, _display| false)
        .await;
    result.exit_code()
}
```

- [ ] **Step 3: Wire up command dispatch**

In `crates/cli/src/commands.rs`, add the `Apply` arm to `dispatch_command()`. The dotfile service
needs to be created with the filesystem and repository. Follow the same pattern as
`create_package_service()` but for dotfiles:

```rust
ClapCommands::Apply(apply_cmd) => {
    let dotfile_service = create_dotfile_service(&config);
    commands::apply::handle_apply(
        &dotfile_service,
        apply_cmd.name.as_deref(),
        apply_cmd.dry_run,
        apply_cmd.yes,
        &config,
        &display,
    )
    .await
}
```

- [ ] **Step 4: Write CLI integration test**

In `crates/cli/tests/apply_tests.rs`, test the command runs:

```rust
#[test]
fn test_apply_with_no_dotfiles_succeeds() {
    let temp_dir = setup_default_test_config();
    // Create a package with no dotfiles
    let package = PackageBuilder::default()
        .name("test-pkg")
        .version("1.0.0")
        .environment(SELFIE_ENV, |b| b.install("echo hi"))
        .build();
    add_package(&temp_dir, &package);

    let mut cmd = get_command_with_test_config(&temp_dir);
    cmd.args(["apply"]);
    cmd.assert().success();
}
```

- [ ] **Step 5: Run full test suite, pre-commit checks, and commit**

```bash
cargo fmt && dprint fmt && cargo clippy --all-targets && cargo test
git add -A && git commit -m "feat: add selfie apply CLI command for dotfile deployment"
```

---

## Task 10: Emit PostInstallNote during install

**Files:**

- Modify: `crates/selfie/src/package/service/install.rs`

When a package has `post_install_note` and the check command was failing before install (meaning it
was a fresh install, not a reinstall), emit the note after successful installation.

- [ ] **Step 1: Write failing test**

Add to the install test infrastructure — create a test where a package has a `post_install_note` and
verify the event is emitted. Follow patterns in the existing `package_service_tests.rs`.

- [ ] **Step 2: Implement**

In `install_single_package()`, after the install command succeeds and before returning:

1. Check if the pre-install check was failing (i.e., `pre_install_check` indicated not installed)
2. If `package.post_install_note()` is `Some(note)`, emit `PostInstallNote` event
3. The CLI event_processor already handles this event (added in Task 4)

- [ ] **Step 3: Run tests and commit**

```bash
cargo fmt && cargo clippy --all-targets && cargo test
git add -A && git commit -m "feat: emit post_install_note after fresh package install"
```

---

## Task 11: Validate dotfile entries

**Files:**

- Modify: `crates/selfie/src/package/validate.rs`

Add validation rules for dotfile entries:

- Target must be an absolute path or start with `~`
- Source must not be empty
- Source must not contain `..` (path traversal)

- [ ] **Step 1: Write failing tests**

```rust
#[test]
fn test_validate_config_relative_target_warns() {
    let package = PackageBuilder::default()
        .name("bad-dotfile")
        .version("1.0.0")
        .dotfiles(vec![DotfileEntry::new("src/file.txt", "relative/path.txt")])
        .build();
    let result = package.validate();
    assert!(result.issues().has_warnings() || result.issues().has_errors());
}

#[test]
fn test_validate_config_absolute_target_passes() {
    let package = PackageBuilder::default()
        .name("good-dotfile")
        .version("1.0.0")
        .dotfiles(vec![DotfileEntry::new("src/file.txt", "~/.config/file.txt")])
        .build();
    let result = package.validate();
    // No dotfile-related issues
    assert!(!result.issues().has_errors());
}
```

- [ ] **Step 2: Implement validation**

Add a `validate_dotfiles()` method to the validation chain. Check each `DotfileEntry`:

- If `source` is empty → error
- If `source` contains `..` → error
- If `target` doesn't start with `/` or `~` → warning

- [ ] **Step 3: Run tests and commit**

```bash
cargo fmt && cargo clippy --all-targets && cargo test
git add -A && git commit -m "feat: validate dotfile entries in package validation"
```

---

## Task 12: Update MCP server

**Files:**

- Modify: `crates/mcp-server/src/server.rs`

Add `selfie_apply` tool and update `selfie_spec_create`/`selfie_spec_update` param types to include
`dotfiles` and `post_install_note`.

- [ ] **Step 1: Add ApplyParam and tool**

```rust
#[derive(Deserialize, JsonSchema)]
pub struct ApplyParam {
    /// Specific package or dotfile name (deploys all if omitted)
    #[serde(default)]
    pub name: Option<String>,
    /// Show what would change without writing files
    #[serde(default)]
    pub dry_run: bool,
    /// Auto-accept overwrite for conflicts (MCP always uses true since no interactive prompt)
    #[serde(default = "default_true")]
    pub auto_accept: bool,
}
```

Add tool handler that calls `DotfileService::apply()` or `apply_all()`.

- [ ] **Step 2: Update create/update params**

Add `dotfiles` and `post_install_note` to `CreateParam` and `UpdateParam` where appropriate.

- [ ] **Step 3: Run tests and commit**

```bash
cargo fmt && cargo clippy --all-targets && cargo test
git add -A && git commit -m "feat: add selfie_apply MCP tool and update param types"
```

---

## Task 13: Semantic detection of existing rc file content

**Files:**

- Create: `crates/selfie/src/dotfile_service/semantic.rs`
- Modify: `crates/selfie/src/dotfile_service.rs`
- Modify: `crates/selfie/src/dotfile_service/service.rs`

When deploying to a shell config file (e.g., `~/.bashrc`, `~/.zshrc`), selfie scans the existing
file for lines related to the package being deployed. This is advisory — it warns but doesn't block
deployment.

- [ ] **Step 1: Write failing tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_find_related_lines_finds_eval_pattern() {
        let content = "# some config\neval \"$(fnm env)\"\n# more config\n";
        let matches = find_related_lines(content, "fnm");
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].line_number, 2);
    }

    #[test]
    fn test_find_related_lines_finds_source_pattern() {
        let content = "source ~/.config/fnm/init.sh\n";
        let matches = find_related_lines(content, "fnm");
        assert_eq!(matches.len(), 1);
    }

    #[test]
    fn test_find_related_lines_no_matches() {
        let content = "export PATH=/usr/bin:$PATH\n";
        let matches = find_related_lines(content, "fnm");
        assert!(matches.is_empty());
    }

    #[test]
    fn test_is_shell_config_file() {
        assert!(is_shell_config_path("~/.bashrc"));
        assert!(is_shell_config_path("~/.zshrc"));
        assert!(is_shell_config_path("~/.config/fish/config.fish"));
        assert!(!is_shell_config_path("~/.config/alacritty/alacritty.toml"));
    }
}
```

- [ ] **Step 2: Implement**

```rust
pub struct RelatedLine {
    pub line_number: usize,
    pub content: String,
}

/// Scan file content for lines related to a package name.
/// Looks for: the package name itself, `eval "$(name`, `source.*name`.
pub fn find_related_lines(content: &str, package_name: &str) -> Vec<RelatedLine> {
    content
        .lines()
        .enumerate()
        .filter(|(_, line)| {
            let line_lower = line.to_lowercase();
            let name_lower = package_name.to_lowercase();
            line_lower.contains(&name_lower)
        })
        .map(|(i, line)| RelatedLine {
            line_number: i + 1,
            content: line.to_string(),
        })
        .collect()
}

/// Check if a target path looks like a shell config file.
pub fn is_shell_config_path(target: &str) -> bool {
    let shell_patterns = [
        ".bashrc", ".bash_profile", ".zshrc", ".zprofile",
        ".profile", "config.fish", ".zshenv",
    ];
    shell_patterns.iter().any(|p| target.contains(p))
}
```

- [ ] **Step 3: Integrate into deployment flow**

In the dotfile service's apply logic, after loading the target file content (if it exists) and
before deploying: if `is_shell_config_path(target)`, call `find_related_lines()` and emit a
`Warning` event if matches are found, e.g.: "Found existing fnm-related content at lines 14-16. The
file you're deploying may conflict."

- [ ] **Step 4: Run tests and commit**

```bash
cargo fmt && cargo clippy --all-targets && cargo test
git add -A && git commit -m "feat: semantic detection of related content in shell rc files"
```

---

## Task 14: Update `selfie remove` to prompt about dotfile cleanup

**Files:**

- Modify: `crates/selfie/src/package/service/remove.rs`
- Modify: `crates/selfie/src/package/event.rs`
- Modify: `crates/cli/src/commands/spec/remove.rs`

When removing a package that has `dotfiles` entries, selfie should inform the user about deployed
dotfiles that may still exist at their target locations.

The library emits a new event; the CLI handles the actual prompt.

- [ ] **Step 1: Add ConfigCleanupInfo event variant**

In `event.rs`, add:

```rust
/// Info about dotfiles that may need cleanup after package removal
ConfigCleanupInfo {
    operation_info: OperationInfo,
    package_name: String,
    config_targets: Vec<String>,
},
```

- [ ] **Step 2: Emit event during remove**

In the remove service, after loading the package (before deleting), check if it has dotfiles. If so,
emit `ConfigCleanupInfo` with the list of target paths.

- [ ] **Step 3: Handle in CLI**

In the remove command handler, when receiving `ConfigCleanupInfo`, display:

```
ℹ Package 'fnm' has deployed dotfiles:
  - ~/.config/fish/conf.d/fnm.fish
  - ~/.config/zsh/conf.d/fnm.zsh
  These files were NOT removed. Delete them manually if no longer needed.
```

(Interactive deletion prompt can be a follow-up — for now, just inform.)

- [ ] **Step 4: Handle in MCP event collector**

Convert `ConfigCleanupInfo` to JSON in `event_to_json()`.

- [ ] **Step 5: Run tests and commit**

```bash
cargo fmt && cargo clippy --all-targets && cargo test
git add -A && git commit -m "feat: inform user about dotfiles when removing a package"
```

---

## Task 15: Final integration testing and cleanup

**Files:**

- All modified files

- [ ] **Step 1: Run full pre-commit checklist**

```bash
cargo fmt
dprint fmt
cargo clippy --all-targets
cargo test
```

All must pass with zero warnings.

- [ ] **Step 2: Manual smoke test**

Create a test package with dotfiles, run `selfie apply`, verify deployment.

- [ ] **Step 3: Final commit and push**

```bash
git add -A && git commit -m "chore: final Phase 2 cleanup"
git push
```

---

## Dependency Graph

```
Task 1 (deps) ─────────────────────────────────────────────────┐
Task 2 (schema) ──────────┬────────────────────────────────────┤
Task 3 (dotfiles dir) ────┤                                    │
Task 4 (events) ──────────┤                                    │
                           │                                    │
Task 5 (deploy state) ────┤                                    │
Task 6 (diff) ────────────┤                                    │
Task 7 (deploy logic) ────┤── requires 5, 6                    │
                           │                                    │
Task 8 (service) ──────────┤── requires 2, 3, 4, 5, 6, 7      │
Task 9 (CLI apply) ────────┤── requires 8                      │
Task 10 (post_install) ────┤── requires 2, 4                   │
Task 11 (validation) ──────┤── requires 2                      │
Task 12 (MCP) ─────────────┤── requires 2, 4, 8               │
Task 13 (semantic detect) ─┤── requires 8                      │
Task 14 (remove cleanup) ──┤── requires 2, 4                   │
Task 15 (integration) ─────┴── requires all                    │
```

Tasks 1-4 can be done first in any order. Tasks 5-7 can be parallelized. Tasks 10-14 can be
parallelized once their deps are met. Task 8 is the critical path.
