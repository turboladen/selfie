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

### How the file is read

Each setting is read as its own type:

- `stop_on_error` takes `true` or `false`. `1` and `0` are refused.
- `command_timeout` and `max_concurrency` take whole numbers, so `60.0` is refused.
- `environment` is text exactly as written, so `environment: 010` names the environment `010`.

A YAML merge key (`<<: *anchor`) is merged into the mapping it sits in. A key spelled with a dot,
such as `cli.verbose: true`, is a key of that name and not a path: selfie reports it as an
unrecognized setting and does not read it as `verbose` under `cli:`.

When the file cannot be parsed, selfie names the kind of problem and the line and column where it
is, and never quotes the file.

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

# Directory for deploy state tracking (default: ~/.local/state/selfie).
# selfie creates it on its first write, whether you name it here or take the default.
state_directory: ~/.local/state/selfie

# Command timeout in seconds (default: 60)
command_timeout: 300

# Stop an apply at its first failure (default: false)
stop_on_error: false

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
```

A leading `~` or `~/` is expanded to your home directory, and extra slashes after it are ignored, so
`~//dev-packages` is `~/dev-packages`. `~user` and environment variables are not expanded. Nothing
else is resolved: a symlinked path is used and shown as written, and a relative path is relative to
the directory selfie runs in, which `selfie config validate` reports as an error.

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

You may point this at the same directory as `package_directory`, directly or through a symlink.
selfie then reads that directory once, so no entry is listed twice and no name collides with itself.
Every spec in it is read as a package spec, which must declare at least one environment: a spec
written by `selfie dotfiles track`, which declares none, would be refused by `selfie apply` there,
so `selfie dotfiles track` refuses to write one. Keep such specs in a directory of their own.

If you **set** this and no directory is at the path, selfie says what is there instead and carries
on without your standalone dotfiles — they are simply absent from `apply`, `dotfiles drift`,
`dotfiles list`, `sync status` and `spec validate --all`, in the CLI and the MCP server alike.
Nothing at the path, a plain file, a symlink whose destination is gone, and a path running through a
non-directory are all this case: none of them can hold a standalone dotfile, so nothing is missing
from the run and it succeeds. Only the first of them is fixed by creating the directory, so only the
first is offered `mkdir -p`.

If you do **not** set it and nothing at all is at the default sibling, selfie says nothing: that is
the ordinary state of a setup with no standalone dotfiles. It stays quiet only for an empty path. A
plain file, a dangling symlink or a path running through a non-directory is reported at the default
just as it is at a path you named, because none of those can be there by your having left the
setting out.

