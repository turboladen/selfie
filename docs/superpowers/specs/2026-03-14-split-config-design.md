# Split AppConfig into Library and Frontend Configs

## Context

`AppConfig` lives in the `selfie` library but contains UI-specific fields (`verbose`, `use_colors`)
that the library never reads. This violates hexagonal architecture boundaries — the library
shouldn't know about presentation concerns. Additionally, the current
`#[serde(deny_unknown_fields)]` prevents the config file from containing frontend-specific sections.

The goal is to split config so the library owns operational settings, each frontend owns its
presentation settings, and they share a single config file with scoped sections.

## Config File Format

```yaml
# Core fields (library reads these)
environment: macos
package_directory: ~/.config/selfie/packages
command_timeout: 60
stop_on_error: true
max_parallel_installations: 4

# Frontend-specific sections (each frontend reads only its own)
cli:
  verbose: false
  use_colors: true
```

Core fields are top-level for simplicity. Each frontend gets a named section. The library ignores
unknown top-level keys (sections). Each frontend ignores sections that aren't its own.

## Types

### Library: `SelfieConfig`

Replaces `AppConfig`. Lives in `crates/selfie/src/config.rs`.

```rust
#[derive(Debug, Clone, Deserialize)]
pub struct SelfieConfig {
    environment: String,
    package_directory: PathBuf,

    #[serde(default = "default_command_timeout")]
    command_timeout: NonZeroU64,

    #[serde(default = "default_stop_on_error")]
    stop_on_error: bool,

    #[serde(default = "default_max_parallel")]
    max_parallel_installations: NonZeroUsize,
}
```

No `deny_unknown_fields` — allows `cli:`, `gui:`, etc. to coexist in the file without errors.

Getters, defaults, and validation carry over from `AppConfig` (minus `verbose`/`use_colors`).

### Library: `SelfieConfigBuilder`

Replaces `AppConfigBuilder`. Same pattern, minus `verbose`/`use_colors` fields.

### CLI: `CliSection`

New type in `crates/cli/src/config.rs`. Represents the `cli:` YAML section.

```rust
#[derive(Debug, Clone, Deserialize)]
pub struct CliSection {
    #[serde(default)]
    verbose: bool,

    #[serde(default = "default_use_colors")]
    use_colors: bool,
}
```

### CLI: `CliConfig`

New type in `crates/cli/src/config.rs`. Composes library config + CLI section.

```rust
pub struct CliConfig {
    selfie: SelfieConfig,
    cli: CliSection,
}
```

Methods:

- `selfie_config(&self) -> &SelfieConfig` — for passing to library service calls
- `verbose(&self) -> bool`
- `use_colors(&self) -> bool`
- Delegate all core getters: `environment()`, `package_directory()`, `command_timeout()`,
  `stop_on_error()`, `max_parallel_installations()` — these are mandatory, not optional, so CLI
  command handlers can change `&AppConfig` to `&CliConfig` with minimal diff

## Config Loading

### Library side

`ConfigLoader` trait changes return type from `AppConfig` to `SelfieConfig`.

`YamlLoader::load_config()` deserializes only core fields into `SelfieConfig`. Unknown keys
(frontend sections) are silently ignored.

`ApplyToConfig` is **removed from the library**. Override logic is a frontend concern.

### CLI side

The CLI loads config in two steps:

1. `SelfieConfig::load(&fs)` via `YamlLoader` — core config from file
2. CLI reads the same file for the `cli:` section — deserializes into `CliSection`
3. CLI flag overrides are applied to both `SelfieConfig` and `CliSection` fields
4. Assembled into `CliConfig`

For step 2, the CLI can use a wrapper struct for deserialization:

```rust
#[derive(Deserialize)]
struct RawCliFile {
    #[serde(default)]
    cli: CliSection,
}
```

This reads only the `cli:` key from the file and ignores everything else. If the `cli:` key is
missing, `#[serde(default)]` provides defaults. If it's present but malformed (e.g., `cli: "bad"`),
deserialization produces a user-facing error — this is correct behavior, not something to suppress.

### Reading the file twice

The library and CLI each read the config file independently for their own sections. This is simple,
correct, and avoids coupling. The file is small and local — double-reading is negligible.

An alternative would be a single raw parse that hands sections to each consumer, but that adds
complexity for no real benefit at this scale.

## CLI Flag Overrides

Handled entirely in the CLI crate. `ClapCli` applies overrides when constructing `CliConfig`:

- `--environment` → overrides `SelfieConfig.environment`
- `--package-directory` → overrides `SelfieConfig.package_directory`
- `--verbose` → overrides `CliSection.verbose`
- `--no-color` → overrides `CliSection.use_colors`

