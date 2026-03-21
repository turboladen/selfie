# Selfie Environment Manager Design

## Context

Selfie is a personal meta-package manager that runs user-configured install/check commands per
environment. While the package management works, users hit a critical gap: **shell environment
configuration**. Tools like `fnm`, `uv`, `bun`, and `chruby` require shell-specific init code (PATH
updates, `eval` statements, `source` commands), and this config differs per shell (fish, bash, zsh).
Currently, this config is either embedded in install commands (fragile, non-idempotent) or managed
separately by yadm (disconnected from the package that needs it).

The root cause is that selfie treats "install a tool" and "configure your shell to use that tool" as
one operation, when they're two distinct concerns. This design adds **config file management** as a
first-class selfie concept, turning selfie into a unified environment manager that handles both
packages and their associated dotfiles — ultimately replacing yadm.

### Goals

1. Packages can declare associated config files (shell configs, app configs, etc.)
2. Config files are deployed separately from installation (`apply` vs `install`)
3. Standalone config files (not tied to any package) can be tracked
4. Soft dependencies (`recommends`) that don't cascade failure
5. Git-backed cross-machine sync (the selfie packages repo IS the dotfile repo)
6. Cautious by default — show changes, prompt before writing, support `--yes` for scripting

### Non-Goals (for now)