Two states refuse instead, because in both of them selfie cannot say what the directory holds. A
directory that **exists and cannot be listed** may have standalone dotfiles in it. A path selfie
**cannot classify at all** — a symlink loop is the ordinary way to get one — may be anything.
`dotfiles list` fails. `apply` with no package name, and `dotfiles drift`, carry on with the package
dotfiles and count one refusal, so they exit non-zero. See
[A refusal is not a success](../README.md#a-refusal-is-not-a-success).

Whether the path was configured decides only whether an **absence** is worth mentioning. It never
decides a refusal: a directory selfie cannot read is refused whether or not you named it.

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

A directory you name in the file must be an absolute path, as `package_directory` must;
`--state-directory` takes a relative path from the current directory. It does not have to exist:
selfie creates it on the first write that needs it, whether you name the path here or leave the
setting out and take the default. `selfie config validate` reports the directory in effect either
way.

What selfie will not do is put its state where something else already is. A path occupied by a file,
or by a symlink whose destination is gone, is refused before any dotfile is deployed, naming what is
there — creating the directory is the remedy for nothing being there and no remedy at all for a file
in the way.

A directory selfie cannot open is refused too, because a deploy state it cannot see is one it must
not overwrite. That refusal names the **state file**, not the directory: selfie stats the path,
finds a directory, and the read that follows fails with a permission error.

So is a deploy state selfie cannot write, as soon as a run would write anything. `selfie apply`
writes the state back just before its first write of any kind: a secret-bearing file, a backup, a
deployed file or a record. `selfie dotfiles track` and `selfie track` do the same just before they
copy the file. If that write fails, the run stops with nothing written, so it never deploys a file
it then cannot record. A run that writes nothing never writes the state and is not stopped by it,
except to tidy records of files that are gone, which only warns when it fails. A write can still
fail later, on a full disk for example; apply then stops after the one file it could not record and
names it. `apply --dry-run` writes nothing, so it checks the directory's permissions instead and
warns when a real run would stop.

If you name a directory that is not there, selfie creates it and says so. The two ways to reach that
are a first run and a typo in the setting, and they look identical in the output otherwise: a
mistyped `state_directory` reports every dotfile you have deployed as untracked, and
`selfie apply -y` would then overwrite an edited target instead of reporting a conflict. The warning
names the path and the setting. Leaving the setting out and taking the default is silent, since a
first run is the default's ordinary state and nothing was typed to get it wrong.

The state file (`deploy-state.yml`) is per-machine — it tracks what was deployed on _this_ machine
and is not meant to be shared or version-controlled.

If the file exists but selfie cannot use it — it cannot be read, it is empty, or it does not parse —
selfie names the file and says which. `selfie apply` and every track command — `selfie track`,
`selfie dotfiles track` and `selfie package track-dotfile` — then **refuse to run** until you repair
the file or move it aside, because each of them would end by writing the state back and selfie never
writes over a state file it could not read. A track only reads the state when it is going to write
one: a call that answers earlier — the target is already tracked, or the target itself is refused —
succeeds or fails on its own terms without looking at the file. `selfie dotfiles drift` and
`selfie apply --dry-run` write nothing, so they warn and carry on as though nothing had been
deployed: every tracked dotfile shows as untracked for that run. An **absent** state file is the
ordinary first-run case and is not reported.

Entries are keyed by the target path, so one source deployed to two targets is two records. A state
file written by an earlier selfie, whose entries were keyed by source, does not parse and is refused
the same way. Move it aside and run `selfie apply` once: every target whose content already matches
its source is recorded again without a prompt, and only a target that genuinely differs asks.

It is written readable only by its owner (mode `0600`). Its contents are not credentials, but they
name each repository-file dotfile selfie manages here, which is a useful map to anyone else with an
account on the machine.

A track that gets as far as writing the state and cannot is a separate case, reported separately:
the file it copied and the spec entry are both in place and only the record is missing, so selfie
names both and tells you to run `selfie apply` rather than the track command again. See
[Tracking New Files](package-files.md#tracking-new-files).

Provider-sourced and templated dotfiles are not recorded at all, so this is not a complete list of
what selfie manages — see
[No deploy state, and what follows from it](package-files.md#no-deploy-state-and-what-follows-from-it).

##### What else lives there

Alongside `deploy-state.yml`, selfie keeps a `backups/` directory holding one copy per target of the
content [an overwrite displaced](package-files.md#what-an-overwrite-keeps). Four things are worth
knowing before you point `state_directory` somewhere:

- **The copies are mode `0600`; the directories holding them are not.** Only files go through the
  owner-only writer, and a created directory gets the usual `0o777 & !umask`. Each directory is
  named after the target's own file name plus a checksum of its full path, so the listing tells
  anyone with an account on the machine which files selfie manages here — the same disclosure
  `deploy-state.yml` makes, one level up.
- **A copy is a verbatim copy of whatever was at the target.** Selfie knows where an entry writes,
  not what you had there first, so pointing an ordinary `source` entry at a path that currently
  holds a credential leaves that credential in plaintext under this directory. Provider-sourced and
  templated _entries_ are never copied; that is a statement about the entry, not about what its
  target happened to contain.
- **Do not put `state_directory` inside `dotfiles_directory`.** `selfie sync push` would commit
  every copy to your dotfiles repository.
- **Two applies running at once are not supported**, and this is one of the places it shows: each
  run removes copies of a target it did not just write, including the other run's.

Deleting a copy by hand is safe. Nothing reads them, no command manages them, and there is no
retention setting — selfie keeps the most recent per target and nothing else prunes them. Copies are
keyed by target path, so retargeting an entry orphans the old target's copy for good: over time the
directory holds one copy per target _ever_ deployed here.

### Global Behavior

#### `cli.verbose`

Do everything `--verbose` does, on every run. Lives under `cli:`, not at the top level.

```yaml
cli:
  verbose: true
```

stdout carries a command's answer at any verbosity. What verbose adds all goes to stderr: each
command's operation header, such as `Spec info package 'bat'`, every step, a configured command's
own output, and `DEBUG` log lines. At default verbosity a step that waits on something outside
selfie (a configured command, a provider command, or the network) still shows: as a spinner on a
terminal, which shows the command's latest output line and ends as `✓` with its elapsed time when it
succeeds or `✗` when it fails, and otherwise as one status line. Steps that run at once, such as the
audits of `package audit --all`, each get their own spinner. When a configured command fails, its
last output lines are shown at every verbosity.

#### `cli.use_colors`

Control colored output. Lives under `cli:`, not at the top level.

```yaml
cli:
  use_colors: false
```

#### `command_timeout`

Default timeout for package operations in seconds. Also bounds every dotfile provider command and
template binding, applied per command rather than per entry — an entry with several bindings can
take longer than this in total to resolve.

```yaml
command_timeout: 600 # 10 minutes
```

#### `stop_on_error`

Whether `selfie apply` stops at its first failure. Off by default, so one run reports every failure
and the next run is not needed to find the second one.

A failure is anything the run counts as refused: an entry selfie refused or could not carry out (a
source it could not read, a target it will not write to or could not classify, a write that failed,
a provider command that failed), a package refused whole, a package file it could not load, a name
several package files claim, or a dotfiles directory it could not read. A conflict is not a failure
and never stops a run, and neither is a warning. A run that carries on counts every failure in its
summary and exits `1`. A run that stops reports the refused entry's warning and the sentence naming
what stopped it, with no summary counts, and exits `1`. Packages refused whole, package files that
could not be loaded, names several files claim and an unreadable dotfiles directory are all known
before anything is deployed, so any of them stops the run before it deploys anything.

With this set, a failed provider command stops the run there, so no later entry is reached. With it
off, once a provider command fails, later entries whose command, or any of whose template bindings,
runs the same program are refused without running, with "an earlier `op` command failed; no command
was run" naming the program. A locked vault or a dismissed prompt fails every one of them the same
way. Entries that run another program, and repository files, still deploy. A dry run runs no
command, so this never applies to one.

```yaml
stop_on_error: true
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

Flags also fill _gaps_ in a file that exists. A file holding only a `cli:` section, or only one of
the two required settings, works when the flags supply the rest. When neither the file nor a flag
supplies a required setting, selfie names each one, with the key and the flag that would set it.

An empty value counts as not given, whether it is in the file or on the command line.
`--environment ''` leaves the file's `environment` in force, and `environment: ""` in the file with
no flag is reported as missing. The path flags cannot be given an empty value at all.

A config file that exists but cannot be read, cannot be parsed, or is not a regular file is still an
error — the flags do not paper over a file you are in the middle of editing.

**The `cli:` booleans only move one way.** `--verbose` turns verbose on and `--no-color` turns
colors off; neither has an opposite. `verbose: true` or `use_colors: false` under `cli:` therefore
cannot be overridden from the command line — edit the file. Written at the top level instead of
under `cli:` they are ignored entirely, and selfie reports them.

**`SELFIE_CONFIG_DIR` and the path flags do not compete.** The variable chooses _which file_ is
read; the flags override _fields_ in whatever file that was. Setting both is normal, and the flag
still wins for the field it names.

**A path flag is processed the way the same value in the file is, and a relative one is made
absolute.** `~` is expanded in `--package-directory`, `--dotfiles-directory` and
`--state-directory`, including in the `--package-directory=~/packages` form that no shell expands. A
relative path is taken from the current directory when selfie starts, as any command's path argument
is. `~user` is not expanded, so `--state-directory=~user/state` is refused as not absolute and
creates nothing.

An absolute state directory that does not exist is **not** refused: selfie creates the directory on
the first write that needs it, by flag exactly as by config file. What it will not do is put its
state where something else already is, so a path occupied by a file, or one selfie cannot read, is
refused before anything is deployed.

**`selfie config validate` reports the file, not the effective settings.** It deliberately reloads
what is on disk and applies no overrides, including to `verbose` and `use_colors`, so that a flag
cannot hide a problem in the file it is masking. A required setting the file leaves out is reported
as an error even when a flag supplies it on this run. It still fails when there is no config file at
all, even on a run that would otherwise succeed from flags — there is no file for it to report on.
Passing `-p` and reading back the file's `package_directory` is expected — it is not the flag being
ignored. Use `selfie package list`, which prints the package directory it actually read, to see the
effective value.

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

`package_directory` and `dotfiles_directory` are listed the way the commands that read them list
them, and reported as those commands treat what they find:

- `package_directory` is an **error** whenever it cannot be listed, since every command that reads
  it fails: nothing there, a file, a symlink to nothing, a directory whose entries cannot be read,
  or a path selfie could not check. A missing directory is offered a correction of the setting if
  the path is a typo, or a `mkdir -p` command.
- `dotfiles_directory` is a **warning** when no directory is there, since the reading commands carry
  on without standalone dotfiles, and an **error** when it is a directory selfie cannot list or a
  path it could not check, such as a symlink loop, since they refuse to go on. A missing directory
  is also refused by `dotfiles track`, which validate still reports as a warning.

`state_directory` gets the verdict a run reaches over the same path, in the run's own words:
validate loads the deploy state as a run does. A missing directory is an informational note, not a
warning, that it is not there yet, since selfie creates it on first use, and that the path may be a
typo; it does not stop the file validating. Anything else in its way, a path selfie could not check,
a state file it cannot read (such as one inside a mode `000` directory), and a state file that is
empty or does not parse are errors, because every command that records a deploy refuses to run over
them.

When `dotfiles_directory` or `state_directory` is not set, the default the commands use is checked
the same way. Nothing at an unset default is not reported, since that is the ordinary state of a
setup with no standalone dotfiles, or of a first run.

## Troubleshooting

### Ignored Configuration Keys

```
⚠ `configs_directory` was renamed to `dotfiles_directory` and is no longer read. Selfie ignored it.
```

Selfie loads a configuration file that contains keys it does not recognize, and reports each one
before the command runs. This is a **warning, not an error** — the rest of the file is still used
and the command still runs. A key that was renamed says what replaced it; anything else is reported
as unrecognized.

`selfie config validate` lists the same keys and does not call such a file valid: it says the
configuration is usable, with warnings, and exits `3`.

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
