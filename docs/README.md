# Selfie Documentation

Welcome to the comprehensive documentation for selfie, a personal package manager that remembers how
you like to install things.

## Quick Navigation

### 🚀 Getting Started

- [**Getting Started Guide**](getting-started.md) - Installation, setup, and first package
- [**Configuration Guide**](configuration.md) - Environment setup and options

### 📖 Core Concepts

- [**Package Files Reference**](package-files.md) - Complete package definition format
- [**Example Packages**](examples/) - Ready-to-use package definitions

### 🎯 Real-World Usage

- [**Polyglot Developer**](use-cases/polyglot-developer.md) - Individual developer workflow

## Documentation Structure

```
docs/
├── README.md                    # This file - navigation guide
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

## Quick Start Path

1. **Install selfie**: Follow [getting started](getting-started.md#installation)
2. **Basic setup**: Create your [first configuration](getting-started.md#initial-setup)
3. **Create packages**: Start with [example packages](examples/)
4. **Advanced usage**: Explore [use cases](use-cases/) for your scenario

## Common Questions

### How is selfie different from other package managers?

Selfie is a **meta-package manager** that orchestrates your existing package managers (homebrew,
apt, npm, pip, etc.) based on your preferences and environment. Unlike traditional package managers:

- **Multi-platform**: Same package definition works on macOS, Linux, and CI
- **Multi-manager**: Use homebrew, apt, npm, cargo, etc. in the same workflow
- **Personal**: You control installation methods and preferences
- **Flexible**: Works with any installation method, not just package repositories

### What are the main use cases?

- **Polyglot developers**: Manage tools across multiple programming languages
- **Multi-environment**: Consistent tool setup across macOS, Linux, CI, etc.
- **Documentation**: Your package files document your tool preferences
- **Reproducibility**: Recreate your development environment anywhere
- **Conflict avoidance**: Use the right package manager for each tool

### How do I get started?

The fastest path is:

1. [Install selfie](getting-started.md#installation) (`cargo install selfie-cli`)
2. [Create basic config](getting-started.md#initial-setup)
3. [Try an example](examples/ripgrep.yaml) (`selfie package create ripgrep`)
4. [Install your first package](getting-started.md#your-first-package)
   (`selfie package install ripgrep`)

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
- **Contributing**: See [CONTRIBUTING.md](../CONTRIBUTING.md) for contribution guidelines

## Advanced Topics

### For Multi-Environment Setup

- [Package repository management](getting-started.md#package-repository-setup)
- [Environment-specific configuration](configuration.md#environment-specific-configuration)
- [Multi-environment workflows](configuration.md#multi-environment-workflow)

### For Package Authors

- [Package validation rules](package-files.md#validation-rules)
- [Best practices](package-files.md#best-practices)
- [Advanced features](package-files.md#advanced-features)

### For Advanced Users

- [Environment-specific configuration](configuration.md#environment-specific-configuration)
- [Multi-environment workflows](configuration.md#multi-environment-workflow)
- [Security considerations](configuration.md#security-considerations)

## What's Next?

After reading the documentation:

1. **Try it out**: Install selfie and create your first package
2. **Share feedback**: Let us know how selfie works for your use case
3. **Contribute**: Add package examples or documentation improvements
4. **Share examples**: Contribute your package definitions to help others

Ready to get started? Head to the [Getting Started Guide](getting-started.md)!
