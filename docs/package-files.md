# Package Files Reference

This document provides a comprehensive reference for creating and managing selfie package definition
files.

## Overview

Package files are YAML documents that define how to install and check packages across different
environments. They serve as the core configuration that tells selfie how you prefer to manage each
tool or library.

## File Structure

Package files follow this basic structure:

```yaml
name: package-name
version: 1.0.0
description: (optional) Brief description of what this package provides
homepage: (optional) https://example.com
post_install_note: (optional) Note displayed after first-time install

dotfiles:
  - source: package-name/config.toml
    target: ~/.config/package-name/config.toml

environments:
  environment-name:
    install: command to install the package
    check: (optional) command to verify installation
    dependencies:
      - dependency-package-1
      - dependency-package-2
    recommends:
      - optional-companion-tool

  another-environment:
    install: different installation command
    check: verification command
```

## Required Fields

### `name`

The package name, which must match the filename (without `.yaml` extension).

```yaml
name: ripgrep # for file ripgrep.yaml
```

### `version`

Semantic version of your package definition (not the tool version).

```yaml
version: 1.2.0
```

### `environments`

A map of environment names to their installation configurations. At least one environment must be
defined.

```yaml
environments:
  macos:
    install: brew install ripgrep
    check: which rg
```

## Optional Fields

### `description`

Brief description of what the package provides.

```yaml
description: Fast text search tool that respects gitignore
```

### `homepage`

URL to the package's homepage or repository.

```yaml
homepage: https://github.com/BurntSushi/ripgrep
```

### `dotfiles`

A list of dotfile mappings that define files to deploy from your dotfiles repository to their target
locations. Unlike environment-specific fields, `dotfiles` is a top-level field that applies across
all environments.

Each entry has two fields:

- `source` — Relative path within your dotfiles directory (see
  [Configuration Guide](configuration.md) for `dotfiles_directory`)
- `target` — Absolute destination path (supports `~` for home directory)

```yaml
dotfiles:
  - source: fnm/fish-conf.fish
    target: ~/.config/fish/conf.d/fnm.fish
  - source: fnm/zsh-conf.zsh
    target: ~/.config/zsh/conf.d/fnm.zsh
```

Dotfiles are deployed with `selfie apply`, not during `selfie package install`. This separation
keeps installation fast and gives you explicit control over when dotfiles are written to disk.

