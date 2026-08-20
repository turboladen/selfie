# Configuration Guide

This guide covers all aspects of configuring selfie for optimal use in your development environment.

## Overview

Selfie uses a configuration file to define global settings that apply across all package operations.
The configuration determines your current environment, package directory location, and other
behavioral settings.

## Configuration File Location

Selfie looks for configuration files in this order:

1. `~/.config/selfie/config.yaml` (primary format)
2. `~/.config/selfie/config.yml` (alternative format)

You can also override the configuration directory using the `SELFIE_CONFIG_DIR` environment
variable.

## Environment Variables

Selfie recognizes several environment variables that affect its behavior:

### `SELFIE_CONFIG_DIR`

Override the default configuration directory location. When set, selfie will look for configuration
files in this directory instead of `~/.config/selfie/`.

**Example:**

```bash
export SELFIE_CONFIG_DIR=/custom/config/path
selfie config validate
```

### `EDITOR`

Specifies which editor to use for the `selfie spec edit` command. This environment variable is
required when using package editing functionality.

**Example:**

```bash
export EDITOR=code    # Use VS Code
export EDITOR=vim     # Use Vim
export EDITOR=nano    # Use Nano

selfie spec edit my-package
```

If `EDITOR` is not set, the `selfie spec edit` command will fail with an error message instructing
you to set this environment variable.

