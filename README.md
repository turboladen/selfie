<div align="center">
  <img src="assets/branding/selfie-logo-horizontal.svg" alt="selfie" width="300">

**A personal package manager that remembers how you like to install things.**

</div>

If you're a polyglot developer tired of remembering whether you installed `ripgrep` via homebrew,
`jq` via apt, or `prettier` via npm, selfie is for you. The challenge gets trickier when the same
tool is available via multiple package managers you're already using, and installing it one way
conflicts with your preferred setup. Define your installation preferences once, then let selfie
handle the details.

## The Problem

As developers, we use tools from everywhere:

- `brew install ripgrep` on macOS, but `sudo pacman -S ripgrep` on Arch
- `npm install -g prettier` for Node tools, but `pip install black` for Python formatters
- `cargo install bat` for Rust tools, but `apt install fd-find` for system utilities
- **Package manager conflicts**: `yaml-language-server` is available via homebrew, but that would
  install Node.js via homebrew too, conflicting with your `fnm`-managed Node.js versions
- **Version managers**: You use `fnm` for Node.js, `pyenv` for Python, `rustup` for Rust, but some
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
    dependencies: [homebrew]

  arch-linux:
    install: sudo pacman -S ripgrep
    check: which rg

  ubuntu:
    install: sudo apt install ripgrep
    check: which rg
```

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

## Quick Start

1. **Install the selfie CLI:**
   ```bash
   git clone https://github.com/turboladen/selfie.git
   cd selfie
   cargo install --path crates/cli
   ```

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

## Real-World Usage

See how selfie works for polyglot developers:

- [Polyglot developer workflow](docs/use-cases/polyglot-developer.md) - Managing tools across
  homebrew, npm, pip, cargo, etc.

## Documentation

- [Getting Started Guide](docs/getting-started.md) - Detailed setup and first steps
- [Package File Reference](docs/package-files.md) - Complete package definition format
- [Configuration Guide](docs/configuration.md) - Environment setup and options
- [Example Packages](docs/examples/) - Ready-to-use package definitions

## How It's Different

As a developer, you can't get everything from one package manager, nor would you want to:

- **OS package managers** (apt/yum/pacman/homebrew): Great for system tools, but often have outdated
  versions of development tools, and you lose control over language runtime versions
- **Language package managers** (npm/pip/gem/cargo): Essential for language-specific tools, but
  limited to their ecosystems and don't handle system dependencies
- **Specialized tools** like Mason (Neovim): Excellent for editor tooling, but tied to specific
  applications, limited package registry, and don't work outside their context out of the box
- **Universal solutions** (Nix/Guix): Powerful but complex, steep learning curve, and can conflict
  with existing workflows

The reality is you need multiple package managers, but remembering which tool comes from where, and
avoiding conflicts between them, is the real challenge.

Selfie is a **meta-package manager** that orchestrates your existing package managers based on your
preferences and environment, solving the "which package manager?" problem without forcing you into a
single ecosystem.

## Installation

### From Source

```bash
git clone https://github.com/turboladen/selfie.git
cd selfie
cargo install --path crates/cli
```

### Verify Installation

```bash
selfie --help
```

## Status

Selfie is actively developed and ready for daily use. Current features:

- ✅ Package installation with environment-specific commands
- ✅ Dependency resolution and installation
- ✅ Package validation and listing
- ✅ Interactive package creation and editing
- ✅ Configuration management
- ⏳ Advanced dependency resolution (in progress)
- 📋 Package groups and bulk operations (planned)

## Contributing

Found a bug or want to contribute? Check out the [issues](../../issues) or submit a pull request.

## License

Licensed under the [MIT License](LICENSE).
