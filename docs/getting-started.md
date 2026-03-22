# Getting Started with Selfie

This guide will walk you through installing selfie, setting up your first configuration, and
creating your first packages.

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

You should see the main help output with available commands.

## Initial Setup

### 1. Set Up Shell Completion (Optional)

Enable tab completion for selfie commands by generating completion scripts for your shell. **Note:**
Completion paths and setup vary by system and shell configuration - these are common examples that
you may need to adapt for your environment.

#### Bash

The location for bash completions varies by system. Here are some common examples:

```bash
# User-local (most Linux distributions)
selfie completion bash > ~/.local/share/bash-completion/completions/selfie

# System-wide (most Linux distributions)
sudo selfie completion bash > /usr/local/share/bash-completion/completions/selfie

# macOS (user-local)
selfie completion bash > ~/.bash_completion.d/selfie

# Some systems may use different paths like:
# ~/.bash_completion.d/selfie
# /etc/bash_completion.d/selfie
```

Check your system's bash completion setup or consult your distribution's documentation for the
correct path.

#### Zsh

Zsh completion setup depends on your configuration. Here's a common approach:

```bash
# Create the completion directory if it doesn't exist
mkdir -p ~/.zfunc

# Generate the completion script
selfie completion zsh > ~/.zfunc/_selfie

# Add to your ~/.zshrc if not already present
echo 'fpath=(~/.zfunc $fpath)' >> ~/.zshrc
echo 'autoload -U compinit && compinit' >> ~/.zshrc
```

If you use a framework like Oh My Zsh or have a custom setup, you may need to place the completion
file in a different location or modify your configuration accordingly.

#### Fish

Fish typically loads completions from a standard location, but this may vary:

```fish
# Standard location (most systems)
selfie completion fish > ~/.config/fish/completions/selfie.fish

# Some systems may use:
# /usr/local/share/fish/completions/selfie.fish
```

Fish will automatically load the completions on next shell start.

### 2. Create Configuration Directory

Selfie stores its configuration and package files in `~/.config/selfie/`:

```bash
mkdir -p ~/.config/selfie/packages
```

### 3. Set Your Environment

Create your configuration file at `~/.config/selfie/config.yml`:

```yaml
environment: macos # or linux, ubuntu, arch, etc.
package_directory: ~/.config/selfie/packages
```

Choose an environment name that makes sense for your current system and context. This can be:

- `macos`, `macos-work`, `macos-home` for macOS systems with different contexts
- `ubuntu`, `debian`, `arch`, `fedora` for specific Linux distributions
- `linux-dev`, `linux-ci` for Linux with context
- `github-actions`, `ci` for CI/CD environments
- Any custom name that describes your environment and context

### 4. Validate Your Configuration

```bash
selfie config validate
```

This will check your configuration file and create default settings if needed.

## Your First Package

Let's create a package for `ripgrep`, a popular text search tool.

### 1. Create the Package File

```bash
selfie spec create ripgrep --interactive
```

This will prompt you for package details and create a template file. Alternatively, create it
manually:

```bash
selfie spec create ripgrep
```

### 2. Edit the Package Definition

```bash
selfie spec edit ripgrep
```

This opens the package file in your default editor. Update it with your preferred installation
method:

```yaml
name: ripgrep
description: Fast text search tool that respects gitignore
homepage: https://github.com/BurntSushi/ripgrep

environments:
  macos:
    install: brew install ripgrep
    check: which rg

  ubuntu:
    install: sudo apt update && sudo apt install -y ripgrep
    check: which rg

  arch:
    install: sudo pacman -S ripgrep
    check: which rg

  ci:
    install: |
      curl -LO https://github.com/BurntSushi/ripgrep/releases/download/13.0.0/ripgrep_13.0.0_amd64.deb
      sudo dpkg -i ripgrep_13.0.0_amd64.deb
    check: which rg
```

### 3. Validate the Package

```bash
selfie spec validate ripgrep
```

This checks your package definition for syntax errors and validates the structure.

### 4. Install the Package

```bash
selfie package install ripgrep
```

Selfie will:

1. Check if the package is already installed
2. Install any dependencies if needed
3. Run the installation command for your current environment

## Essential Commands

### Package Management

```bash
# List all available packages
selfie spec list

# Get detailed information about a package
selfie spec info ripgrep

# Check if a package is installed
selfie package check ripgrep

# Create a new package
selfie spec create my-tool

# Edit an existing package
selfie spec edit my-tool

# Remove a package definition
selfie spec remove my-tool
```

### Configuration

```bash
# Validate your configuration
selfie config validate

# Use a different environment temporarily
selfie --environment=linux package install ripgrep

# Use a different package directory
selfie --package-directory=/path/to/packages package list
```

### Global Options

```bash
# Verbose output for debugging
selfie --verbose package install ripgrep

# Disable colored output
selfie --no-color package list

# Override environment setting
selfie --environment=ci package install ripgrep
```

## Package Repository Setup

For better organization and backup, consider setting up a git repository for your packages:

### 1. Create a Repository

```bash
mkdir ~/my-selfie-packages
cd ~/my-selfie-packages
git init
```

### 2. Update Your Configuration

Edit `~/.config/selfie/config.yml`:

```yaml
environment: macos
package_directory: ~/my-selfie-packages
```

### 3. Add Some Packages

Create a few essential packages for your workflow:

```bash
cd ~/my-selfie-packages
selfie spec create node
selfie spec create docker
selfie spec create kubectl
```

### 4. Version Control

```bash
git add .
git commit -m "Initial package definitions"
git remote add origin https://github.com/your-username/my-selfie-packages.git
git push -u origin main
```

Now your packages are versioned and can be used across multiple machines or backed up safely.

## Next Steps

- Check out [use case examples](use-cases/) for real-world scenarios
- Read the [package file reference](package-files.md) for advanced features
- Explore [configuration options](configuration.md) for customization
- Browse [example packages](examples/) for inspiration

## Troubleshooting

### Command Not Found

If `selfie` isn't found after installation:

1. Ensure `~/.cargo/bin` is in your PATH
2. Try restarting your shell
3. Rebuild from source: `cargo install --path crates/cli`

### Permission Errors

If you get permission errors during package installation:

1. Check that your installation commands are correct for your system
2. Some packages may require `sudo` - include it in your install commands
3. Verify your user has appropriate permissions

### Package Validation Fails

If package validation fails:

1. Check YAML syntax with `selfie spec validate package-name`
2. Ensure all required fields are present
3. Verify environment names match your configuration
4. Check that commands are appropriate for your system

### Need Help?

- Run any command with `--help` for detailed usage information
- Use `--verbose` flag for detailed debugging output
- Check the [configuration guide](configuration.md) for setup issues
