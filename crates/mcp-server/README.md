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

### Spec tools (definition operations)

| Tool                       | Description                                  |
| -------------------------- | -------------------------------------------- |
| `selfie_spec_info`         | Get detailed definition info about a package |
| `selfie_spec_list`         | List all specs for the current environment   |
| `selfie_spec_validate`     | Validate a single spec file                  |
| `selfie_spec_validate_all` | Validate all spec files                      |
| `selfie_spec_create`       | Create a new spec file                       |
| `selfie_spec_update`       | Update fields of an existing spec            |
| `selfie_spec_update_batch` | Update multiple specs in a single call       |
| `selfie_spec_remove`       | Remove a spec file                           |

### Package tools (runtime operations)

| Tool                       | Description                            |
| -------------------------- | -------------------------------------- |
| `selfie_package_check`     | Check if a package is installed        |
| `selfie_package_status`    | Check runtime installation status      |
| `selfie_package_list`      | List packages with installation status |
| `selfie_package_install`   | Install a package                      |
| `selfie_package_audit`     | Audit a package's installation sources |
| `selfie_package_audit_all` | Audit all packages for conflicts       |

### Config tools

| Tool                | Description               |
| ------------------- | ------------------------- |
| `selfie_config_get` | Get current configuration |

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
