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

   # Verify your config is valid
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

A `selfie` command that runs exits with one of these three codes. Scripts and CI steps should branch
on them rather than on output text. A command line selfie cannot parse — an unknown flag, a missing
argument — is rejected by the argument parser before any of this applies, and exits `2`.

| Code  | Meaning                                                                       |
| ----- | ----------------------------------------------------------------------------- |
| `0`   | The command did everything it was asked to do.                                |
| `1`   | The command failed, **or refused part of its work**. See below.               |
| `130` | The command was interrupted (Ctrl+C). This is the usual `128 + SIGINT` value. |

### A refusal is not a success

`selfie apply` exits `1` when it declines to deploy an entry, even though the rest of the run
succeeded and the command reports itself as completed. Selfie refuses an entry when it cannot deploy
it safely or unambiguously — an unrecognized key in the entry, a target it will not write to (a
symlink, or a path outside your home directory), or a source file it cannot read. Each refusal is
named in the output, and the summary line counts them:

```
Dotfiles applied: 2 deployed, 1 skipped, 0 conflict(s), 1 refused (4/4 steps)
```

Whole packages are refused too, when the problem is in the file rather than in one entry: any
unrecognized key at the top level or in the environment being applied, or a package file with
nothing to deploy that selfie could not re-read to check for one. See
[Package Files](docs/package-files.md#the-same-rule-applies-to-a-packages-top-level-keys).

Two things are deliberately **not** refusals, and neither of them makes the exit code non-zero:

- **A skip.** The entry was already in sync, so there was nothing to do.
- **A conflict.** The target exists, is untracked, and differs from the repository file. Selfie
  leaves it alone and reports it, because overwriting it is your decision — see
  [Dotfiles](docs/package-files.md#dotfiles).

`selfie apply --dry-run` follows the same rule: a refusal it can predict without writing anything is
still reported, and still exits `1`. A preview whose job is to tell you what `apply` would do must
not report success for a run that would refuse.

The MCP server applies the same contract: a refusal comes back as an error result with
`"status": "refused"`, so an assistant is not told the deploy worked.

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
- **Contributing**: See [CONTRIBUTING.md](CONTRIBUTING.md) for contribution guidelines

## Status

Selfie is actively developed and ready for daily use. Current features:

- ✅ Package installation with environment-specific commands
- ✅ Dependency resolution and installation
- ✅ Soft dependencies (`recommends`) with `--no-recommends` flag
- ✅ Dotfile deployment (`selfie apply`) with conflict and drift detection
- ✅ Spec validation and package listing
- ✅ Interactive spec creation and editing
- ✅ Configuration management
- ✅ Audit: detect installation sources and flag conflicts
- ✅ Spec update: structured field modifications via CLI and MCP
- ✅ MCP server for AI assistant integration ([docs](crates/mcp-server/README.md))
- ✅ Auto-formatting: `dprint fmt` runs on saved package files
- ✅ Login shell execution for install/check/audit commands
- ✅ Dotfile tracking: `selfie dotfiles track`, `selfie package track-dotfile`, `selfie track`
- ✅ Dotfile drift detection: `selfie dotfiles drift`
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
