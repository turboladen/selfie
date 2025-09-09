# Selfie Multi-Distribution Testing

Test the Selfie Rust CLI across multiple Linux distributions using Tilt and Docker Compose.

## Quick Start

```bash
# Start containers
just docker start
# Start Tilt UI
tilt up
# Open http://localhost:10350

# Run tests on all distributions
just tilt test

# Test specific distribution
just tilt test-debian
just tilt test-alpine
```

## Distributions

- **Debian 12** - `.deb` + `apt` + `glibc` (stable ecosystem)
- **Alpine Linux** - `apk` + `musl` (minimal/security-focused)

Perfect coverage of different package managers and libc variants.

## Using the Tilt UI

1. **Start containers**: `just docker start`, then **Start Tilt**: `tilt up`
2. **Open UI**: http://localhost:10350
3. **Run tests**: Click the trigger button for:
   - `test-all` - Run tests on all distributions
   - `debian-tests` - Test only Debian
   - `alpine-tests` - Test only Alpine
4. **View logs**: Click on any resource to see real-time output
5. **Stop**: `tilt down` or Ctrl+C

The Tilt UI provides visual feedback and real-time logs for all operations.

## Just Commands

### Essential Commands

```bash
just docker start   # Start containers
tilt up            # Start Tilt UI
just tilt test     # Run tests on all distributions
just docker clean  # Clean up everything
```

### Distribution-Specific Testing

```bash
just tilt test-debian    # Test only on Debian
just tilt test-alpine    # Test only on Alpine
```

### Container Access

```bash
just docker shell-debian   # Open shell in Debian container
just docker shell-alpine   # Open shell in Alpine container
```

### Local Development

```bash
just local-test     # Run tests locally (not in containers)
just local-clippy   # Run clippy locally
just build          # Build locally
just fmt            # Format code with dprint and cargo
just docs           # Generate documentation
```

### Docker Management

```bash
just docker start          # Start all containers
just docker stop           # Stop all containers
just docker status         # Show container status
just docker build-images   # Build Docker images
just docker logs           # Show container logs
```

## Running Selfie Commands in Containers

### Method 1: Interactive Shell

```bash
# Get a shell in any distribution
just docker shell-debian
just docker shell-alpine

# Inside the container, run selfie commands:
cargo run -- package list
cargo run -- package validate my-package.yml
cargo run -- config validate
cargo run -- --help
```

### Method 2: Direct Commands

```bash
# Run commands directly in containers
docker-compose exec -T debian cargo run -- package list
docker-compose exec -T alpine cargo run -- --help
docker-compose exec -T debian cargo run -- config validate

# Test specific functionality
docker-compose exec -T alpine cargo test --test command_execution_tests
docker-compose exec -T debian cargo test package_validation
```

### Method 3: Via Tilt (for testing)

```bash
# Through Tilt resources
tilt trigger test-all           # Run all tests
tilt trigger debian-tests       # Run tests on Debian
tilt trigger alpine-tests       # Run tests on Alpine
```

## Common Development Workflows

### Quick Test Cycle

```bash
# Make code changes, then:
just tilt test-debian    # Quick test on one distro
just tilt test           # Full test across all distros
```

### Debugging Test Failures

```bash
# Get into the failing environment
just docker shell-alpine

# Inside container:
cargo test test_that_failed
cargo run -- package list --debug
```

### Cross-Platform Package Manager Testing

```bash
# Test package commands across different package managers
docker-compose exec -T debian bash -c "apt list --installed | head -5"
docker-compose exec -T alpine bash -c "apk list --installed | head -5"

# Test selfie with different package managers available
docker-compose exec -T debian cargo run -- package check my-package.yml
docker-compose exec -T alpine cargo run -- package check my-package.yml
```

### Performance Testing

```bash
# Compare performance across distributions
docker-compose exec -T debian time cargo run -- package list
docker-compose exec -T alpine time cargo run -- package list
```

## File Structure

```
selfie-v4/
├── Tiltfile                    # Tilt orchestration
├── docker-compose.yml         # Container definitions
├── Justfile                   # Command shortcuts
├── docker/
│   ├── debian/Dockerfile     # Debian + Rust environment
│   └── alpine/Dockerfile     # Alpine + Rust environment
└── target/
    ├── debian/               # Debian build artifacts
    └── alpine/               # Alpine build artifacts
```

## Features

- ✅ **Warning-free Tilt setup** - Loads cleanly without dependency issues
- ✅ **ARM64 native** - Optimized for Apple Silicon performance
- ✅ **Fast builds** - Cached layers and optimized Dockerfiles
- ✅ **Separate build artifacts** - Per-distribution target directories
- ✅ **CI-equivalent testing** - Reproduces production environments

## Troubleshooting

### Container Issues

```bash
just docker status                    # Check container status
just docker logs                     # View all container logs
docker-compose logs debian           # View specific container logs
just docker build-images             # Rebuild images if needed
```

### Clean Slate

```bash
just docker clean                     # Remove all containers and volumes
just docker rebuild-images           # Rebuild from scratch
```

### Tilt Issues

```bash
tilt down                      # Stop Tilt
tilt up                        # Restart Tilt
```

## Prerequisites

- **Docker** and **Docker Compose**
- **Tilt**: `brew install tilt-dev/tap/tilt`
- **Just**: `brew install just`

Each distribution has Rust pre-installed and ready for testing.
