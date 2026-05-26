# llmenv

A universal scope-aware environment manager for AI coding agents.

**llmenv** is like `direnv` for Claude Code and other AI tools. It automatically applies context-specific configuration based on your current network, host, user, or project.

## Features

- **Scope-aware config** — Different settings for office, home, projects, etc.
- **Tag-based bundles** — Organize environment variables, rules, and plugins
- **Shell integration** — Automatic scope detection via shell hooks
- **Cache & sync** — Local caching with optional GitHub synchronization
- **MCP integration** — Scope-aware access to external tools via Model Context Protocol
- **Diagnostics** — Built-in `llme doctor` for troubleshooting

## Quick Start

### 1. Install

```bash
cargo install llmenv
```

### 2. Initialize

```bash
llme init
```

### 3. Configure your environment

Edit `~/.config/llmenv/config.toml` to add your scopes and bundles.

### 4. Activate shell integration

Add to your `.zshrc` or `.bashrc`:

```bash
eval "$(llme hook zsh)"
```

### 5. Verify setup

```bash
llme doctor
```

## Example

```toml
[settings]
cache_dir = "~/.cache/llmenv"

[[scope.network]]
id = "office"
match = { ssid = "OfficeWiFi" }
tags = ["office"]

[[scope.project]]
id = "myapp"
match = { marker = ".llmerc" }
tags = ["myapp-dev"]

[[bundle]]
name = "base"
tags = []

[bundle.vars]
AGENT = "claude"

[[bundle]]
name = "office-config"
tags = ["office"]

[bundle.vars]
OFFICE_PROXY = "proxy.internal"
```

As you move to the office WiFi or into the `myapp` project, the corresponding bundles automatically activate.

## Documentation

- [Getting Started](docs/getting-started.md) — Installation and basic usage
- [Configuration Reference](docs/configuration.md) — Complete schema and examples
- [MCP Integration](docs/icm-topology.md) — Model Context Protocol setup

## Supported Agents

- Claude Code
- Codex
- Other tools via MCP integration

## How It Works

llmenv evaluates **scopes** (network, host, user, project) against your current environment and activates matching **bundles** of configuration:

```
Current environment → Match scopes → Collect tags → Export variables
                                                  → Activate MCP server (if configured)
```

When you run `llme export`, it returns shell commands to set up your environment. The shell hook runs this automatically on every prompt, keeping your config in sync as you move between projects and networks.

## Commands

| Command | Purpose |
|---------|---------|
| `llme init` | Create configuration |
| `llme export` | Export environment variables |
| `llme hook {zsh\|bash}` | Generate shell integration code |
| `llme doctor [--gc]` | Diagnostics and cache cleanup |
| `llme scope-ls` | List scopes |
| `llme bundle-ls` | List bundles |
| `llme sync` | Push config to GitHub |

## Version

Version 1.0.0 — Ready for production use.