If no configuration file is found, selfie does **not** create one. Commands still run when the flags
supply what the file would have: `--environment` and `--package-directory` have no default, so a run
that passes both needs no file at all. Without them the run fails, naming the settings that are
missing — see [Command-Line Overrides](#command-line-overrides). To use a file instead, write
`~/.config/selfie/config.yaml` yourself, or point `SELFIE_CONFIG_DIR` at a directory that has one.

## Basic Configuration

### Minimal Configuration

The simplest configuration requires only two settings:

```yaml
environment: macos
package_directory: ~/.config/selfie/packages
```

### Full Configuration Example

```yaml
# Current environment name
environment: macos

# Directory containing package definition files
package_directory: ~/.config/selfie/packages

# Directory for standalone dotfile definitions (default: sibling of package_directory)
dotfiles_directory: ~/.config/selfie/dotfiles

# Directory for deploy state tracking (default: ~/.local/state/selfie)
state_directory: ~/.local/state/selfie

# Command timeout in seconds (default: 60)
command_timeout: 300

# Stop on first error (default: true)
stop_on_error: true

# Maximum concurrent operations (default: number of CPUs)
max_concurrency: 4

# Presentation settings, read only from this section
cli:
  # Verbosity level (default: false)
  verbose: false

  # Use colored output (default: true)
  use_colors: true
```

`verbose` and `use_colors` are read **only** from the `cli:` section. Written at the top level they
are ignored — selfie says so on every run, but the setting does nothing.

## Required Settings

### `environment`

Specifies which environment configuration to use when installing packages. This must match an
environment name defined in your package files.

```yaml
environment: macos
```

**Common environment names:**

- `macos`, `macos-work`, `macos-home` - macOS systems with context
- `ubuntu`, `debian`, `fedora`, `arch` - Linux distributions
- `linux-dev`, `linux-ci` - Linux with context
- `ci`, `github-actions` - CI/CD environments
- `dev`, `staging`, `prod` - Deployment environments

### `package_directory`

Path to the directory containing your package definition files. Can be absolute or relative to your
home directory.

```yaml
package_directory: ~/.config/selfie/packages
```

**Examples:**

```yaml
# Absolute path
package_directory: /home/user/my-packages

# Relative to home directory
package_directory: ~/dev-packages

# Using environment variables
package_directory: ${SELFIE_PACKAGES:-~/.config/selfie/packages}
```

## Optional Settings

### Dotfile Deployment

#### `dotfiles_directory`

Path to the directory containing standalone dotfile definitions — YAML files and their associated
source files for dotfiles not tied to any package. If not set, selfie looks for a `dotfiles`
directory as a sibling of `package_directory`.

```yaml
dotfiles_directory: ~/.config/selfie/dotfiles
```

**Default behavior without this setting:**

```
# If package_directory is ~/.selfie/packages,
# dotfiles_directory defaults to ~/.selfie/dotfiles
```

If you **set** this and the directory does not exist, selfie says so and carries on without your
standalone dotfiles — they are simply absent from `apply`, `dotfiles drift` and `dotfiles list`, and
a new spec's name is not checked against them. If you do **not** set it and the default sibling does
not exist, selfie says nothing: that is the ordinary state of a setup with no standalone dotfiles.

`dotfiles track` is the exception. It copies the file _into_ that directory, so it refuses rather
than warning.

Standalone dotfiles live here with their source files colocated alongside their YAML definitions.
Package dotfiles live alongside their package YAML in `package_directory` instead. See
[Package Files Reference](package-files.md#dotfile-deployment) for details.

#### `state_directory`

Path where selfie stores deploy state (checksums of deployed files, used for conflict and drift
detection). Defaults to `~/.local/state/selfie` — the location the
[XDG Base Directory Specification](https://specifications.freedesktop.org/basedir-spec/latest/)
gives for `XDG_STATE_HOME`. Only the path is taken from the specification; the variable itself is
never read, so exporting `XDG_STATE_HOME` moves nothing. Set `state_directory` here, or pass
`--state-directory`, to put the state file elsewhere.

```yaml
state_directory: ~/.local/state/selfie
```

The state file (`deploy-state.yml`) is per-machine — it tracks what was deployed on _this_ machine
and is not meant to be shared or version-controlled.

If the file exists but cannot be read or parsed, selfie says so — naming the file and whether it was
the read or the contents that failed — and then proceeds as though nothing had been deployed. Every
tracked dotfile looks untracked for that run, which turns routine applies into conflict prompts.
**Answering them writes a fresh state file over the unusable one**, so anything that was salvageable
in it is gone; if the contents matter to you, copy the file aside before running `selfie apply` or
`selfie dotfiles track` again. Preserving it automatically is tracked, deferred rather than
overlooked. An **absent** state file is the ordinary first-run case and is not reported.

On Unix it is written readable only by its owner (mode `0600`). On Windows it inherits the parent
directory's ACL, and on any other platform it gets default permissions — treat owner-only as a Unix
guarantee, not a portable one. Its contents are not credentials, but they name each repository-file
dotfile selfie manages here, which is a useful map to anyone else with an account on the machine.

Provider-sourced and templated dotfiles are not recorded at all, so this is not a complete list of
what selfie manages — see
[No deploy state, and what follows from it](package-files.md#no-deploy-state-and-what-follows-from-it).

### Global Behavior

#### `cli.verbose`

Print the extra per-step detail that `--verbose` prints. Lives under `cli:`, not at the top level.

```yaml
cli:
  verbose: true
```

It does **not** turn on the `DEBUG` log lines: the tracing level is chosen from the `--verbose` flag
alone, before the configuration file is read, so those come only from passing the flag.

#### `cli.use_colors`

Control colored output. Lives under `cli:`, not at the top level.

```yaml
cli:
  use_colors: false
```

#### `command_timeout`

Default timeout for package operations in seconds.

```yaml
command_timeout: 600 # 10 minutes
```

#### `stop_on_error`

Whether to stop on first error during operations.

```yaml
stop_on_error: false
```

#### `max_concurrency`

Maximum number of concurrent operations for bulk commands (list, audit, install recommends) and
dependency/recommend status checks. Defaults to the number of CPUs.

```yaml
max_concurrency: 2
```

## Environment Naming Strategies

Environment names can be simple OS identifiers or context-specific:

```yaml
# Simple OS-based naming
environment: macos

# Context-specific naming for different scenarios
environment: macos-work # Work laptop configuration
environment: macos-home # Personal machine configuration
environment: ubuntu-dev # Development server
environment: ci-github # GitHub Actions environment
```

This allows you to have different package installation preferences for different contexts even on
the same OS.

## Command-Line Overrides

Settings are resolved in this order, highest first:

1. Command-line flags
2. The configuration file
3. Built-in defaults

```bash
# Override environment
selfie --environment=linux package install node

# Override package directory
selfie --package-directory=/path/to/packages package list

# Override the dotfiles and deploy-state directories
selfie --dotfiles-directory=/path/to/dotfiles --state-directory=/path/to/state apply

# Enable verbose mode
selfie --verbose package install docker

# Disable colors
selfie --no-color package list
```

These flags are global, so they work on either side of the subcommand:
`selfie -p /path/to/packages package list` and `selfie package list -p /path/to/packages` are the
same run.

Six things this order does not mean:

**A configuration file is optional, but two settings are not.** With no config file anywhere selfie
searched, it runs from the flags alone — `environment` and `package_directory` have no default, so
those two must be supplied:

```bash
selfie --environment macos --package-directory ~/selfie/packages package list
```

Everything else keeps its default, so a flags-only run and a two-key config file produce the same
settings: `dotfiles_directory` falls back to a sibling of the package directory and
`state_directory` to `~/.local/state/selfie`. Supply neither flag and selfie names **both**, along
with the directory it searched.

This applies only to a file that is **absent**. A config file that exists but cannot be read, cannot
be parsed, or is not a regular file is still an error — the flags do not paper over a file you are
in the middle of editing.

Nor do flags fill _gaps_ in a file that exists. A configuration file must carry both `environment`
and `package_directory` itself; a file holding only a `cli:` section fails even with both flags
supplied. Flags override settings that are present and stand in for a file that is not there — they
are not merged into a partial one. Making them merge is tracked separately.

**The `cli:` booleans only move one way.** `--verbose` turns verbose on and `--no-color` turns
colors off; neither has an opposite. `verbose: true` or `use_colors: false` under `cli:` therefore
cannot be overridden from the command line — edit the file. Written at the top level instead of
under `cli:` they are ignored entirely, and selfie reports them. `--verbose` is also the wider of
the two: it selects the `DEBUG` tracing level as well, which `cli: verbose: true` does not, because
the tracing level is chosen from the flag before the file is read.

**`SELFIE_CONFIG_DIR` and the path flags do not compete.** The variable chooses _which file_ is
read; the flags override _fields_ in whatever file that was. Setting both is normal, and the flag
still wins for the field it names.

**A flag value is not processed the way the same value in the file is.** `~` is expanded for the
path settings in the configuration file; a flag value is used exactly as typed. Your shell expands a
bare `~/packages`, but neither bash nor zsh expands `--package-directory=~/packages`, so that form
reaches selfie as the literal string and fails with `Package directory not found: ~/packages` even
though the identical value works in the file. Use the separated form (`-p ~/packages`) or an
absolute path. Flag values also skip the absolute-path check `selfie config validate` applies to the
file.

Only `--package-directory` fails loudly, and the other two fail differently from each other:

- `--state-directory='~/state'` **creates a directory literally named `~`** in the current working
  directory and reports success, so `selfie --state-directory='~/state' apply -y` exits 0 having
  written its deploy state somewhere nobody will look for it.
- `--dotfiles-directory='~/dotfiles'` creates nothing, so the standalone dotfiles repository is
  dropped: every standalone dotfile disappears from `selfie dotfiles list` and is skipped by
  `selfie apply`, which still reports success. selfie warns once on stderr naming the directory,
  because the path was given rather than defaulted — the run's exit status does not change.

**`selfie config validate` reports the file, not the effective settings.** It deliberately reloads
what is on disk and applies no overrides, including to `verbose` and `use_colors`, so that a flag
cannot hide a problem in the file it is masking. It therefore still fails when there is no config
file at all, even on a run that would otherwise succeed from flags — there is no file for it to
report on. Passing `-p` and reading back the file's `package_directory` is expected — it is not the
flag being ignored. Use `selfie package list`, which prints the package directory it actually read,
to see the effective value.

**Two paths are not covered by any flag.** A dotfile `target` beginning with `~`, and the
deploy-state fallback used when no `state_directory` is configured, both resolve against `HOME`.

## Running Under `sudo`

The commands in the table below refuse to run under `sudo`. `--allow-sudo` overrides that for the
case where you mean it. Everything else is unaffected — see
[what is not refused](#what-is-not-refused).

What is refused is running as a **different user than the one who invoked selfie**. That includes
`sudo -u alice`, which is not root at all and does the same kind of damage with a different owner on
the files. Running as root _without_ `sudo` — a container, a CI job, or root managing root's own
dotfiles — is not affected and needs no flag, and neither is a process that merely inherited
`SUDO_UID` from a session running as you.

| command                                            | why it is refused                                                                                                       |
| -------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------- |
| `apply`                                            | the whole run is written by the other user, including the entries under your home directory                             |
| `track`, `dotfiles track`, `package track-dotfile` | the same, plus a spec and deploy state you can no longer rewrite                                                        |
| `sync push`, `sync pull`                           | commits, fetches and merges as that user, leaving objects, refs and index entries you do not own in a repository you do |

The sync case is the one that does not repair itself. A deploy-state file owned by the wrong user is
replaced on the next successful run, because it is written from a temporary file you own; git
objects are not, and the next ordinary `git` fails on them.

### What is not refused

Read-only commands — `dotfiles drift`, `dotfiles list`, `sync status`, `package status`, everything
under `spec` — are unaffected: they write nothing. So is `package install`, even though it very much
writes: the commands it runs are yours, and some of them genuinely need `sudo`.

```bash
sudo selfie --allow-sudo apply
```

It is the one flag with **no** configuration-file equivalent, deliberately — a `cli:` setting that
turned the guard off permanently would defeat a guard whose entire value is that it fires on the run
you did not think through.

## Configuration Validation

Validate your configuration file:

```bash
selfie config validate
```

This checks:

- YAML syntax
- Required fields presence
- Path accessibility
- Environment name validity
- Repository connectivity (if configured)

## Troubleshooting

### Ignored Configuration Keys

```
⚠ `configs_directory` was renamed to `dotfiles_directory` and is no longer read. Selfie ignored it.
```

Selfie loads a configuration file that contains keys it does not recognize, and reports each one
before the command runs. This is a **warning, not an error** — the rest of the file is still used
and the command still runs. A key that was renamed says what replaced it; anything else is reported
as unrecognized.

`selfie config validate` lists the same keys and does not call such a file valid.

**Solution:** rename or remove the key. Supported top-level keys are `environment`,
`package_directory`, `dotfiles_directory`, `state_directory`, `command_timeout`, `stop_on_error` and
`max_concurrency`; `verbose` and `use_colors` go under `cli:`.

A stray key under `cli:` is reported as `cli.<key>`. A `cli:` section that is not a mapping —
`cli: true` — is reported too, and selfie falls back to the default CLI settings for that run. An
empty `cli:` is fine and says nothing.

### Configuration File Is Not a Regular File

```
Error: …/config.yaml: the configuration file is a named pipe (fifo), not a regular file.
```

Selfie refuses a configuration file that is not a regular file. Replace it with a regular file or
remove it.

### EDITOR Environment Variable Not Set

```
Error: EDITOR environment variable is not set.
```

**Solution:** Set the EDITOR environment variable to your preferred editor:

```bash
# Temporarily for current session
export EDITOR=code    # VS Code
export EDITOR=vim     # Vim
export EDITOR=nano    # Nano

# Permanently in your shell profile (~/.bashrc, ~/.zshrc, etc.)
echo 'export EDITOR=code' >> ~/.bashrc
```

This environment variable is required for the `selfie spec edit` command.

## Best Practices

1. **Version control**: Keep configuration files in version control
2. **Environment separation**: Use different configurations for different environments
3. **Minimal configuration**: Start with minimal settings, add complexity as needed
4. **Documentation**: Comment your configuration files
5. **Validation**: Regularly validate configuration with `selfie config validate`
6. **Backup**: Keep backups of working configurations
7. **Team consistency**: Use shared configuration templates for teams
8. **Security**: Never commit sensitive data like tokens to version control
