---
paths:
  - "crates/**"
  - "docs/**"
---

# `selfie` concepts

We're incrementally implementing this functionality. selfie is a personal meta-package manager: it
doesn't install packages directly, it runs whatever commands the user configures per package. It's a
glorified command runner, scoped to user-defined environments.

## Packages

Package files are YAML, represented by `selfie::package::Package`. Each package file defines
per-environment install and check commands. Packages may also declare `dotfiles` (dotfile mappings
deployed via `selfie apply`), `post_install_note` (first-install guidance), and per-environment
`recommends` (soft dependencies that warn on failure instead of failing the parent). Example:
`bash-language-server` might use Homebrew on macOS and `npm` on Ubuntu -- the user decides per
environment, then just runs `selfie package install bash-language-server` regardless of which
machine they're on.

Package operations:

- **Validate**: Check that a package file follows the spec.
- **Check**: Run the user-defined check command to see if a package is installed.
- **Audit**: Run the user-defined audit command to detect installation sources and conflicts.
- **List**: List all YAML files in the configured package directory.
- **Create / Edit / Info / Update / Remove**: CRUD for package files in `package_directory`.
- **Apply**: Deploy dotfiles defined in a package's `dotfiles` field to their target locations.

A dotfile's content comes from one of three sources: a repository file (`source`), a repository file
rendered by substituting named values (`source` plus `vars`), or the whole stdout of a command
(`command`). The latter two are secret-bearing — see `secrets.md`.

## Environments

An environment is an arbitrary user-chosen label (typically per OS/distro). Package files have
`environment` sections tying install/check commands to these labels. The user sets their current
environment in config so selfie knows which commands to run.

## Configuration

Config file: `~/.config/selfie/config.yml`. Also settable via CLI flags.

Core settings (top-level, read by `SelfieConfig`):

- `environment`: The current environment label.
- `package_directory`: Directory containing selfie package files.
- `dotfiles_directory`: Directory containing dotfile source files for `selfie apply`.
- `state_directory`: Directory for deploy state tracking (checksums, drift detection).
- `command_timeout`, `stop_on_error`, `max_concurrency`: Execution settings.

CLI settings (under `cli:` section, read by `CliConfig`):

- `verbose`: Enable debug logging.
- `use_colors`: Enable colored terminal output.
