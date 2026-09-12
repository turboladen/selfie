---
name: run-selfie
description: Build, run, and drive selfie's two binaries — the selfie-cli terminal tool and the selfie-mcp JSON-RPC/stdio server. Use when asked to run selfie, exercise a CLI command, call an MCP tool, screenshot-equivalent (list/call) MCP tools, or sandbox either binary against a throwaway HOME.
---

selfie is a Rust workspace with two driving adapters over the same `selfie` library: `selfie-cli` (a
terminal CLI, binary name `selfie`) and `selfie-mcp` (a JSON-RPC/stdio MCP server, binary name
`selfie-mcp`). Neither talks to a real machine safely by default — both read `$HOME` for config and
dotfile targets — so both are driven inside a throwaway sandbox `HOME`. The CLI's sandbox is
`just
sandbox-run`; the MCP server's is `.claude/skills/run-selfie/driver.mjs`, a Node harness that
speaks the server's newline-delimited JSON-RPC protocol over its stdin/stdout. All paths below are
relative to the repo root.

## Prerequisites

Nothing beyond the Rust toolchain (`cargo`) already in the repo's `rust-toolchain`/CI, `just`, and
Node (used only for the MCP driver; any modern version works, since the driver uses no
version-specific APIs).

## Build

```bash
cargo build --release          # builds both target/release/selfie and target/release/selfie-mcp
```

`just sandbox-run` (below) builds `selfie-cli` itself in dev profile on each invocation, so a
separate build step isn't required for the CLI path. The MCP driver does **not** build `selfie-mcp`
for you — build it first, or set `SELFIE_MCP_BIN` to point at a debug build.

## Run (agent path): CLI

Use `just sandbox-run <args>` — it mints a fresh throwaway `HOME` per invocation (never reused),
seeds it with a minimal `~/.config/selfie/config.yaml` and one `sandbox-sentinel` package, builds
`selfie-cli`, verifies the binary actually read the sandbox config (not the real one), and then runs
your command:

```bash
just sandbox-run package list
# -> sandbox HOME: /var/folders/.../selfie-sandbox.XXXXXX
# -> ℹ Package list in environment 'sandbox'
# -> ⚠ sandbox-sentinel  No check
# -> Package directory: .../selfie-sandbox.XXXXXX/packages
# -> 1 packages
# -> ✓ Package listing completed with 1 valid package(s) (2/2 steps)

just sandbox-run spec info sandbox-sentinel
just sandbox-run config validate
just sandbox-run package install sandbox-sentinel   # runs the fixture's `install: "true"`
```

One invocation runs exactly one selfie command — state does not carry over between calls, and the
sandbox is discarded (though left on disk under `$TMPDIR` for inspection). To watch state change
across commands, don't call `just sandbox-run` twice; drive the printed sandbox `HOME` directly:

```bash
home=$(just sandbox-run package list 2>&1 | grep '^sandbox HOME: ' | sed 's/^sandbox HOME: //')
bin="$(cargo build -p selfie-cli --message-format=json-render-diagnostics \
    | sed -n 's/.*"executable":"\([^"]*\/selfie\)".*/\1/p' | tail -1)"
env -i PATH="$PATH" HOME="$home" XDG_CONFIG_HOME="$home/.config" \
  SELFIE_CONFIG_DIR="$home/.config/selfie" SHELL=/bin/sh TERM=dumb \
  "$bin" spec info sandbox-sentinel
```

Match only the `sandbox HOME:` line — a looser pattern like
`grep -o '.../selfie-sandbox\.[A-Za-z0-9]*'` also matches `just`'s later "Package directory: ..."
line, and the two paths concatenate into one broken multi-line `HOME`.

Resolve the binary through `cargo build`'s own JSON output, exactly as `just sandbox-run` does,
rather than hardcoding `target/debug/selfie` — a customized `CARGO_TARGET_DIR` or `build.target-dir`
moves the real output elsewhere, and a hardcoded path then silently runs a stale binary or nothing
at all. Do this build in the ambient environment, not the sandboxed one: `cargo run` fails once
`HOME` points at the sandbox, because `rustup` needs the real `HOME` to resolve a default toolchain.

`just sandbox-run` only sandboxes config and filesystem reads. It does **not** sandbox execution:
`install`/`check`/`audit` and any package's `command:` dotfile source still run for real on this
machine. Fixtures must use inert commands (`true`, `echo ...`).

## Run (agent path): MCP server

`selfie-mcp` speaks MCP over stdio: newline-delimited JSON-RPC, no `Content-Length` framing (verify
in `~/.cargo/registry/.../rmcp-3.2.0/src/transport/io.rs` — `stdio()` just wraps
`tokio::io::{stdin, stdout}`). `.claude/skills/run-selfie/driver.mjs` is the harness: it builds a
throwaway sandbox `HOME` (config plus one `sandbox-sentinel` package, like the CLI's — its fixture
additionally sets `check: "true"`, so package list output never shows the CLI path's "No check"
warning), spawns `selfie-mcp` with `HOME`/ `XDG_CONFIG_HOME`/`SELFIE_CONFIG_DIR` pointed at it,
performs the `initialize` handshake, and lets you list or call tools.

