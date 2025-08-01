# Example Package Definitions

This directory contains example package definitions for common development tools and utilities.

## Usage

Copy any of these examples to your package directory and customize them for your needs:

```bash
# Copy an example to your package directory
cp docs/examples/ripgrep.yaml ~/my-packages/

# Edit to match your preferences
selfie package edit ripgrep

# Install the package
selfie package install ripgrep
```

## Environment Support

Most examples include configurations for:

- **macOS** - Using version managers and Homebrew strategically
- **Ubuntu** - Using version managers, apt, and direct downloads
- **Arch Linux** - Using pacman and version managers
- **CI** - Using direct downloads and binaries for reproducibility

## Customization Tips

1. **Modify installation methods**: Change package managers or installation sources
2. **Add environments**: Include your specific OS or distribution
3. **Update versions**: Pin to specific versions for reproducibility
4. **Add dependencies**: Include required dependencies for your setup
5. **Customize checks**: Adjust verification commands for your needs

## Contributing Examples

If you have package definitions for tools not covered here, consider contributing them:

1. Follow the naming convention: `tool-name.yaml`
2. Include multiple environment configurations
3. Add comprehensive check commands
4. Document any special requirements
5. Test on clean systems
6. Focus on personal development workflows

## Common Patterns

### Version Manager Installation

```yaml
environments:
  macos:
    install: fnm install --lts && fnm use lts-latest
    check: node --version | grep -q "v20"
    dependencies:
      - fnm
```

### Package Manager Installation

```yaml
environments:
  macos:
    install: brew install tool-name
    check: which tool-name
  ubuntu:
    install: sudo apt install -y tool-name
    check: which tool-name
```

### Direct Download

```yaml
environments:
  ubuntu:
    install: |
      set -e
      curl -Lo tool https://releases.example.com/tool-linux-amd64
      sudo install tool /usr/local/bin/
    check: which tool
```

### Language-Specific Tools (avoiding conflicts)

```yaml
environments:
  macos:
    # Use npm with fnm-managed node, not homebrew node
    install: npm install -g tool-name
    check: npm list -g tool-name
    dependencies:
      - node # This should install via fnm, not homebrew
```

### Version Verification

```yaml
check: tool --version | grep -q "expected-version"
```

### Multi-Step Installation

```yaml
install: |
  # Exit on error
  set -e
  # Download
  curl -Lo installer.sh https://get.example.com/install.sh
  # Make executable
  chmod +x installer.sh
  # Run installer
  ./installer.sh
  # Cleanup
  rm installer.sh
```

## Testing

Test example packages in a clean environment:

```bash
# Validate package syntax
selfie package validate tool-name

# Test installation (use with caution)
selfie package install tool-name

# Verify check command
selfie package check tool-name
```

## Troubleshooting

Common issues with example packages:

1. **Permission errors**: Ensure install commands have appropriate sudo usage
2. **Path issues**: Verify tools are installed in PATH locations
3. **Version mismatches**: Update version checks to match installed versions
4. **Dependency failures**: Ensure all dependencies are properly defined
5. **Platform differences**: Test on actual target platforms

For more guidance, see:

- [Package Files Reference](../package-files.md)
- [Getting Started Guide](../getting-started.md)
- [Polyglot Developer Use Case](../use-cases/polyglot-developer.md)
