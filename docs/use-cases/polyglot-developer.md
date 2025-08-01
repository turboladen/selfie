# Use Case: Polyglot Developer on macOS

This guide shows how a polyglot developer working across multiple programming languages and tools
can use selfie to manage their development environment consistently.

## The Scenario

Meet Alex, a full-stack developer who works with:

- **Frontend**: Node.js, TypeScript, React
- **Backend**: Python, Rust, Go
- **DevOps**: Docker, Kubernetes, Terraform
- **Tools**: Various CLI utilities for productivity

Alex uses macOS as their primary development machine and prefers version managers for language
runtimes:

- `fnm` for Node.js version management
- `uv` for Python package and environment management
- `rustup` for Rust toolchain management
- Homebrew for system tools and CLI utilities
- Language-specific package managers (npm, uv, cargo) for tools in those ecosystems

The challenge: many tools are available via multiple package managers, and Alex wants to avoid
conflicts between version managers and system package managers.

## The Challenge

Without selfie, Alex has to remember:

- `brew install jq` for JSON processing, but not `brew install node` (conflicts with fnm)
- `npm install -g typescript` for TypeScript (using fnm-managed node)
- `uv tool install black` for Python formatting (using uv-managed python)
- `cargo install ripgrep` for fast search
- Which tools to install via npm vs homebrew when both are available (e.g., `yaml-language-server`)
- Manual download and installation for some tools

**The conflict problem**: Tools like `yaml-language-server` are available via homebrew, but that
would install Node.js via homebrew too, conflicting with Alex's fnm-managed Node.js versions. Alex
prefers to install it via npm using the fnm-managed node instead. Similarly, many Python tools are
available via homebrew, but Alex prefers using `uv` which handles both Python versions and tool
installation in isolated environments.

When setting up a new machine or helping a teammate, Alex has to remember all these different
commands, package managers, and conflict avoidance strategies.

## The Selfie Solution

Alex creates a selfie package repository to encode all their tool preferences once.

### 1. Initial Setup

```bash
# Create package repository
mkdir ~/my-dev-packages
cd ~/my-dev-packages
git init

# Configure selfie
cat > ~/.config/selfie/config.yml << EOF
environment: macos-work
package_directory: ~/my-dev-packages
EOF
```

### 2. Core Development Tools

#### Node.js Version Manager and TypeScript

```yaml
# fnm.yaml
name: fnm
description: Fast Node.js version manager
homepage: https://github.com/Schniz/fnm

environments:
  macos:
    install: brew install fnm
    check: which fnm
```

```yaml
# node.yaml
name: node
description: Node.js runtime via fnm
homepage: https://nodejs.org

environments:
  macos:
    install: |
      set -e

      fnm install --lts
      fnm use lts-latest
      fnm default lts-latest
    check: node --version && npm --version
    dependencies:
      - fnm
```

```yaml
# typescript.yaml
name: typescript
description: TypeScript compiler and language server
homepage: https://www.typescriptlang.org

environments:
  macos:
    install: npm install -g typescript
    check: which tsc && tsc --version
    dependencies:
      - node
```

#### Python with uv

```yaml
# uv.yaml
name: uv
description: Fast Python package and project manager
homepage: https://github.com/astral-sh/uv

environments:
  macos:
    install: |
      set -e

      # Install uv via official installer (preferred method)
      curl -LsSf https://astral.sh/uv/install.sh | sh
      # Source the environment to make uv available
      source ~/.local/bin/env
    check: which uv && uv --version
```

```yaml
# python.yaml
name: python
description: Python 3 via uv
homepage: https://python.org

environments:
  macos:
    install: |
      set -e

      # Install Python 3.11 via uv
      uv python install 3.11
      # Set as default for projects
      uv python pin 3.11
    check: uv python list | grep -q "3.11" && uv python which python | grep -q "3.11"
    dependencies:
      - uv
```

```yaml
# black.yaml
name: black
description: Python code formatter via uv
homepage: https://black.readthedocs.io

environments:
  macos:
    install: uv tool install black
    check: which black && black --version
    dependencies:
      - uv
```

```yaml
# ruff.yaml
name: ruff
description: Extremely fast Python linter and formatter
homepage: https://github.com/astral-sh/ruff

environments:
  macos:
    install: uv tool install ruff
    check: which ruff && ruff --version
    dependencies:
      - uv
```

#### Rust Toolchain and Tools

```yaml
# rust.yaml
name: rust
description: Rust programming language toolchain via rustup
homepage: https://www.rust-lang.org

environments:
  macos:
    install: |
      set -e
      curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
      source ~/.cargo/env
      rustup component add rust-analyzer
    check: which rustc && which cargo && rustc --version
```

```yaml
# ripgrep.yaml
name: ripgrep
description: Fast text search tool
homepage: https://github.com/BurntSushi/ripgrep

environments:
  macos:
    install: cargo install ripgrep
    check: which rg && rg --version
    dependencies:
      - rust
```

### 3. System and DevOps Tools

#### Core CLI Utilities

```yaml
# jq.yaml
name: jq
description: Command-line JSON processor
homepage: https://jqlang.github.io/jq/

environments:
  macos:
    install: brew install jq
    check: which jq && jq --version
```

```yaml
# bat.yaml
name: bat
description: Cat with syntax highlighting
homepage: https://github.com/sharkdp/bat

environments:
  macos:
    install: brew install bat
    check: which bat && bat --version
```

```yaml
# fd.yaml
name: fd
description: Fast alternative to find
homepage: https://github.com/sharkdp/fd

environments:
  macos:
    install: brew install fd
    check: which fd && fd --version
```

