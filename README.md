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

environments:
  macos:
    install: brew install ripgrep
    check: which rg
    audit: |
      brew list ripgrep 2>/dev/null && echo "homebrew"
    dependencies: [homebrew]

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
   selfie package create ripgrep --interactive
   ```

4. **Install it:**
   ```bash
   selfie package install ripgrep
   ```

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
selfie package --help           # Package commands
selfie package install --help   # Specific command help
```

### Debugging

Use verbose mode for detailed output:

```bash
selfie --verbose package install package-name
```

### Common Issues

- **Permission errors**: Check if install commands need `sudo`
- **Command not found**: Verify PATH includes tool installation locations
- **Package validation fails**: Use `selfie package validate package-name`
- **Configuration issues**: Run `selfie config validate`

### Community

- **Issues**: Report bugs and request features in [GitHub Issues](../../issues)
- **Discussions**: Share usage patterns and ask questions in [GitHub Discussions](../../discussions)
- **Contributing**: See [CONTRIBUTING.md](CONTRIBUTING.md) for contribution guidelines

## Status

Selfie is actively developed and ready for daily use. Current features:

- ✅ Package installation with environment-specific commands
- ✅ Dependency resolution and installation
- ✅ Package validation and listing
- ✅ Interactive package creation and editing
- ✅ Configuration management
- ✅ Audit: detect installation sources and flag conflicts
- ✅ Package update: structured field modifications via CLI and MCP
- ✅ MCP server for AI assistant integration ([docs](crates/mcp-server/README.md))
- ✅ Auto-formatting: `dprint fmt` runs on saved package files
- ⏳ Advanced dependency resolution (in progress)
- 📋 Package groups and bulk operations (planned)

## Contributing

Found a bug or want to contribute? Check out the [issues](../../issues) or submit a pull request.

## License

Licensed under the [MIT License](LICENSE).
