<div align="center">
  <img src="assets/branding/selfie-logo-horizontal.svg" alt="selfie" width="300">

**A personal package manager that remembers how you like to install things.**

</div>

If you're a polyglot developer tired of remembering whether you installed `ripgrep` via homebrew,
`jq` via apt, or `prettier` via npm, selfie is for you. The challenge gets trickier when the same
tool is available via multiple package managers you're already using, and installing it one way
conflicts with your preferred setup. Define your installation preferences once, then let selfie
handle the details.

## Quick Navigation

### 🚀 Getting Started

- [**Installation**](#installation) - Get selfie up and running
- [**Quick Start**](#quick-start) - Your first package in minutes
- [**Documentation**](#documentation) - Complete guides and references

### 📖 Core Concepts

- [**Package Files Reference**](docs/package-files.md) - Complete package definition format
- [**Example Packages**](docs/examples/) - Ready-to-use package definitions
- [**Configuration Guide**](docs/configuration.md) - Environment setup and options
- [**Git Sync Guide**](docs/sync.md) - Syncing specs across machines

### 🎯 Real-World Usage

- [**Polyglot Developer**](docs/use-cases/polyglot-developer.md) - Individual developer workflow

## The Problem

As developers, we use tools from everywhere:

- `brew install ripgrep` on macOS, but `sudo pacman -S ripgrep` on Arch
- `npm install -g prettier` for Node tools, but `pip install black` for Python formatters
- `cargo install bat` for Rust tools, but `apt install fd-find` for system utilities
- **Package manager conflicts**: `yaml-language-server` is available via homebrew, but that would
  install Node.js via homebrew too, conflicting with your `fnm`-managed Node.js versions
- **Version managers**: You use `fnm` for Node.js, `uv` for Python, `rustup` for Rust, but some
  tools want to install language runtimes via the OS package manager
- Different commands for checking if things are installed
- Different approaches across team members and environments

## The Solution

Define your packages once, install them everywhere:

```yaml
# ~/.selfie/packages/ripgrep.yaml
name: ripgrep
description: Fast text search tool
homepage: https://github.com/BurntSushi/ripgrep

dotfiles:
  - source: ripgrep/ripgreprc
    target: ~/.config/ripgrep/config

environments:
  macos:
    install: brew install ripgrep
    check: which rg
    audit: |
      brew list ripgrep 2>/dev/null && echo "homebrew"
    dependencies: [homebrew]
    recommends: [bat] # nice companion tool

  arch-linux:
    install: sudo pacman -S ripgrep
    check: which rg

  ubuntu:
    install: sudo apt install ripgrep
    check: which rg
```

The optional `audit` field lets you detect _how_ a package is installed — useful for finding
conflicts when the same tool is available via multiple package managers. Run
`selfie package audit ripgrep` to check, or `selfie package audit --all` to scan everything.

Then simply:

```bash
selfie package install ripgrep
```

Selfie knows your current environment and runs the right commands. No more remembering, no more
inconsistency.

## Key Benefits

- **Memory**: Never forget how you prefer to install something on all environments you work in
- **Documentation**: Your package files serve as documentation of your choices
- **Dependency tracking**: Install dependencies (that you pick) automatically before main packages
- **Portability**: Package definitions work across your different machines
- **Flexibility**: Any shell command can be a package
- **Environment-aware**: Different installation methods for macOS, Linux, CI, work, home, etc.

## How Selfie Is Different

### Why not use existing package managers?

As a developer, you can't always get everything you need from one package manager:

- **OS package managers** (apt/yum/pacman/homebrew): Great for system tools, but often have outdated
  versions of development tools, and you lose control over language runtime versions
- **Language package managers** (npm/pip/gem/cargo): Essential for language-specific tools, but
  limited to their ecosystems and don't handle system dependencies
- **Specialized tools** like Mason (Neovim): Excellent for editor tooling, but tied to specific
  applications, limited package registry, and don't work outside their context out of the box
- **Universal solutions** (Nix/Guix): Powerful but complex, steep learning curve, and can conflict
  with existing workflows

### What makes selfie different?

Selfie is a **meta-package manager** that orchestrates your existing package managers based on your
preferences and environment. Unlike traditional package managers:

- **Personal**: You control installation methods and preferences
- **Simple**: Package definitions can be as simple as a name, version, environment, and install
  command
- **Multi-platform**: Same package definition works anywhere you can run a shell script: macOS,
  Linux, CI, k8s, VMs, etc.
- **Multi-manager**: Use homebrew, apt, npm, cargo, etc. in the same workflow
- **Flexible**: Works with any installation method, not just package repositories

The reality is you probably need multiple package managers, but remembering which tool comes from
where, and avoiding conflicts between them, is the real challenge. Selfie solves the "which package
manager?" problem without forcing you into a single ecosystem.

## Installation

### Supported platforms

selfie runs on macOS and Linux. Windows is not supported: nothing in this project builds or tests
it, parts of the test suite do not compile there, and dotfile deployment assumes Unix file
permissions throughout. A Windows build is not expected to work, and a green one would not mean
selfie had been verified on it.

### From Source

```bash
git clone https://github.com/turboladen/selfie.git
cd selfie
cargo install --path crates/cli
```

### MCP Server (for AI assistants)

```bash
cargo install --path crates/mcp-server
```

See the [MCP server README](crates/mcp-server/README.md) for setup with Claude Desktop, Claude Code,
Cursor, etc.

### Verify Installation

```bash
selfie --help
```

## Quick Start

1. **Install the selfie CLI** (see [Installation](#installation) above)

2. **Create your config file:**
   ```bash
   # Create the config directory
   mkdir -p ~/.config/selfie

   # Create your config file with your preferred settings
   cat > ~/.config/selfie/config.yaml << EOF
   # Your current environment (use whatever makes sense for you)
   environment: "macos"  # or "linux", "ubuntu", "work", "home", etc.

   # Where to store your package definition files
   package_directory: "~/.selfie/packages"
   EOF

   # Create the package directory, then verify your config is valid
   mkdir -p ~/.selfie/packages
   selfie config validate
   ```

3. **Create your first package:**
   ```bash
   selfie spec create ripgrep --interactive
   ```

4. **Install it:**
   ```bash
   selfie package install ripgrep
   ```

5. **Deploy dotfiles** (if your package has a `dotfiles` section):
   ```bash
   selfie apply ripgrep
   ```

## Exit codes

Every `selfie` command exits with one of these codes. Scripts and CI steps should branch on them
rather than on output text.

| Code  | Meaning                                                                                         |
| ----- | ----------------------------------------------------------------------------------------------- |
| `0`   | Clean: the command did what it was asked and found nothing to report.                           |
| `1`   | Failed: an error, **a refusal**, a declined `spec create`, or a run that ends without a result. |
| `2`   | Usage: the command line could not be parsed.                                                    |
| `3`   | Found: the command did what it was asked, and **found what it was asked to look for**.          |
| `130` | Cancelled (Ctrl+C). This is the usual `128 + SIGINT` value.                                     |

A failure outranks a finding: a command that refused part of its work exits `1` even if it also
found something, since its answer has a hole in it. A code is never renumbered or reused. A new one
takes the next free value from `3` to `63`; `64` to `78` (the `sysexits.h` codes) and `126` and up
(reserved by shells) are never used.

### What each command reports

| Command               | Clean (`0`)                                                             | Found (`3`)                                                               | Failed (`1`)                                                             |
| --------------------- | ----------------------------------------------------------------------- | ------------------------------------------------------------------------- | ------------------------------------------------------------------------ |
| `apply`               | deployed, up to date, or conflicts skipped                              | never                                                                     | a refused entry; an error; a write that could not be recorded            |
| `dotfiles drift`      | in sync; nothing deploys on this machine                                | drift; an orphaned target; the orphan check could not finish and said why | a refused entry; an error                                                |
| `package check`       | the check command exited 0                                              | the check command exited non-zero, for any reason (not installed)         | no check command; selfie's own timeout; the command could not be started |
| `package audit`       | no conflict                                                             | a conflict; not installed                                                 | the audit command exited non-zero; no audit command                      |
| `package audit --all` | every audited package clean; a package with no audit command is skipped | a conflict; not installed                                                 | an audit command exited non-zero; a spec left out                        |
| `spec validate`       | no warnings                                                             | warnings                                                                  | errors; the spec does not parse                                          |
| `config validate`     | no warnings                                                             | warnings, including unrecognized keys                                     | errors; the file cannot be loaded                                        |
| `spec create`         | created                                                                 | never                                                                     | declined, because the name already exists; an error                      |

A configured command's exit status is all selfie knows about it. Your shell reports a command killed
by a signal as an ordinary non-zero status, so a check that was killed reads as "not installed". An
audit reports "not installed" when its command exits 0 and prints no source; an audit command that
exits non-zero is a failure. An informational note, such as the one saying `apply` runs a spec's
commands, never makes a run exit `3`.

`selfie apply <name>` matches the name against package file names, ignoring case, the same way
`selfie package install` does. A name that matches no package, names a package file that could not
be loaded, or is claimed by several package files (such as `bat.yml` and `bat.yaml`), is a failure
and exits `1` with nothing deployed. Without a name, such a set of files is refused, counted, and
the rest of the run carries on, when one of them failed to parse or declares dotfiles for the
current environment; a set that would deploy nothing here is left to `selfie package install` to
refuse. A named package that declares no dotfiles for the current environment says so and exits `0`:
there is nothing to apply on this machine.

### A refusal is not a success

`selfie apply` exits `1` when it declines to deploy an entry, even though the rest of the run
succeeded and the command reports itself as completed. Selfie refuses an entry when it cannot deploy
it safely or unambiguously — an unrecognized key in the entry, a target it will not write to (a
symlink, whether or not its content already matches, or a path outside your home directory), a
target it cannot read, or a source file it cannot read. Each refusal is named in the output, and the
summary line counts them:

```
Dotfiles applied in environment 'macos': 2 deployed, 1 skipped, 0 conflict(s), 1 refused
```

Whole packages are refused too, when the problem is in the file rather than in one entry: any
unrecognized key at the top level or in the environment being used, a package file selfie could not
re-read to check for one, or a package-directory spec that declares no environment. See
[Package Files](docs/package-files.md#the-same-rule-applies-to-a-packages-top-level-keys).

`selfie package install`, `check` and `audit` refuse the same files, and exit non-zero rather than
running a command the file's author did not write — a key hiding `environments:` costs them the very
mapping they take that command from. Each was given one package name, so a refusal leaves them
nothing to do.

`selfie dotfiles drift` follows the same rule for an entry it did not compare: an entry apply would
refuse, a source that escapes the package directory or cannot be read, and a target that is a
symlink, a fifo, socket or device node, a directory, or that it cannot read are refusals, and the
check exits `1`. A secret-bearing entry is not a refusal: drift reports it as not verifiable, since
checking it would run its commands, and counts it apart. That includes one whose target is a
symlink. A dotfiles directory that exists but cannot be listed counts as one refusal for
`selfie apply` with no package name, and for `selfie dotfiles drift`. Every standalone dotfile in it
was part of the run and none could be read, while the package dotfiles still deploy or are still
checked. A package file that cannot be loaded counts the same way, one refusal each, since nothing
it declares was deployed or checked: every such file in the package directory, and one in the
dotfiles directory unless the package directory already has a spec of that name, which it only warns
about.

Two things are deliberately **not** refusals, and neither of them makes the exit code non-zero:

- **A skip.** The entry was already in sync, so there was nothing to do. A symlinked target is not a
  skip even when its content matches: selfie does not read through the link to find out, and refuses
  it.
- **A conflict.** The target exists, is untracked, and differs from the repository file. Selfie
  leaves it alone and reports it, because overwriting it is your decision — see
  [Dotfiles](docs/package-files.md#dotfiles). When you do decide to overwrite, selfie copies the
  content it displaced under `state_directory` first and names the copy, so the decision is
  reversible — see [What an overwrite keeps](docs/package-files.md#what-an-overwrite-keeps).

`selfie apply --dry-run` follows the same rule: a refusal it can predict without writing anything is
still reported, and still exits `1`. A preview whose job is to tell you what `apply` would do must
not report success for a run that would refuse.

The MCP server applies the same contract: a refusal comes back as an error result with
`"status": "refused"`, so an assistant is not told the deploy worked. A result that could not answer
without refusing anything, such as an audit whose command failed, is an error result with
`"status": "failed"`. A finding is a successful call with `"status": "found"`, and a cancelled
operation an error result with `"status": "cancelled"`. Every result also carries `outcome`:
`"clean"`, `"found"` or `"failed"`, or `"cancelled"` for a cancelled operation.

## Documentation

### Complete Documentation

- [**Getting Started Guide**](docs/getting-started.md) - Detailed setup and first steps
- [**Configuration Guide**](docs/configuration.md) - Environment setup and options
- [**Package Files Reference**](docs/package-files.md) - Complete package definition format
- [**Example Packages**](docs/examples/) - Ready-to-use package definitions

### Use Cases

- [**Polyglot Developer**](docs/use-cases/polyglot-developer.md) - Managing tools across homebrew,
  npm, pip, cargo, etc.

### Documentation Structure

```
docs/
├── getting-started.md           # Installation and first steps
├── configuration.md             # Setup and configuration options
├── package-files.md             # Package definition reference
├── use-cases/                   # Real-world scenarios
│   └── polyglot-developer.md    # Individual developer workflow
└── examples/                    # Example package definitions
    ├── README.md                # Guide to examples
    ├── ripgrep.yaml             # Multi-platform text search tool
    ├── node.yaml                # Node.js with version management
    ├── docker.yaml              # Container platform setup
    └── ...                      # More tool examples
```

## Help and Support

### CLI Help

Every command has built-in help:

```bash
selfie --help                    # Main help
selfie spec --help               # Spec (definition) commands
selfie package --help            # Package (runtime) commands
selfie apply --help              # Dotfile deployment commands
selfie dotfiles --help           # Dotfile inspection and tracking
selfie track --help              # Interactive file tracking shortcut
selfie sync --help               # Git sync operations
selfie spec create --help        # Specific command help
```

### Debugging

Use verbose mode for detailed output:

```bash
selfie --verbose package install package-name
```

### Common Issues

- **Permission errors**: Check if install commands need `sudo`. Do not reach for `sudo selfie apply`
  when a dotfile target is unwritable — selfie refuses it, because the run has no per-entry
  privilege scope and would write every `~/` entry as that user too. See
  [Configuration](docs/configuration.md#running-under-sudo).
- **Command not found**: Verify PATH includes tool installation locations
- **Package validation fails**: Use `selfie spec validate package-name`
- **Configuration issues**: Run `selfie config validate`

### Community

- **Issues**: Report bugs and request features in [GitHub Issues](../../issues)
- **Discussions**: Share usage patterns and ask questions in [GitHub Discussions](../../discussions)
- **Contributing**: Open an issue before a large change so the approach can be agreed first

## Status

Selfie is actively developed and ready for daily use. Current features:

- ✅ Package installation with environment-specific commands
- ✅ Dependency resolution and installation
- ✅ Soft dependencies (`recommends`) with `--no-recommends` flag
- ✅ Dotfile deployment (`selfie apply`) with conflict and drift detection, keeping a copy of the
  content an overwrite displaces (not for provider-sourced files — see
  [What an overwrite keeps](docs/package-files.md#what-an-overwrite-keeps))
- ✅ Spec validation and package listing
- ✅ Interactive spec creation and editing
- ✅ Configuration management
- ✅ Audit: detect installation sources and flag conflicts
- ✅ Spec update: structured field modifications via MCP (`selfie_spec_update`); the CLI has no
  `spec update` command
- ✅ MCP server for AI assistant integration ([docs](crates/mcp-server/README.md))
- ✅ Auto-formatting: `dprint fmt` runs on saved package files
- ✅ Login shell execution for install/check/audit commands
- ✅ Dotfile tracking: `selfie dotfiles track`, `selfie package track-dotfile`, `selfie track`
- ✅ Dotfile drift detection: `selfie dotfiles drift`
- ✅ Orphaned targets: `apply` and `dotfiles drift` report a file selfie deployed that no entry
  deploys to any more, and never delete it
- ✅ Sudo refusal: `apply`, the track commands and `sync push`/`pull` decline to run under `sudo`,
  with `--allow-sudo` as the deliberate override
- ✅ Dotfile listing: `selfie dotfiles list`
- ✅ Provider-sourced and templated dotfiles: content from a command, or from a template with named
  values, resolved at deploy time and never stored
- ✅ Git sync: `selfie sync status/push/pull` with per-package conventional commits
- 📋 Package groups and bulk operations (planned)

## Contributing

Found a bug or want to contribute? Check out the [issues](../../issues) or submit a pull request.

## License

Licensed under the [MIT License](LICENSE).