#### Container and Cloud Tools

```yaml
# docker.yaml
name: docker
description: Container platform
homepage: https://www.docker.com

environments:
  macos:
    install: brew install --cask docker
    check: which docker && docker --version
```

```yaml
# kubectl.yaml
name: kubectl
description: Kubernetes command-line tool
homepage: https://kubernetes.io

environments:
  macos:
    install: brew install kubectl
    check: which kubectl && kubectl version --client
```

```yaml
# terraform.yaml
name: terraform
description: Infrastructure as code tool
homepage: https://www.terraform.io

environments:
  macos:
    install: brew install terraform
    check: which terraform && terraform version
```

#### Git and Development Tools

```yaml
# git-delta.yaml
name: git-delta
description: Better git diff viewer
homepage: https://github.com/dandavison/delta

environments:
  macos:
    install: brew install git-delta
    check: which delta && delta --version
```

```yaml
# gh.yaml
name: gh
description: GitHub CLI
homepage: https://cli.github.com

environments:
  macos:
    install: brew install gh
    check: which gh && gh --version
```

### 4. Language Servers and Development Tools

This is where package manager conflicts become most apparent. Many language servers are available
via multiple package managers.

```yaml
# yaml-language-server.yaml
name: yaml-language-server
description: YAML language server (via npm to avoid node conflicts)
homepage: https://github.com/redhat-developer/yaml-language-server

environments:
  macos:
    # Install via npm using fnm-managed node, not homebrew
    # This avoids installing a second node runtime via homebrew
    install: npm install -g yaml-language-server
    check: which yaml-language-server
    dependencies:
      - node
```

```yaml
# pylsp.yaml
name: pylsp
description: Python Language Server Protocol implementation
homepage: https://github.com/python-lsp/python-lsp-server

environments:
  macos:
    install: uv tool install 'python-lsp-server[all]'
    check: which pylsp && pylsp --version
    dependencies:
      - uv
```

```yaml
# typescript-language-server.yaml
name: typescript-language-server
description: TypeScript language server
homepage: https://github.com/typescript-language-server/typescript-language-server

environments:
  macos:
    install: npm install -g typescript-language-server
    check: which typescript-language-server
    dependencies:
      - typescript
```

### 5. Custom and Direct Install Tools

For tools not available via

## Daily Workflow

### Setting Up a New Machine

```bash
# Clone package repository
git clone https://github.com/alex/my-dev-packages.git ~/my-dev-packages

# Configure selfie
cat > ~/.config/selfie/config.yml << EOF
environment: macos
package_directory: ~/my-dev-packages
EOF

# Install core development stack
selfie package install fnm
selfie package install node
selfie package install uv
selfie package install python
selfie package install rust
selfie package install docker

# Install development tools
selfie package install typescript
selfie package install black
selfie package install ruff
selfie package install ripgrep
selfie package install jq
selfie package install kubectl
```

### Adding a New Tool

When Alex discovers a new tool they want to use:

```bash
# Create package definition
selfie package create new-tool --interactive

# Edit with preferred installation method
selfie package edit new-tool

# Test installation
selfie package install new-tool

# Commit to repository
git add new-tool.yaml
git commit -m "Add new-tool package"
git push
```

### Checking Development Environment

```bash
# Verify all tools are installed
selfie package list | while read package; do
  echo "Checking $package..."
  selfie package check "$package"
done

# Quick check of essential tools
for tool in fnm node uv python rust docker jq; do
  selfie package check "$tool"
done
```

### Updating Tools

```bash
# Check which tools need updates (manual process)
selfie package info typescript  # Check current version info
npm outdated -g typescript      # Check if updates available

# Update package definition if needed
selfie package edit typescript

# Reinstall if necessary
selfie package install typescript
```

## Benefits for Alex

1. **Consistency**: Same installation logic across all machines
2. **Documentation**: Package files serve as documentation of tool choices
3. **Portability**: Can reproduce development environment anywhere
4. **Recovery**: Easy disaster recovery - just run selfie installs
5. **Memory**: No need to remember different package manager commands
6. **Version control**: Can track changes to tool preferences over time

## Package Repository Structure

Alex's final repository looks like:

```
my-dev-packages/
├── README.md
├── bat.yaml
├── black.yaml
├── docker.yaml
├── fd.yaml
├── fnm.yaml
├── gh.yaml
├── git-delta.yaml
├── jq.yaml
├── kubectl.yaml
├── lazygit.yaml
├── node.yaml
├── pylsp.yaml
├── python.yaml
├── ripgrep.yaml
├── ruff.yaml
├── rust.yaml
├── terraform.yaml
├── typescript.yaml
├── typescript-language-server.yaml
├── uv.yaml
└── yaml-language-server.yaml
```

All package files are stored in the root directory since selfie flattens any directory structure
when loading packages.

## Tips for Polyglot Developers

1. **Start with core tools**: Begin with version managers (fnm, uv, rustup) then language tools
2. **Use dependencies**: Let selfie handle tool installation order
3. **Version your packages**: Keep package definitions in git for history
4. **Avoid conflicts**: Use language-specific package managers over OS package managers
5. **Follow upstream recommendations**: Use each tool's preferred installation method
6. **Test on clean systems**: Validate your packages work on fresh installations
7. **Document special cases**: Add comments for complex installation procedures
8. **Environment-specific**: Prepare for different environments (Linux, CI, etc.)
9. **Keep it personal**: Focus on your own workflow and preferences
