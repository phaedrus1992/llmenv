# MCP Server Integration (ICM Topology)

llmenv integrates with the Model Context Protocol (MCP) to provide AI agents with access to external tools and services.

## Overview

The **ICM topology** refers to the deployment of an MCP server that bridges Claude Code and other agents to external tools. When enabled, llmenv manages:

1. **Server activation** — Ensures the MCP proxy is running when needed
2. **Scope-aware binding** — Server is only active in relevant scopes
3. **Process lifecycle** — Automatic startup and state management

## Configuration

Add the `[icm]` section to your config:

```toml
[icm]
server_tag = "icm-server"              # Tag that activates the server
server_bind = "127.0.0.1:9092"         # Server address (stdio or TCP)
```

Then add the `server_tag` to scopes where the MCP server should be available:

```toml
[[scope.project]]
id = "myapp"
match = { marker = ".llmenvrc" }
tags = ["myapp", "icm-server"]  # Activates MCP when in this project
```

## How It Works

1. **Scope Evaluation** — When you open a terminal or run `llmenv export`, scopes are evaluated against your current environment (network, host, user, project)
2. **Tag Activation** — Matched scopes contribute their tags; if any includes the `server_tag`, the MCP server is activated
3. **Server Startup** — llmenv ensures the MCP proxy is running on the configured address
4. **Environment Export** — Variables are exported to your shell, including MCP_SERVER and related settings

### Example Flow

```
Current environment:
  - WiFi: "OfficeWiFi" → matches scope.network[0] → tags: ["office", "icm-server"]
  - Project: ".llmenvrc" detected → matches scope.project[0] → tags: ["myapp"]

Active tags: ["office", "myapp", "icm-server"]

Since "icm-server" is active:
  → MCP server starts on 127.0.0.1:9092
  → Agent can connect and use registered tools
```

## Diagnostics

Check MCP server status with doctor:

```bash
llmenv doctor
```

This validates:
- Config parses correctly
- Cache is writable
- Git remote is reachable
- (Future) MCP server is running and responding

With GC:

```bash
llmenv doctor --gc
```

Garbage collects the cache (removes entries older than `cache_retention_hours`).

## Troubleshooting

### Server not starting

Check logs:

```bash
llmenv doctor
# Review warnings about MCP startup
```

Ensure the server address is not already in use:

```bash
netstat -an | grep 9092
```

### Wrong server active

Verify active scopes:

```bash
llmenv export
```

Review your scope configuration:

```bash
llmenv scope-ls
llmenv tag-ls
```

### Scope not matching

Check each scope condition individually against your current environment:

```bash
# Current WiFi network
airport -I | grep SSID

# Current hostname
hostname

# Current user
whoami

# Project markers
ls -la | grep .llmenvrc
```

## Architecture

The MCP server typically runs as a background daemon that:

- Listens on the configured address (TCP or stdio)
- Provides tool definitions (read files, search, execute commands, etc.)
- Routes requests from Claude Code agents
- Enforces scope-aware access control

See your MCP implementation docs for details on tool definitions and security policies.
