# Selfie Development Tasks

# Show available commands
default:
    @just --list

# Run all quality checks (pre-commit)
check: fmt clippy test
    @echo "All checks passed."

# Format code
fmt:
    cargo fmt
    dprint fmt

# Check formatting without modifying
fmt-check:
    cargo fmt --check
    dprint check

# Lint with clippy (zero warnings)
clippy:
    cargo clippy --all-targets -- -D warnings

# Run all tests
test:
    cargo test

# Test library only
test-lib:
    cargo test -p selfie

# Test CLI only
test-cli:
    cargo test -p selfie-cli

# Build all crates
build:
    cargo build

# Generate and open documentation
docs:
    cargo doc --open

# Check for outdated dependencies
outdated:
    cargo outdated