See [Dotfile Deployment](#dotfile-deployment) below for the full workflow.

### `post_install_note`

An optional message displayed to the user after a fresh install. Use this for important setup steps
that can't be automated, like adding shell integrations or restarting services.

```yaml
post_install_note: |
  Add this to your shell profile to activate fnm:
    eval "$(fnm env --use-on-cd)"
```

The note is only shown on first-time installs, not on subsequent runs where the check command
already passes.

## Environment Configuration

Each environment must have an `install` command and optionally a `check` command, `dependencies`,
and `recommends`.

### `install`

Command(s) to install the package. Can be a single command or multi-line script.

**Single command:**

```yaml
install: brew install ripgrep
```

**Multi-line script:**

```yaml
install: |
  curl -Lo ripgrep.tar.gz https://github.com/BurntSushi/ripgrep/releases/download/13.0.0/ripgrep-13.0.0-x86_64-unknown-linux-musl.tar.gz \
    && tar xzf ripgrep.tar.gz \
    && sudo cp ripgrep-13.0.0-x86_64-unknown-linux-musl/rg /usr/local/bin/
```

> **Working Directory**: All install and check commands automatically run in the package directory
> (where the `.yaml` file is located). This means you can use relative paths like
> `./scripts/install.sh` without needing to manually change directories.

**Multi-step with error handling (recommended):**

```yaml
install: |
  set -e
  echo "Downloading Node.js..."
  curl -fsSL https://deb.nodesource.com/setup_20.x | sudo -E bash -
  echo "Installing Node.js..."
  sudo apt-get install -y nodejs
  echo "Verifying installation..."
  node --version
```

> **Important:** For multi-line scripts, consider adding `set -e` at the beginning to ensure the
> script exits immediately if any command fails. This prevents subsequent commands from running
> after a failure, which is critical for package installation integrity. Selfie will warn you if it
> detects multi-line commands without error handling to help you write safer installation scripts.

### `check`

Optional command to verify the package is installed correctly. Should exit with code 0 if installed,
non-zero if not. If not given, executing the `check` command will always behave as if the package is
not yet installed.

**Simple binary check:**

```yaml
check: which rg
```

**Version verification:**

```yaml
check: node --version | grep -q "v20"
```

**Complex verification:**

```yaml
check: |
  which docker && \
    docker --version && \
    docker info > /dev/null 2>&1
```

**Service status check:**

```yaml
check: systemctl is-active --quiet docker
```

### `dependencies`

List of other selfie packages that must be installed before this package.

```yaml
dependencies:
  - node
  - python
  - homebrew
```

Dependencies are installed in the order listed, and selfie will recursively install their
dependencies first.

### `recommends`

Optional list of packages to install as soft dependencies. Unlike `dependencies`, a recommend
failure does **not** prevent the parent package from succeeding.

```yaml
recommends:
  - node # Useful companion, but fnm works without it
  - yarn # Nice to have alongside
```

**Key differences from `dependencies`:**

| Behavior         | `dependencies` | `recommends`      |
| ---------------- | -------------- | ----------------- |
| Install order    | Before parent  | After parent      |
| Failure handling | Parent fails   | Warning only      |
| Depth            | Recursive      | One level only    |
| Skip flag        | —              | `--no-recommends` |

Recommends are installed by default. To skip them:

```bash
selfie package install fnm --no-recommends
```

Recommends are only one level deep — a recommended package's own recommends are not followed. This
keeps installation predictable and avoids deep recursive recommend chains.

## Command Execution

### Working Directory

All install and check commands automatically run in the **package directory** (where the `.yaml`
file is located). This means you can use relative paths to reference scripts, configuration files,
or other resources without needing to manually change directories.

**Example with relative paths:**

```yaml
environments:
  macos:
    install: |
      set -e

      # These paths are relative to the package directory
      source ./scripts/common.sh
      ./scripts/macos_install.sh
    check: ./scripts/check.sh

  ubuntu:
    install: ./scripts/ubuntu_install.sh
    check: ./scripts/check.sh
```

**Directory structure:**

```
package-directory/
├── my-package.yaml        # Package definition
├── other-package.yaml     # Other packages
└── scripts/               # Scripts directory
    ├── common.sh          # Shared utilities
    ├── macos_install.sh   # macOS installation
    ├── ubuntu_install.sh  # Ubuntu installation
    └── check.sh           # Verification script
```

### Error Handling

For multi-line scripts, consider adding proper error handling to ensure scripts exit immediately if
any command fails:

```yaml
install: |
  set -e  # Exit on first error
  echo "Installing package..."
  command1
  command2
  echo "Installation complete"
```

## Advanced Features

### Environment Variables

You can use environment variables in your commands:

```yaml
install: |
  set -e

  export VERSION=${TERRAFORM_VERSION:-1.6.0}
  wget https://releases.hashicorp.com/terraform/${VERSION}/terraform_${VERSION}_linux_amd64.zip
  unzip terraform_${VERSION}_linux_amd64.zip
  sudo mv terraform /usr/local/bin/
```

### Conditional Logic

Use shell conditionals for platform-specific behavior within an environment:

```yaml
install: |
  if [[ "$OSTYPE" == "darwin"* ]]; then
    brew install postgresql
  else
    sudo apt-get install postgresql-client
  fi
```

### User-specific Installation

Install tools in user space rather than system-wide:

```yaml
install: |
  mkdir -p ~/.local/bin \
    && curl -Lo ~/.local/bin/jq https://github.com/stedolan/jq/releases/download/jq-1.6/jq-linux64 \
    && chmod +x ~/.local/bin/jq
check: ~/.local/bin/jq --version
```

### Post-installation Setup

Perform additional configuration after installation:

```yaml
install: |
  set -e
  brew install docker

  # Start Docker Desktop
  open /Applications/Docker.app

  # Wait for Docker to start
  while ! docker info > /dev/null 2>&1; do
    echo "Waiting for Docker to start..."
    sleep 5
  done

  echo "Docker is ready!"
```

## Dotfile Deployment

Packages can declare dotfiles that should be deployed to specific locations on your system. This is
useful for shell integrations, editor configs, tool settings, and anything that lives outside the
package directory.

### How It Works

1. **Define** dotfile mappings in your package YAML (the `dotfiles` field)
2. **Store** dotfile source files in your dotfiles directory (a sibling of your package directory by
   default — see [Configuration Guide](configuration.md) for `dotfiles_directory`)
3. **Deploy** with `selfie apply`

### Directory Structure

```
~/.selfie/
├── packages/              # Package definitions (package_directory)
│   ├── fnm.yaml
│   └── starship.yaml
└── dotfiles/              # Dotfile source files (dotfiles_directory)
    ├── fnm/
    │   ├── fish-conf.fish
    │   └── zsh-conf.zsh
    └── starship/
        └── starship.toml
```

### Example Package with Dotfiles

```yaml
name: starship
version: 1.0.0
description: Cross-shell prompt
homepage: https://starship.rs

post_install_note: |
  Restart your shell or source your profile to activate starship.

dotfiles:
  - source: starship/starship.toml
    target: ~/.config/starship.toml

environments:
  macos:
    install: brew install starship
    check: which starship
    recommends:
      - nerd-fonts
```

### Deploying Dotfiles

```bash
# Deploy all dotfiles from all packages
selfie apply

# Deploy dotfiles for a specific package only
selfie apply starship

# Preview what would change without writing files
selfie apply --dry-run

# Overwrite even if target was modified locally
selfie apply --yes
```

### Conflict Detection

Selfie tracks checksums of deployed files. If you modify a deployed file locally _and_ the source
file changes, selfie detects this as a conflict:

```
⚠ Conflict: ~/.config/starship.toml
  Source and target both changed since last deploy.
  Use --yes to overwrite, or resolve manually.
```

Without `--yes`, conflicts are reported but the target file is left untouched.

### Drift Detection

Check whether deployed dotfiles have been modified since they were last deployed:

```bash
selfie apply --dry-run
```

This shows which files are up to date, which have drifted, and which need deploying — without
writing anything.

### Validation Rules for Dotfiles

- `source` must not be empty
- `source` must not contain path traversal sequences (`../`)
- `target` must be an absolute path (or start with `~`)

Selfie validates these rules when you run `selfie spec validate`.

## Common Patterns

### Package Managers

#### Homebrew (macOS)

```yaml
environments:
  macos:
    install: brew install package-name
    check: brew list package-name
```

#### APT (Ubuntu/Debian)

```yaml
environments:
  ubuntu:
    install: |
      sudo apt update \
        && sudo apt install -y package-name
    check: dpkg -l | grep -q package-name
```

#### Pacman (Arch Linux)

```yaml
environments:
  arch:
    install: sudo pacman -S package-name
    check: pacman -Q package-name
```

#### npm (Node.js)

```yaml
environments:
  macos:
    install: npm install -g package-name
    check: npm list -g package-name
    dependencies:
      - node
```

#### pip (Python)

```yaml
environments:
  macos:
    install: pip3 install package-name
    check: pip3 show package-name
    dependencies:
      - python
```

#### Cargo (Rust)

```yaml
environments:
  macos:
    install: cargo install package-name
    check: cargo install --list | grep -q package-name
    dependencies:
      - rust
```

### Direct Downloads

#### GitHub Releases

```yaml
install: |
  set -e
  LATEST=$(curl -s https://api.github.com/repos/owner/repo/releases/latest | jq -r .tag_name)
  curl -Lo binary https://github.com/owner/repo/releases/download/${LATEST}/binary-linux-amd64
  sudo install binary /usr/local/bin/
```

#### Installer Scripts

```yaml
install: |
  set -e
  curl -fsSL https://get.example.com | bash
  source ~/.bashrc
```

#### Container Images

```yaml
install: docker pull alpine:latest
check: docker images | grep -q alpine
dependencies:
  - docker
```

### Language Runtimes

#### Node.js with Version Management

```yaml
environments:
  macos:
    install: |
      set -e

      # Install nvm if not present
      if ! command -v nvm &> /dev/null; then
        curl -o- https://raw.githubusercontent.com/nvm-sh/nvm/v0.39.0/install.sh | bash
        source ~/.bashrc
      fi

      nvm install 20
      nvm use 20
      nvm alias default 20
    check: node --version | grep -q "v20"
```

#### Python with Virtual Environment

```yaml
install: |
  set -e
  python3 -m venv ~/.virtualenvs/myproject
  source ~/.virtualenvs/myproject/bin/activate
  pip install -r requirements.txt
check: |
  set -e
  source ~/.virtualenvs/myproject/bin/activate
  python -c "import required_package"
```

## Validation Rules

Selfie validates package files according to these rules:

### Required Fields

- `name` must be present and match filename
- `environments` must contain at least one environment
- Each environment must have an `install` command

### Naming Conventions

- Package names should be lowercase with hyphens (kebab-case)
- Environment names should be descriptive and consistent
- Avoid special characters in names

### Dependencies

- Dependencies must reference valid package names
- Circular dependencies are not allowed
- Dependencies should exist in the package directory

### Commands

- Install and check commands should be valid shell scripts
- Multi-line commands should consider including `set -e` for proper error handling (stops execution
  on first failure)
- Commands should handle errors appropriately and provide meaningful error messages
- Long-running commands should provide progress feedback

## Best Practices

### 1. Make Commands Idempotent

Ensure install commands can be run multiple times safely:

```yaml
install: |
  if ! command -v tool &> /dev/null; then
    curl -Lo tool https://example.com/tool
    sudo install tool /usr/local/bin/
  fi
```

### 2. Provide Clear Error Messages

Help users understand what went wrong:

```yaml
install: |
  if ! command -v curl &> /dev/null; then
    echo "Error: curl is required but not installed"
    exit 1
  fi
  # ... rest of installation
```

### 3. Use Specific Versions

Pin to specific versions for reproducibility:

```yaml
install: |
  VERSION=1.21.5
  curl -Lo go.tar.gz https://go.dev/dl/go${VERSION}.linux-amd64.tar.gz
  sudo tar -C /usr/local -xzf go.tar.gz
```

### 4. Use External Scripts for Complex Installations

For complex multi-step installations, consider using external shell scripts. Commands automatically
run in the package directory, so you can reference scripts using relative paths:

```yaml
environments:
  ubuntu:
    install: ./my-package/ubuntu_install.sh
    check: ./my-package/check.sh

  macos:
    install: |
      # Multi-line scripts also run in package directory
      source ./my-package/common.sh
      ./my-package/macos_install.sh
    check: ./my-package/check.sh
```

**Benefits of external scripts:**

- Better code organization and reusability
- IDE syntax highlighting and linting
- Easier testing and debugging
- Version control friendly
- Shared utilities across environments

**Example structure:**

```
package-directory/
├── my-package.yaml        # Package definition (must be at root)
├── other-package.yaml     # Other packages also at root
└── my-package/            # Scripts directory named after package
    ├── common.sh          # Shared utilities
    ├── ubuntu_install.sh  # Ubuntu-specific installation
    ├── macos_install.sh   # macOS-specific installation
    └── check.sh           # Shared verification script
```

**Script path approaches:**

```yaml
# Recommended: Use relative paths (commands run in package directory)
install: ./my-package/ubuntu_install.sh

# Alternative: Multi-line with relative paths
install: |
  set -e
  source ./my-package/common.sh
  ./my-package/ubuntu_install.sh

# Absolute paths work too of, course
install: /absolute/path/to/packages/my-package/ubuntu_install.sh
```

**Script patterns:**

```bash
#!/bin/bash
set -e

# Get the directory where this script is located
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "$SCRIPT_DIR/common.sh"

log_info "Installing my-package..."
# Installation logic here
verify_installation
```

See `docs/examples/docker-scripted.yaml` and `docs/examples/docker-scripted/` for a complete
example.

### 4. Handle Different Architectures

Account for different CPU architectures:

```yaml
install: |
  ARCH=$(uname -m)
  case $ARCH in
    x86_64) ARCH=amd64 ;;
    aarch64) ARCH=arm64 ;;
    *) echo "Unsupported architecture: $ARCH"; exit 1 ;;
  esac
  curl -Lo binary https://example.com/binary-linux-${ARCH}
```

### 5. Cleanup After Installation

Remove temporary files:

```yaml
install: |
  cd /tmp
  curl -Lo installer.tar.gz https://example.com/installer.tar.gz
  tar xzf installer.tar.gz
  sudo ./install.sh
  rm -rf installer.tar.gz install.sh
```

### 6. Verify Installation

Add verification steps to install commands:

```yaml
install: |
  brew install package-name
  if ! command -v package-name &> /dev/null; then
    echo "Installation failed: package-name not found in PATH"
    exit 1
  fi
  echo "Successfully installed package-name"
```

## Troubleshooting

### Common Issues

#### Package Not Found

```
Error: Package 'package-name' not found
```

- Ensure the package file exists in your package directory
- Check that the filename matches the package name
- Verify the file has a `.yaml` extension

#### Validation Errors

```
Error: Package validation failed
```

- Run `selfie spec validate package-name` for detailed errors
- Check YAML syntax with a YAML validator
- Ensure all required fields are present

#### Installation Failures

```
Error: Installation command failed
```

- Run with `--verbose` flag for detailed output
- Check that all dependencies are installed
- Verify commands work in your shell manually
- Ensure you have necessary permissions

#### Check Command Failures

```
Warning: Check command failed
```

- Verify the check command syntax
- Test the check command manually
- Ensure the check command matches the actual installation location

### Debugging Tips

1. **Use verbose mode**: `selfie --verbose package install package-name`
2. **Test commands manually**: Run install/check commands in your shell
3. **Check dependencies**: Ensure all dependencies are properly installed
4. **Validate syntax**: Use `selfie spec validate package-name`
5. **Check permissions**: Ensure you have necessary permissions for installation paths
6. **Review logs**: Check system logs for additional error information

## Examples

See the [examples directory](examples/) for complete package definitions covering:

- Popular development tools
- Language runtimes
- Container platforms
- Cloud CLI tools
- Text editors and IDEs
- System utilities

## Migration from Other Tools

### From Homebrew Bundle

Convert `Brewfile` entries:

```ruby
# Brewfile
brew "ripgrep"
cask "docker"
```

```yaml
# ripgrep.yaml
name: ripgrep
environments:
  macos:
    install: brew install ripgrep
    check: which rg
```

### From package.json

Convert global npm packages:

```json
{
  "devDependencies": {
    "typescript": "^5.0.0"
  }
}
```

```yaml
# typescript.yaml
name: typescript
environments:
  macos:
    install: npm install -g typescript@5.0.0
    check: which tsc
    dependencies: [node]
```

### From requirements.txt

Convert Python packages:

```
black==23.0.0
flake8==6.0.0
```

```yaml
# black.yaml
name: black
environments:
  macos:
    install: pip install black==23.0.0
    check: which black
    dependencies: [python]
```
