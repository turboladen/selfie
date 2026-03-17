# selfie-mcp

MCP (Model Context Protocol) server for selfie. Exposes selfie's package management capabilities to
AI assistants like Claude, Cursor, and other MCP-compatible clients.

## Setup

### Build

```bash
cargo install --path crates/mcp-server
```

Or from the workspace root:

```bash
cargo build -p selfie-mcp --release
```

### Configure in Claude Code

Add to your project's `.mcp.json`:

```json
{
  "mcpServers": {
    "selfie": {
      "command": "selfie-mcp"
    }
  }
}
```

Or if running from source:

```json
{
  "mcpServers": {
    "selfie": {
      "command": "cargo",
      "args": ["run", "-p", "selfie-mcp"]
    }
  }
}
```

### Configure in Claude Desktop

Add to `~/Library/Application Support/Claude/claude_desktop_config.json` (macOS):

```json
{
  "mcpServers": {
    "selfie": {
      "command": "selfie-mcp"
    }
  }
}
```

### Configure in Cursor

Add to Cursor's MCP settings:

```json
{
  "mcpServers": {
    "selfie": {
      "command": "selfie-mcp"
    }
  }
}
```

## Requirements

selfie-mcp reads the same config file as the CLI (`~/.config/selfie/config.yml`). Make sure selfie
is configured before starting the MCP server. See the main [README](../../README.md) for setup
instructions.

## Environment Notes

The MCP server handles two common issues with GUI-launched processes:

- **PATH**: Uses a login shell (`$SHELL -l -c`) to source your profile, so tools in `~/.cargo/bin`,
  homebrew paths, fnm, etc. are available for check/install/audit commands.
- **HOME**: Recovers the home directory from the system password database if `HOME` isn't set,
  ensuring `~` in config paths expands correctly.

## Available Tools

### Single package

| Tool                      | Description                                                    |
| ------------------------- | -------------------------------------------------------------- |
| `selfie_get_package`      | Get detailed package info (environments, dependencies, status) |
| `selfie_check_package`    | Check if a package is installed                                |
| `selfie_validate_package` | Validate a package definition file                             |
| `selfie_audit_package`    | Audit a package's installation sources and detect conflicts    |

### Bulk (current environment)

| Tool                   | Description                                                        |
| ---------------------- | ------------------------------------------------------------------ |
| `selfie_get_all_specs` | Get full definitions for all packages (fast, no commands executed) |
| `selfie_list_packages` | List all packages with install status (runs check commands)        |
| `selfie_audit_all`     | Audit all packages for installation source conflicts               |
| `selfie_validate_all`  | Validate all package definitions (fast, no commands executed)      |
| `selfie_get_config`    | Get current environment, package directory, and settings           |

### Mutating

| Tool                     | Description                                       |
| ------------------------ | ------------------------------------------------- |
| `selfie_install_package` | Install a package using its configured method     |
| `selfie_create_package`  | Create a new package definition file              |
| `selfie_update_package`  | Update a single package's fields                  |
| `selfie_update_packages` | Update multiple packages in a single call (batch) |
| `selfie_remove_package`  | Remove a package definition file                  |

## What AI Assistants Can Do

With these tools, an AI assistant can:

- **Environment health check** -- audit all packages, analyze conflicts, recommend fixes
- **New computer setup** -- list packages, install them sequentially, verify results
- **Package optimization** -- review definitions, suggest better package manager choices
- **Migration assistance** -- compare environments, identify gaps, update package files
- **Package authoring** -- create and update package files from natural language descriptions

## Protocol

selfie-mcp uses stdio transport (stdin/stdout for the MCP JSON-RPC protocol). Diagnostic logs go to
stderr at WARN level. Set `RUST_LOG=selfie_mcp=debug` for verbose logging.

Saved package files are automatically formatted with `dprint fmt` if dprint is installed.