- Template system (yadm's `##template` — selfie's environments handle per-machine differences)
- Encryption (yadm's `yadm encrypt`)
- Full git client (selfie wraps common operations, not all of git)

---

## Package YAML Schema Changes

### Current schema

```yaml
name: fnm
version: "1.0.0"
description: Fast Node.js version manager
homepage: https://github.com/Schniz/fnm
environments:
  macos-home:
    install: brew install fnm
    check: command -v fnm
    audit: brew list --versions fnm
    dependencies:
      - homebrew
```

### New fields

```yaml
name: fnm
version: "1.0.0"
description: Fast Node.js version manager
homepage: https://github.com/Schniz/fnm
environments:
  macos-home:
    install: brew install fnm
    check: command -v fnm
    audit: brew list --versions fnm
    dependencies:
      - homebrew
    recommends: # NEW: soft dependencies
      - nodejs
configs: # NEW: associated config files
  - source: fnm/fish-conf.fish # resolves to packages/fnm/fish-conf.fish
    target: ~/.config/fish/conf.d/fnm.fish
  - source: fnm/zsh-conf.zsh # resolves to packages/fnm/zsh-conf.zsh
    target: ~/.config/zsh/conf.d/fnm.zsh
post_install_note: | # NEW: first-time setup guidance
  Configure your shell for fnm. See: https://github.com/Schniz/fnm#shell-setup
  Fish: fnm env --use-on-cd --shell fish | source
  Bash: eval "$(fnm env --use-on-cd --shell bash)"
  Zsh:  eval "$(fnm env --use-on-cd --shell zsh)"
```

### Field details

**`configs`** (optional, top-level — not per-environment):

- `source`: Path relative to the **parent directory** of the YAML file (`packages/` for packages,
  `configs/` for standalone configs). For example, in `packages/fnm.yaml`, a source of
  `fnm/fish-conf.fish` resolves to `<packages_dir>/fnm/fish-conf.fish`. In `configs/dprint.yaml`, a
  source of `dprint/dprint.jsonc` resolves to `<configs_dir>/dprint/dprint.jsonc`.
- `target`: Absolute path where the file should be deployed on the user's machine. Supports `~`
  expansion.
- Configs are NOT environment-specific — the same config files apply regardless of environment. If a
  config truly differs per-environment, use separate packages or conditional content within the
  file. For shell-specific configs (fish vs bash vs zsh), list each as a separate config entry with
  its own source and target — the user deploys all shells they use.

**Configuration requirement:** `SelfieConfig` gains a `configs_directory` field (defaults to sibling
of `package_directory`, e.g., if `package_directory` is `~/selfie-packages/packages`, then
`configs_directory` defaults to `~/selfie-packages/configs`). This can be overridden in config.

**`recommends`** (optional, per-environment):

- List of package names, same format as `dependencies`.
- Installed after the package succeeds. Individual failures produce warnings, not errors.
- The parent package reports success regardless of recommend outcomes.
- `selfie install fnm --no-recommends` skips them entirely.

**`post_install_note`** (optional, top-level):

- Freeform text shown to the user after a successful install where the check command was failing
  before install (i.e., the package was not already installed). Shown regardless of whether this is
  the user's first-ever install or a reinstall.
- Intended for shell configuration guidance.
- Not shown when the package was already installed (check passed before install).

---

## Config File Deployment

### Deployment method: Copy with sync

Config files are **copied** from the repo to their target locations (not symlinked). Selfie tracks
checksums to detect drift between the repo version and the deployed version.

### Conflict detection

When deploying a config file whose target already exists:

1. Compute checksum of existing target file.
2. Compare against the repo source's checksum.
3. If they match: no-op (already deployed and unchanged).
4. If they differ:
   - Show a diff (using the `similar` crate).
   - Prompt the user:
     - **(a) Overwrite** with repo version.
     - **(b) Keep current** target, skip deployment.
     - **(c) Update repo** — copy the target back into the repo (pull changes from this machine).
     - **(d) Show diff** — display the full diff before choosing.
5. `--yes` flag auto-selects overwrite. `--dry-run` shows what would change without doing anything.

### Semantic detection for existing rc files

When deploying to a shell config file (e.g., `~/.bashrc`, `~/.zshrc`), selfie performs a basic scan
for related content:

- Search for lines containing the package name or known patterns (e.g., `eval "$(fnm`,
  `source.*fnm`).
- If found, warn the user: "Found existing fnm-related config at lines 14-16. The file you're
  deploying may conflict."
- This is advisory, not blocking.

### Checksum tracking

Selfie maintains a machine-local state file at `~/.config/selfie/deploy-state.yml` (NOT in the repo
— this is per-machine state) that records:

```yaml
deployed:
  - source: fnm/fish-conf.fish
    target: ~/.config/fish/conf.d/fnm.fish
    source_checksum: abc123
    deployed_checksum: abc123
    deployed_at: 2026-03-18T10:00:00Z
```

This lets selfie detect:

- **Repo changed, target unchanged** → safe to update (repo has newer version).
- **Target changed, repo unchanged** → user edited the deployed file (offer to sync back).
- **Both changed** → conflict (show diff, ask user).

---

## Standalone Configs

For files not tied to any package (e.g., `.dprint.jsonc`, `.vale.ini`), selfie supports standalone
config definitions.

### Tracking a standalone config

```bash
selfie dotfiles track dprint ~/.dprint.jsonc
```

This:

1. Copies `~/.dprint.jsonc` into the repo at `configs/dprint/dprint.jsonc`.
2. Creates `configs/dprint.yaml`:

```yaml
name: dprint
version: "1.0.0"
configs:
  - source: dprint/dprint.jsonc
    target: ~/.dprint.jsonc
```

### Adding a config to an existing package

```bash
selfie package track-config fnm ~/.config/fish/conf.d/fnm.fish
```

This:

1. Copies the file into the repo at `packages/fnm/fish-conf.fish` (derives a filename from the
   source path).
2. Updates `packages/fnm.yaml` to add the file to its `configs` section.

### Convenience shortcut

```bash
selfie track ~/.config/alacritty/colors.toml
```

Prompts:

```
Tracking ~/.config/alacritty/colors.toml
? Does this belong to an existing package?
  > alacritty (package)
  > Create new standalone config
  > Let me type a name
```

### Promotion path

A standalone config can be promoted to a full package by adding `environments` with install/check
commands and moving it from `configs/` to `packages/`. The YAML schema is the same — packages just
have more fields filled in.

### Shared namespace

Packages and configs share one namespace. You cannot have `configs/foo.yaml` and
`packages/foo.yaml`. This avoids ambiguity in commands like `selfie apply foo`.

---

## Shell Behavior: Login Shell Execution

### The change

Selfie runs all install/check/audit commands in a **login shell** that sources the user's shell
profile. This ensures that deployed config files (which may set PATH, define aliases, etc.) are
available to subprocesses.

The MCP server already uses `ShellCommandRunner::login_shell()` for this reason. The CLI should do
the same for install/check/audit operations.

### Why this solves temporal coupling

On a new machine:

1. `selfie apply` deploys all config files (including `~/.config/fish/conf.d/fnm.fish`).
2. `selfie install fnm` runs in a login shell → sources `config.fish` → sources `conf.d/fnm.fish` →
   ... wait, fnm isn't installed yet, so the init might error.

**Important edge case:** On first install, the config file exists (deployed by `apply`) but the tool
isn't installed yet. The shell init code (e.g., `fnm env | source`) will error because `fnm` isn't
in PATH yet.

**Mitigation:** Well-written shell config should guard against missing commands:

```fish
# In fnm.fish
if command -v fnm &>/dev/null
    fnm env --use-on-cd --shell fish | source
end
```

This is already best practice for shell configs (don't error on missing tools). Selfie's
documentation and `post_install_note` should encourage this pattern.

After install completes: 3. `selfie install node` (depends on fnm) runs in a login shell → fnm.fish
loads → fnm is now in PATH → `fnm install` succeeds.

### First-time setup (no config files in repo yet)

When a user installs a tool for the first time ever (no config files tracked yet):

1. `selfie install fnm` runs the install command.
2. If `post_install_note` exists, selfie displays it.
3. Selfie optionally pauses (configurable or prompted): "fnm was installed. Configure your shell,
   then press Enter to continue."
4. User adds shell config manually (following the note).
5. User presses Enter. Selfie re-runs check in a login shell (which now sources the new config).
6. User runs `selfie package track-config fnm ~/.config/fish/conf.d/fnm.fish` to capture it.

On subsequent machines, the config file is already in the repo, so `apply` + `install` just works.

---

## Soft Dependencies (Recommends)

### Behavior during install

When `selfie install neovim` runs:

1. Resolve hard dependencies (homebrew). Fail if any hard dep fails.
2. Install neovim. Fail if install fails.
3. Attempt each recommend (rust-analyzer, lua-language-server, stylua).
4. Report results:
   ```
   ✓ neovim installed successfully
   ✓ rust-analyzer installed
   ✗ lua-language-server failed (see above for details)
   ✓ stylua installed

   1 recommended package failed. neovim is installed and functional.
   ```

### CLI flags

- `selfie install neovim` — installs package + all recommends (default).
- `selfie install neovim --no-recommends` — installs package only, skips recommends.

### Dependency resolution

Recommends participate in cycle detection but not in failure propagation. A recommend that has unmet
hard dependencies will fail individually without affecting the parent.

---

## Git-Backed Sync

### Repository structure

The selfie packages directory is a git repo:

```
selfie-packages/             # git repo
├── packages/                # package definitions + their config files
│   ├── fnm.yaml
│   ├── fnm/                 # fnm's config files
│   │   ├── fish-conf.fish
│   │   └── zsh-conf.zsh
│   ├── alacritty.yaml
│   ├── alacritty/           # alacritty's config files
│   │   ├── alacritty.toml
│   │   └── colors.toml
│   └── ...
└── configs/                 # standalone config definitions + their files
    ├── dprint.yaml
    ├── dprint/
    │   └── dprint.jsonc
    ├── nvim.yaml
    ├── nvim/
    │   ├── init.lua
    │   └── ...
    └── ...
```

### Sync commands

```bash
selfie sync push             # prompts for commit message, stages all changes, commits, pushes
selfie sync pull             # git pull (surfaces conflicts if any, user resolves with git)
selfie sync status           # git status + show which deployed files have drifted
```

`selfie sync push` stages all modified/new files in the repo, prompts for a commit message (with a
sensible default like "Update configs"), commits, and pushes. `--message "..."` skips the prompt. If
there are no changes, it reports "nothing to push."

`selfie sync pull` does NOT auto-apply. After pulling, it shows what changed and suggests
`selfie apply` if config files were updated.

### New machine workflow

```bash
# 1. Install selfie
cargo install selfie-cli     # or curl-based installer

# 2. Clone packages repo
git clone git@github.com:user/selfie-packages.git ~/selfie-packages

# 3. Configure selfie
selfie config set package_directory ~/selfie-packages/packages
selfie config set environment macos-home

# 4. Deploy configs first, then install packages
selfie apply                 # deploys all config files to target locations
selfie install --all         # installs all packages for current environment
```

---

## CLI Commands (New and Modified)

### New commands

| Command                                    | Description                                                |
| ------------------------------------------ | ---------------------------------------------------------- |
| `selfie apply`                             | Deploy all config files from repo to target locations      |
| `selfie apply <name>`                      | Deploy config files for a specific package or config       |
| `selfie apply --dry-run`                   | Show what would change without writing                     |
| `selfie dotfiles track <name> <file>`      | Track a standalone config file                             |
| `selfie package track-config <pkg> <file>` | Add a config file to a package                             |
| `selfie track <file>`                      | Interactive shortcut — asks if standalone or package-owned |
| `selfie sync push`                         | Commit and push the packages repo                          |
| `selfie sync pull`                         | Pull changes from remote                                   |
| `selfie sync status`                       | Show local vs remote differences                           |

### Modified commands

| Command                          | Change                                                                                             |
| -------------------------------- | -------------------------------------------------------------------------------------------------- |
| `selfie install <pkg>`           | Runs in login shell. Shows `post_install_note`. Installs recommends. Does NOT deploy config files. |
| `selfie install --all`           | Installs all packages for current environment. Shows summary with recommend failures as warnings.  |
| `selfie install --no-recommends` | Skips soft dependencies.                                                                           |
| `selfie remove <pkg>`            | Prompts to also remove associated config files from target locations (not from repo).              |
| `selfie check <pkg>`             | Could report "config drift" (deployed file differs from repo).                                     |

---

## Service Layer Design

### New traits and methods

Config deployment is a new concern that doesn't belong on `PackageService`. Following hexagonal
architecture, introduce a new port:

```
ConfigService (trait)
├── apply_all_configs()         → EventStream   # deploy all configs
├── apply_configs_for(name)     → EventStream   # deploy configs for one package/config
├── check_drift()               → EventStream   # report which deployed files have drifted
├── track_file(name, path)      → Result        # create a standalone config definition
├── track_file_for_package(pkg, path) → Result  # add a config file to an existing package
```

`PackageService` gains:

- `recommends` resolution in the install flow (modify existing `install_packages` method).
- No other new methods — install stays on `PackageService`, config deployment stays on
  `ConfigService`.

### New `PackageEvent` variants

```
ConfigDeploying { name, source, target }         # about to deploy a file
ConfigDeployed { name, source, target }           # file deployed successfully
ConfigSkipped { name, source, target, reason }    # file skipped (already current, user declined)
ConfigConflict { name, source, target, diff }     # conflict detected, awaiting user decision
ConfigDriftDetected { name, target, drift_type }  # deployed file differs from repo
PostInstallNote { package_name, note }            # display post-install guidance
RecommendStarted { package_name }                 # starting a soft dependency install
RecommendSucceeded { package_name }               # soft dep installed
RecommendFailed { package_name, error }           # soft dep failed (not fatal)
ConfigTracked { name, source, target }            # file tracked successfully
```

The CLI's `EventProcessor` handles these via new handler functions. The MCP server's
`McpEventCollector` converts them to structured JSON.

### Config discovery

`selfie apply` discovers configs by:

1. Scanning all YAML files in `packages/` — extract `configs` entries from each.
2. Scanning all YAML files in `configs/` — extract `configs` entries from each.
3. Both use the same `Package` deserialization (configs are just packages without `environments`).

This means `PackageRepository` (or a new `ConfigRepository`) needs to know about both directories.
The simplest approach: extend `YamlPackageRepository` to accept multiple directories, or introduce a
`ConfigRepository` that reads from `configs/` and returns the same `Package` struct.

### Recommends in dependency resolution

The existing `deps::resolve_dependencies` resolves hard dependencies. For recommends:

- Cycle detection considers the union of `dependencies` and `recommends` (a recommend that creates a
  cycle with hard deps is invalid).
- After hard deps are resolved and installed, recommends are resolved and installed independently.
- Each recommend's failure is isolated — it emits `RecommendFailed` but does not affect the parent.

---

## Implementation Phases

This design is large. Recommended phasing:

### Phase 1: Recommends + Login Shell

- Add `recommends` field to package YAML schema and `EnvironmentConfig`.
- Update install flow to handle soft deps (attempt, warn on failure, don't cascade).
- Add `--no-recommends` flag.
- Switch CLI's `ShellCommandRunner` to use login shell for install/check/audit (same as MCP server
  already does). This is bundled here because it's a small change and is a prerequisite for the
  config deployment workflow in Phase 2.
- Test and document implications (slower shell startup, profile errors).

### Phase 2: Config file deployment

- Add `configs` and `post_install_note` fields to package YAML schema.
- Add `repo_root` derivation to `SelfieConfig`.
- Introduce `ConfigService` trait and implementation.
- Implement `selfie apply` command with conflict detection and diffing (`similar` crate).
- Implement checksum tracking (`~/.config/selfie/deploy-state.yml`).
- Update `selfie install` to show post_install_note and NOT auto-deploy configs.
- Update `selfie remove` to prompt about config cleanup.
- Semantic detection of existing config in rc files.

### Phase 3: Config tracking

- Add `configs/` directory support alongside `packages/`.
- Implement `selfie dotfiles track`, `selfie package track-config`, `selfie track`.
- Implement standalone config YAML definitions.
- Shared namespace validation across `packages/` and `configs/`.

### Phase 4: Git sync

- Implement `selfie sync push/pull/status`.
- Document the new-machine workflow.
- Migration guide from yadm.

---

## Key Files to Modify

### selfie library (`crates/selfie/`)

- `src/package.rs` — Add `configs`, `recommends`, `post_install_note` to `Package` and
  `EnvironmentConfig` structs.
- `src/package/service/install.rs` — Handle recommends in install flow. Emit post_install_note
  events.
- `src/package/validate.rs` — Validate new fields.
- `src/package/repository/yaml.rs` — Deserialize new fields.
- `src/command.rs` — Add login shell option to `ShellCommandRunner`.
- New: `src/config_deploy.rs` — Config file deployment logic (copy, checksum, conflict detection).
- New: `src/config_deploy/diff.rs` — Diffing with the `similar` crate.

### selfie-cli (`crates/cli/`)

- New command: `apply` — deploy config files.
- New command: `sync` — git operations on packages repo.
- New command: `track` — convenience config tracking.
- Modified: `install` — login shell, post_install_note display, recommends handling.
- Modified: `remove` — prompt about config file cleanup.

### selfie-mcp (`crates/mcp-server/`)

- Update tool descriptions for new fields.
- Add tools for config tracking and apply operations.

---

## Verification

### Unit tests

- Recommends: install succeeds when a recommend fails.
- Config deployment: conflict detection, checksum tracking, overwrite/skip/sync-back.
- Package validation: new fields validated correctly.
- Standalone config creation and promotion.

### Integration tests

- Full install flow with recommends (mock command runner).
- Config deploy + drift detection cycle.
- Login shell command execution.

### Manual testing

- Create a package with configs, install it, verify configs are NOT deployed.
- Run `selfie apply`, verify configs are deployed.
- Modify deployed file, run `selfie apply` again, verify conflict detection.
- `selfie dotfiles track` a standalone file, verify YAML created.
- `selfie install` with recommends, verify partial failure handling.
