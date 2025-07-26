# Configuration Guide

This guide covers all aspects of configuring selfie for optimal use in your development environment.

## Overview

Selfie uses a configuration file to define global settings that apply across all package operations.
The configuration determines your current environment, package directory location, and other
behavioral settings.

## Configuration File Location

Selfie looks for configuration files in this order:

1. `~/.config/selfie/config.yaml` (primary format)
2. `~/.config/selfie/config.yml` (alternative format)

You can also override the configuration directory using the `SELFIE_CONFIG_DIR` environment
variable.

## Environment Variables

Selfie recognizes several environment variables that affect its behavior:

### `SELFIE_CONFIG_DIR`

Override the default configuration directory location. When set, selfie will look for configuration
files in this directory instead of `~/.config/selfie/`.

**Example:**

```bash
export SELFIE_CONFIG_DIR=/custom/config/path
selfie config validate
```

### `EDITOR`

Specifies which editor to use for the `selfie package edit` command. This environment variable is
required when using package editing functionality.

**Example:**

```bash
export EDITOR=code    # Use VS Code
export EDITOR=vim     # Use Vim
export EDITOR=nano    # Use Nano

selfie package edit my-package
```

If `EDITOR` is not set, the `selfie package edit` command will fail with an error message
instructing you to set this environment variable.

If no configuration file is found, selfie will create a default configuration at
`~/.config/selfie/config.yml`.

## Basic Configuration

### Minimal Configuration

The simplest configuration requires only two settings:

```yaml
environment: macos
package_directory: ~/.config/selfie/packages
```

### Full Configuration Example

```yaml
# Current environment name
environment: macos

# Directory containing package definition files
package_directory: ~/.config/selfie/packages

# Verbosity level (default: false)
verbose: false

# Use colored output (default: true)
use_colors: true

# Command timeout in seconds (default: 60)
command_timeout: 300

# Stop on first error (default: true)
stop_on_error: true

# Maximum parallel installations (default: number of CPUs)
max_parallel_installations: 4
```

## Required Settings

### `environment`

Specifies which environment configuration to use when installing packages. This must match an
environment name defined in your package files.

```yaml
environment: macos
```

**Common environment names:**

- `macos`, `macos-work`, `macos-home` - macOS systems with context
- `ubuntu`, `debian`, `fedora`, `arch` - Linux distributions
- `linux-dev`, `linux-ci` - Linux with context
- `windows` - Windows systems
- `ci`, `github-actions` - CI/CD environments
- `dev`, `staging`, `prod` - Deployment environments

### `package_directory`

Path to the directory containing your package definition files. Can be absolute or relative to your
home directory.

```yaml
package_directory: ~/.config/selfie/packages
```

**Examples:**

```yaml
# Absolute path
package_directory: /home/user/my-packages

# Relative to home directory
package_directory: ~/dev-packages

# Using environment variables
package_directory: ${SELFIE_PACKAGES:-~/.config/selfie/packages}
```

## Optional Settings

### Global Behavior

#### `verbose`

Enable verbose output by default.

```yaml
verbose: true
```

#### `use_colors`

Control colored output.

```yaml
use_colors: false
```

#### `command_timeout`

Default timeout for package operations in seconds.

```yaml
command_timeout: 600 # 10 minutes
```

#### `stop_on_error`

Whether to stop on first error during operations.

```yaml
stop_on_error: false
```

#### `max_parallel_installations`

Number of parallel operations for dependency installation.

```yaml
max_parallel_installations: 2
```

## Environment Naming Strategies

Environment names can be simple OS identifiers or context-specific:

```yaml
# Simple OS-based naming
environment: macos

# Context-specific naming for different scenarios
environment: macos-work # Work laptop configuration
environment: macos-home # Personal machine configuration
environment: ubuntu-dev # Development server
environment: ci-github # GitHub Actions environment
```

This allows you to have different package installation preferences for different contexts even on
the same OS.

## Command-Line Overrides

All configuration options can be overridden via command-line flags:

```bash
# Override environment
selfie --environment=linux package install node

# Override package directory
selfie --package-directory=/path/to/packages package list

# Enable verbose mode
selfie --verbose package install docker

# Disable colors
selfie --no-color package list
```

## Configuration Validation

Validate your configuration file:

```bash
selfie config validate
```

This checks:

- YAML syntax
- Required fields presence
- Path accessibility
- Environment name validity
- Repository connectivity (if configured)

## Troubleshooting

### Unknown Configuration Fields

```
Error: Configuration contains unknown fields
```

**Solution:** Check field names against supported options:

```bash
# Validate configuration
selfie config validate

# Supported fields: environment, package_directory, verbose, use_colors,
# command_timeout, stop_on_error, max_parallel_installations
```

### EDITOR Environment Variable Not Set

```
Error: EDITOR environment variable is not set.
```

**Solution:** Set the EDITOR environment variable to your preferred editor:

```bash
# Temporarily for current session
export EDITOR=code    # VS Code
export EDITOR=vim     # Vim
export EDITOR=nano    # Nano

# Permanently in your shell profile (~/.bashrc, ~/.zshrc, etc.)
echo 'export EDITOR=code' >> ~/.bashrc
```

This environment variable is required for the `selfie package edit` command.

## Best Practices

1. **Version control**: Keep configuration files in version control
2. **Environment separation**: Use different configurations for different environments
3. **Minimal configuration**: Start with minimal settings, add complexity as needed
4. **Documentation**: Comment your configuration files
5. **Validation**: Regularly validate configuration with `selfie config validate`
6. **Backup**: Keep backups of working configurations
7. **Team consistency**: Use shared configuration templates for teams
8. **Security**: Never commit sensitive data like tokens to version control
