# TODOs & Guiding Principals

## Guiding Principals

- Thinking about lib vs app pieces...
  - Library stuff should:
    - use concrete errors
    - use static dispatch/generics
  - App stuff should:
    - use `anyhow` errors
    - use dynamic dispatch/generics
- Thinking about errors...
  - Each command could/should have its own error type. Some behavior across those commands will be
    the same (couldn't find package file, etc), but duplicating into one uber-error makes things
    complicated downstream.

## TODOs

### Phase 1: Base CLI

- [x] Add `clap` CLI app foundation
- [x] Add file system port and adapter

### Phase 2: Configuration Basics

- [x] Add YAML config file loader
- [x] Merge CLI flags with config file for runtime use
- [x] Add command runner port and shell command adapter
- [x] Add running `config validate`

### Phase 3: Package Basics

- [x] Add package domain
- [x] Add YAML package file loader
- [x] Add running `package validate [name]`
- [x] Add running `package list`
- [x] Add running `package info [name]`

#### 3.1 Package Improvements

- [x] Look to add error types that are specific to package commands
  - Consider error handling in the CLI, where `PackageRepoError` variants feel a bit strange
    depending on the command.
- [x] Move the `Reporter` stuff out of the `selfie` crate--this should be the job of the UI.

### Phase 4: Package Check Command

- [x] Add running `package check`

### Phase 5: Library Commands

I'm not liking how the library feels like a cobbling together of tools. It should provide clear
interfaces for each command and sub-command; add these interfaces to the library. To facilitate
this, add an event stream pattern implementation that includes:

1. [x] Rich, Structured Events — not just strings but typed data structures
2. [x] Bidirectional Communication — Support for control commands from UI to library
3. [x] Context/Metadata — Each event carries operation context/ID

### Phase 6: Package Create, Edit, Remove

To help get users started (even me), I want to easily add new and edit package files; via the CLI
this would be via `package create` and `package edit` commands.

- [x] Add `package create`
- [x] Add `package edit`
- [x] Add `package remove`

### Phase 7: Package Installation

- [ ] Add running `package install`
  - [ ] Run `check` before install
  - [ ] Run `install`

### Phase 8: Package Installation: Dependency Resolution

- [ ] Build a dependency graph
- [ ] Install dependencies of a package before installing the package

---

### Later

- [ ] Add `--dry-run` flag for `package install`
- [ ] Support `use:` to Environments (environment inheritance/fallback)
- [ ] Assess library for use of `pub`

### Ideas

#### After Adding `dialoguer` (2025-07-10)

#### Package Creation (create.rs) - Currently just a TODO

The `create` command is currently unimplemented. When implemented, it might benefit from:

- **Template selection**: `Select` to choose from different package templates
- **Environment setup**: `MultiSelect` for initial environments to configure
- **Confirmation**: `Confirm` before creating (similar to edit)

##### Package Installation (install.rs) - Currently just a TODO

The `install` command could use:

- **Dependency confirmation**: `Confirm` for installing dependencies
- **Environment selection**: `Select` if package supports multiple environments but current env
  isn't configured

##### Package List (list.rs) - Enhancement opportunity

Could add interactive CLI features (using `dialoguer`):

- **Action selection**: `Select` to choose actions on listed packages (edit, info, install, etc.)
- **Filtering**: `Input` for interactive filtering of package list

Interactive mode example:

```rust
let action = Select::with_theme(&SimpleTheme)
    .with_prompt("What would you like to do?")
    .items(&["Edit package", "Show info", "Install", "Validate", "Cancel"])
    .interact()?;
```

##### Error Recovery - Throughout the codebase

When operations fail, could offer recovery options:

- **Retry mechanisms**: `Confirm` to retry failed operations
- **Alternative actions**: `Select` menu for "What would you like to do?" scenarios

##### Configuration Management (config.rs)

Could add interactive config setup:

- **Initial setup wizard**: Series of `Input`/`Select` prompts for first-time users
- **Environment management**: `MultiSelect` for active environments

```rust
let package_dir = Input::with_theme(&SimpleTheme)
    .with_prompt("Package directory")
    .default(default_package_dir)
    .interact()?;

let environment = Input::with_theme(&SimpleTheme)
    .with_prompt("Default environment name")
    .default("macos")
    .interact()?;
```