```bash
# 1. Build the release binary once (see Build, above).
# 2. Mint a sandbox HOME:
home=$(node .claude/skills/run-selfie/driver.mjs sandbox)
echo "$home"
# -> /var/folders/.../selfie-mcp-sandbox.XXXXXX

# 3. List every tool the server exposes:
node .claude/skills/run-selfie/driver.mjs list-tools "$home"
# -> selfie_apply_dotfiles - Deploy dotfiles to their target locations. ...
# -> selfie_config_get - Get the current selfie configuration ...
# -> ... (23 tools total)

# 4. Call one:
node .claude/skills/run-selfie/driver.mjs call "$home" selfie_package_list '{}'
# -> { "data": [ { "environments": ["sandbox"], "name": "sandbox-sentinel",
# ->     "status": "installed", "type": "package_list_item" } ],
# ->   "result": { "message": "Package listing completed with 1 valid package(s) (2/2 steps)",
# ->     "status": "success" } }

node .claude/skills/run-selfie/driver.mjs call "$home" selfie_config_get '{}'
node .claude/skills/run-selfie/driver.mjs call "$home" selfie_spec_info '{"package":"sandbox-sentinel"}'
node .claude/skills/run-selfie/driver.mjs call "$home" selfie_package_install '{"package":"sandbox-sentinel"}'
```

Like `just sandbox-run`, this only sandboxes config and filesystem reads. `selfie-mcp` uses a real
`ShellCommandRunner`: `selfie_package_install`/`check`/`audit` and any package's `command:` dotfile
source run for real on this machine, and `selfie_apply_dotfiles` can write to any absolute target
path a package spec names. Fixtures must use inert commands (`true`, `echo ...`).

`driver.mjs` looks for `./target/release/selfie-mcp` relative to the current directory (run it from
the repo root, or set `SELFIE_MCP_BIN=/path/to/selfie-mcp`). It spawns one fresh `selfie-mcp`
process per `list-tools`/`call` invocation and tears it down afterward — state does not carry over
between invocations, same as `just sandbox-run`. Reuse the same `home` across calls only to observe
config/package-directory state (as above); reads from a fixed sandbox don't need a fresh `home` each
time, but nothing about the CLI or the MCP server persists cross-process runtime state beyond what's
on disk in that `HOME`.

| driver.mjs command               | what it does                                                                                |
| -------------------------------- | ------------------------------------------------------------------------------------------- |
| `sandbox`                        | mints a throwaway sandbox `HOME` (config + one `sandbox-sentinel` package), prints its path |
| `list-tools <home>`              | connects, initializes, lists every MCP tool with its first description line                 |
| `call <home> <tool> [json-args]` | connects, initializes, calls one tool, prints its `content` blocks                          |

## Run (human path)

`cargo run -p selfie-cli -- <args>` and `cargo run -p selfie-mcp` both read and write **your real**
`$HOME` — do not use them for anything but a deliberate check against your own machine. `selfie-mcp`
blocks waiting for a JSON-RPC client on stdin; it's meant to be launched by an MCP host (Claude
Desktop, etc.), not run interactively.

## Test

```bash
just test-lib          # canonical: cargo test -p selfie (needs test-common's with_mocks feature —
                        # see the root CLAUDE.md if this ever fails to compile)
cargo test -p selfie-cli
cargo test -p selfie-mcp
just check             # fmt + dprint fmt + clippy -D warnings + cargo test, stops at first failure
```

`just test-lib` runs all unit and doc tests, including the two intentional `compile_fail` doctests
on `TargetPath`.

## Gotchas

- **`selfie-mcp` takes no CLI flags at all** — no `--help`, no `--package-directory`. Its only
  config inputs are environment variables (`SELFIE_CONFIG_DIR` beats `XDG_CONFIG_HOME`, which beats
  `HOME`-derived defaults) and the YAML config file they resolve to. Sandbox it by env var, not by
  argument, unlike the CLI.
- **Every MCP tool that names a package uses the JSON field `package`, not `name`.** Calling
  `selfie_package_install` or `selfie_spec_info` with `{"name": "..."}` fails server-side
  deserialization (`missing field 'package'`) rather than a client-side schema check — the error
  only surfaces once the call reaches the server. Read `inputSchema.properties` from `list-tools`
  output, or grep `crates/mcp-server/src/server.rs`, rather than guessing from the tool name.
- **`selfie config` has no `get` subcommand** — only `validate`. To read effective config from the
  CLI, use `selfie config validate` (it prints the resolved settings) or `selfie -v <anything>`, not
  `selfie config get`.
- **The MCP server's stdio framing is one bare JSON value per line — no LSP-style `Content-Length`
  header.** A driver written against a header-framed assumption will hang forever on the first
  `readline`.
- **`just sandbox-run` needs an _absolute_ `$TMPDIR`.** Its own guard refuses a relative one before
  calling `mktemp`, because a relative `HOME` resolves against the current directory (the repo)
  instead of the sandbox. `driver.mjs`'s `sandbox` command uses Node's `os.tmpdir()` directly and
  doesn't carry this guard.

## Troubleshooting

- **`./target/release/selfie-mcp` exits immediately with
  `Error: connection closed: initialize
  request`**: normal when nothing is writing to its stdin —
  it's a server waiting for a client, not a program that runs and finishes. Drive it through
  `driver.mjs`, don't run it bare.
- **A tool call returns `failed to deserialize parameters: missing field '<x>'`**: the JSON arg name
  doesn't match the tool's schema (see Gotchas above) — check `list-tools` output for that tool's
  `inputSchema`.
