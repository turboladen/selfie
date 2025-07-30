# Docker Package Scripts

This directory demonstrates how to organize complex package installations using external shell
scripts. This approach provides better maintainability, reusability, and testing capabilities
compared to embedding all installation logic directly in YAML files.

## File Structure

```
package-directory/
├── docker-scripted.yaml    # Package definition
└── docker-scripted/
    ├── README.md           # This file
    ├── common.sh          # Shared utilities and functions
    ├── ubuntu_install.sh  # Ubuntu-specific installation
    ├── macos_install.sh   # macOS-specific installation
    ├── ci_install.sh      # CI/CD environment installation
    └── check.sh           # Shared verification script
```

## Benefits of External Scripts

### 1. **Better Organization**

- Separate concerns (installation vs. verification vs. utilities)
- Easier to navigate and understand
- Clear separation between different environments

### 2. **Code Reusability**

- Common functions can be shared across scripts
- Installation logic can be composed and extended
- Scripts can call other scripts for modularity

### 3. **Development Experience**

- Full IDE support with syntax highlighting
- Linting and static analysis
- Easier debugging and testing
- Version control friendly (meaningful diffs)

### 4. **Maintainability**

- Individual scripts can be updated independently
- Easier to test components in isolation
- Better error handling and logging

## Usage Patterns

### Pattern 1: Direct Script Execution

```yaml
environments:
  ubuntu:
    install: ./docker-scripted/ubuntu_install.sh
    check: ./docker-scripted/check.sh
```

### Pattern 2: Script with Common Utilities

```yaml
environments:
  arch:
    install: |
      source ./docker-scripted/common.sh

      log_info "Installing Docker on Arch Linux..."
      sudo pacman -S --noconfirm docker docker-compose
      add_user_to_docker_group
      start_docker_service
      verify_docker_installation
```

### Pattern 3: Script Composition

```yaml
environments:
  development:
    install: |
      source ./docker-scripted/common.sh

      # Install Docker using OS-specific script
      case "$(detect_os)" in
        ubuntu) ./docker-scripted/ubuntu_install.sh ;;
        macos)  ./docker-scripted/macos_install.sh ;;
        *)      log_error "Unsupported OS"; exit 1 ;;
      esac

      # Add development-specific configuration
      setup_development_environment
```

## Script Guidelines

### Error Handling

All scripts should use proper error handling:

```bash
#!/bin/bash
set -e  # Exit on first error

# Your installation logic here
```

### Logging

Use the common logging functions for consistent output:

```bash
source ./docker-scripted/common.sh

log_info "Starting installation..."
log_success "Installation completed!"
log_warning "This is a warning"
log_error "This is an error"
```

### Function Organization

Keep functions focused and reusable:

````bash
# Good: Specific, testable function
install_docker_prerequisites() {
    log_info "Installing prerequisites..."
    sudo apt-get install -y ca-certificates curl gnupg
}

```bash
# Good: Composable function
setup_docker_repository() {
    log_info "Setting up Docker repository..."
    # Repository setup logic
}
````

## Testing Scripts

You can test individual scripts outside of selfie:

```bash
# Test the Ubuntu installation script (from package directory)
cd docs/examples
chmod +x docker-scripted/*.sh
./docker-scripted/ubuntu_install.sh

# Test the check script
./docker-scripted/check.sh

# Test with common utilities
source docker-scripted/common.sh
print_system_info
```

## Environment Variables

Scripts can access environment variables and selfie context:

```bash
# Commands automatically run in package directory
# No need to change directories manually

# User information
echo "Installing for user: $USER"

# System information
OS="$(detect_os)"
echo "Detected OS: $OS"
```

## Advanced Patterns

### Conditional Script Loading

```bash
# Load OS-specific functions
case "$(detect_os)" in
    ubuntu|debian) source ./docker-scripted/debian_functions.sh ;;
    centos|fedora) source ./docker-scripted/redhat_functions.sh ;;
    macos)         source ./docker-scripted/macos_functions.sh ;;
esac
```

### Configuration Files

```bash
# Load configuration from external file
if [ -f "./docker-scripted/docker.conf" ]; then
    source ./docker-scripted/docker.conf
fi

# Use configuration
DOCKER_VERSION="${DOCKER_VERSION:-latest}"
```

### Script Dependencies

```bash
# Ensure prerequisites are met
check_prerequisites() {
    command_exists curl || { log_error "curl is required"; exit 1; }
    command_exists sudo || { log_error "sudo is required"; exit 1; }
}
```

## Integration with Selfie

When using external scripts with selfie:

1. **Working Directory**: Commands automatically run in the package directory (where the `.yaml`
   file is located), so you can use relative paths like `./scripts/install.sh` directly
2. **Error Handling**: Use `set -e` or proper error checking to ensure selfie detects failures
3. **Output**: Use consistent logging for better user experience
4. **Permissions**: Ensure scripts are executable (`chmod +x`)
5. **Relative Paths**: Use paths relative to the package directory (e.g.,
   `./docker-scripted/common.sh`)

## Best Practices

1. **Use relative paths** since commands automatically run in the package directory
2. **Include proper error handling** with `set -e` in scripts
3. **Use shared utilities** from `common.sh` for consistency
4. **Document script purpose** and usage in comments
5. **Test scripts independently** before integrating with selfie
6. **Keep scripts focused** on a single responsibility
7. **Use meaningful function names** that describe their purpose
8. **Validate prerequisites** before attempting installation

This approach scales well for complex packages and provides a much better development and
maintenance experience compared to inline YAML scripts.
