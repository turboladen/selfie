# Selfie Development Tasks

# Show available commands
default:
    @just --list

# CI calls these same recipes (`.github/workflows/ci.yml`), so a recipe
# changed here changes CI with it. The typos job is the one exception: it
# uses the crate-ci action, which pins the tool version.

# Run every gate CI runs except `hack`, in order, stopping at the first failure. selfie has one
# feature, and CI's hack job covers building and testing it both ways.
check: fmt typos clippy build test docs-check
    @echo "All checks passed."

# Format code
fmt:
    cargo fmt
    dprint fmt

# Check formatting without modifying
fmt-check:
    cargo fmt --check
    dprint check

# Spell-check the tree
typos:
    @command -v typos >/dev/null || { echo "typos is not installed; run: cargo install typos-cli" >&2; exit 1; }
    typos

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

# Test MCP server only
test-mcp:
    cargo test -p selfie-mcp

# `selfie` is the only crate with optional features, and a plain `cargo test`
# always builds it with `with_mocks` on because `test-common` requests it, so
# only this catches a feature that has stopped compiling on its own.

# Test every feature combination of the library
hack:
    @command -v cargo-hack >/dev/null || { echo "cargo-hack is not installed; run: cargo install cargo-hack" >&2; exit 1; }
    cargo hack --each-feature test -p selfie

# Build all crates
build:
    cargo build

# `--all-features` documents the `with_mocks` items too; without it their
# links go unchecked. The previous run's output is removed first: with a reused
# target directory, `cargo doc` exits 101 because `doc/selfie` is not empty,
# which is stale output and not a broken doc comment. The path is asked of
# cargo because `CARGO_TARGET_DIR` and `build.target-dir` both move it.
#
# Expect one cargo warning about an output filename collision on
# `selfie/index.html`: `selfie` is documented both as a workspace member and as
# test-common's dependency. It is a cargo warning, not a rustdoc one, so
# `-D warnings` does not turn it into an error. Do not chase it.

# Build the docs with a broken intra-doc link as an error
docs-check:
    rm -rf "$(cargo metadata --format-version 1 --no-deps | sed -n 's/.*"target_directory":"\([^"]*\)".*/\1/p')/doc"
    RUSTDOCFLAGS="-D warnings" cargo doc --no-deps --workspace --all-features

# Run the CLI against a throwaway sandbox: `just sandbox-run package list`
[positional-arguments]
sandbox-run *ARGS:
    #!/usr/bin/env bash
    set -euo pipefail

    tmp="${TMPDIR:-/tmp}"
    # A guard, not a hope, and it runs before mktemp rather than on its result.
    # `HOME` is the whole sandbox: a `~` dotfile target and the deploy-state
    # fallback resolve against it, and a relative one resolves against the
    # current directory instead — which is the repository. A relative `TMPDIR` is
    # how that happens, and mktemp passes it straight through, so checking
    # afterwards would leave the stray directory already created here.
    case "$tmp" in
        /*) ;;
        *) echo "refusing to run: TMPDIR '$tmp' is not absolute" >&2; exit 1 ;;
    esac

    root="${tmp%/}/selfie-sandbox"
    home="$(mktemp -d "${root}.XXXXXX")"

    mkdir -p "$home/.config/selfie" "$home/packages"
    cat > "$home/.config/selfie/config.yaml" <<EOF
    environment: sandbox
    package_directory: $home/packages
    EOF
    # Quoted delimiter: nothing here is meant to be expanded, and an unquoted one
    # would silently substitute a `$` someone later adds to the fixture.
    cat > "$home/packages/sandbox-sentinel.yaml" <<'EOF'
    name: sandbox-sentinel
    environments:
      sandbox:
        install: "true"
    EOF

    # Asked of cargo rather than assembled from `target/debug`: `CARGO_TARGET_DIR`
    # and `build.target-dir` both move the output, and a hardcoded path then runs
    # whatever binary an earlier build happened to leave there. A stale binary is
    # worse than a missing one — it runs, and it lies about which code you tested.
    #
    # Captured inside the `if` for the same reason the gate below is: `set -e`
    # aborts on the assignment form, so a build that fails to compile would take
    # the script out before the diagnostic naming the sandbox could be printed.
    if ! bin="$(cargo build -p selfie-cli --message-format=json-render-diagnostics \
        | sed -n 's/.*"executable":"\([^"]*\/selfie\)".*/\1/p' | tail -1)"; then
        echo "refusing to run: cargo could not build selfie-cli" >&2
        exit 1
    fi
    if [ ! -x "$bin" ]; then
        echo "refusing to run: cargo did not report a selfie binary to run" >&2
        exit 1
    fi

    echo "sandbox HOME: $home"
    echo "NOTE: this sandboxes config and filesystem reads only. install, check and"
    echo "      audit commands, and any 'command:' dotfile source, still run for real"
    echo "      on this machine. Use inert commands such as 'true' in fixtures."

    run() {
        env -i PATH="$PATH" HOME="$home" XDG_CONFIG_HOME="$home/.config" \
            SELFIE_CONFIG_DIR="$home/.config/selfie" SHELL=/bin/sh \
            TERM="${TERM:-dumb}" "$bin" "$@"
    }

    # The gate. `package list` prints the package directory it actually read, so
    # this fails whenever the binary reached anything but the sandbox. Matched on
    # the unique mktemp component because the printed path is canonicalized.
    #
    # Captured inside the `if` rather than by a plain assignment because `set -e`
    # aborts on the assignment form, and a missing or unreadable config is exactly
    # what makes `package list` exit non-zero — so the diagnostic below would never
    # reach the reader in the case it was written for.
    if ! gate="$(run --no-color package list 2>&1)"; then
        echo "gate failed: the sandbox run exited non-zero; refusing to run" >&2
        printf '%s\n' "$gate" >&2
        exit 1
    fi

    # `case` rather than a pipe into `grep -q`: under `set -o pipefail`, grep
    # exiting on its first match can SIGPIPE the writer and fail a gate that
    # passed. That already happened here once.
    case "$gate" in
        *"Package directory: "*"$(basename "$home")/packages"*) ;;
        *)
            echo "gate failed: the binary did not read the sandbox config; refusing to run" >&2
            printf '%s\n' "$gate" >&2
            exit 1
            ;;
    esac

    # `"$@"`, never `{{ ARGS }}`. just interpolates the latter as unquoted text,
    # which splits `spec search "search tool"` into two patterns and hands a `;`
    # or a backtick inside an argument straight to this shell — where `HOME` is
    # still the developer's, so the sandbox is bypassed by the very thing it was
    # given to run. `[positional-arguments]` is what puts the arguments in `$@`.
    run "$@"

# Generate and open documentation
docs:
    cargo doc --open

# Check for outdated dependencies
outdated:
    cargo outdated