`SelfieConfig` needs mutable access for the CLI to apply core overrides. Options:

- Keep `environment_mut()` / `package_directory_mut()` on `SelfieConfig`
- Or use a builder to reconstruct with overrides

Mutable getters are simpler and match the current pattern.

## Validation

`SelfieConfig::validate()` validates core fields only (environment non-empty, package_directory
expandable). Same logic as today minus any UI field checks (there were none).

CLI can add its own validation on `CliSection` if needed (currently unnecessary).

The `config validate` command currently displays `verbose` and `use_colors` values. After the split,
it should receive `&CliConfig` and source those from the CLI section. It should also display the
`cli:` section values alongside core values.

## The `original_config` Pattern

Currently `main.rs` keeps both an overridden config and the raw file config (`original_config`) for
the `config validate` command. After the split, `config validate` should load the file fresh (via
`YamlLoader` + `RawCliFile`) to show raw values, rather than threading an "original" through the
entire dispatch chain. This simplifies the loading flow in `main.rs` — it only constructs one
`CliConfig` (the overridden one), and `config validate` handles its own file reading.

## Test Helpers (test-common)

- `test_config()` and variants return `SelfieConfig` (renamed from `AppConfig`)
- `AppConfigBuilder` → `SelfieConfigBuilder`
- CLI-specific test helpers can construct `CliConfig` wrapping a `SelfieConfig` from test-common
- Helpers that set `verbose` or `use_colors` (`test_config_verbose()`, `test_config_with_colors()`)
  move to CLI test utilities or are removed if unused outside CLI

## Public API Changes (selfie lib)

### Removed

- `AppConfig` (replaced by `SelfieConfig`)
- `AppConfigBuilder` (replaced by `SelfieConfigBuilder`)
- `ApplyToConfig` trait
- `verbose()` / `verbose_mut()` / `use_colors()` / `use_colors_mut()` getters

### Added

- `SelfieConfig` (same fields minus `verbose`/`use_colors`)
- `SelfieConfigBuilder`

### Changed

- `ConfigLoader::load_config()` returns `SelfieConfig`
- `YamlLoader::load_config()` returns `SelfieConfig`
- All service methods that take `&AppConfig` now take `&SelfieConfig`

## Files Modified

### Library (`crates/selfie/`)

- `src/config.rs` — rename `AppConfig` → `SelfieConfig`, remove `verbose`/`use_colors`, remove
  `deny_unknown_fields`, rename builder, update re-exports (this file is the module root — there is
  no separate `config/mod.rs`), remove `ApplyToConfig` re-export
- `src/config/loader.rs` — `ConfigLoader` returns `SelfieConfig`, remove `ApplyToConfig` trait
- `src/config/yaml.rs` — update return type, update tests
- `src/config/validate.rs` — update type references
- `src/lib.rs` — update re-exports if any reference `AppConfig`
- `src/package/service.rs` — `PackageServiceImpl` stores `AppConfig` as a field and clones it in
  `execute_operation_with_deps()`; rename to `SelfieConfig`
- `src/package/service/*.rs` (`check.rs`, `install.rs`, `list.rs`, etc.) — handler closures receive
  config; update type references

### CLI (`crates/cli/`)

- `src/config.rs` — remove `ApplyToConfig` impl, add `CliSection`, `CliConfig`, `RawCliFile`, and
  CLI-specific override logic
- `src/main.rs` — update config loading flow to use `CliConfig`
- `src/commands/` — all command handlers change `&AppConfig` to `&CliConfig` (not `&SelfieConfig`),
  since handlers like `format_environment_names` and `display_environment_summary` need both core
  fields and `use_colors()`. The delegation getters on `CliConfig` make this a straightforward type
  rename.

### Test Common (`crates/test-common/`)

- `src/config.rs` — rename to `SelfieConfig`/`SelfieConfigBuilder`, audit which helpers are still
  needed
- `src/lib.rs` — update re-export from `AppConfigBuilder` to `SelfieConfigBuilder`

## Verification

1. `cargo fmt` — formatting
2. `cargo clippy --all-targets` — zero warnings
3. `cargo test` — all existing tests pass
4. Manual: create a config file with a `cli:` section, verify the CLI reads it and the library
   ignores it
5. Manual: verify `--verbose` and `--no-color` flags still work
6. Manual: verify a config file without a `cli:` section still works (defaults apply)
7. Manual: verify `config validate` displays both core and CLI-specific settings
8. Update `CLAUDE.md` references to `AppConfig` in Boundary Rules and Key Abstractions sections
