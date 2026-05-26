# llmenv

[![CI](https://github.com/phaedrus1992/llmenv/actions/workflows/ci.yml/badge.svg)](https://github.com/phaedrus1992/llmenv/actions/workflows/ci.yml)
[![coverage](https://github.com/phaedrus1992/llmenv/actions/workflows/coverage.yml/badge.svg)](https://github.com/phaedrus1992/llmenv/actions/workflows/coverage.yml)

A universal scope-aware environment manager for AI coding agents.

**llmenv** is like `direnv` for Claude Code and other AI tools. It automatically applies context-specific configuration based on your current network, host, user, or project.

## Features

- **Scope-aware config** — Different settings for office, home, projects, etc.
- **Tag-based bundles** — Organize environment variables, rules, and plugins
- **Shell integration** — Automatic scope detection via shell hooks
- **Cache & sync** — Local caching with optional GitHub synchronization
- **MCP integration** — Scope-aware access to external tools via Model Context Protocol
- **Diagnostics** — Built-in `llmenv doctor` for troubleshooting

## Quick Start

### 1. Install

```bash
cargo install llmenv
```

### 2. Initialize

```bash
llmenv init
```

### 3. Configure your environment

Edit `~/.config/llmenv/config.toml` to add your scopes and bundles.

### 4. Activate shell integration

Add to your `.zshrc` or `.bashrc`:

```bash
eval "$(llmenv hook zsh)"
```

### 5. Verify setup

```bash
llmenv doctor
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
match = { marker = ".llmenvrc" }
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

## Development

After cloning, install the git hooks so the same checks CI enforces run locally:

```bash
prek install --hook-type pre-commit --hook-type pre-push
```

- `cargo fmt --check` runs on every commit (fast).
- `cargo clippy -D warnings` and `cargo test` run on push (slower).

Install `prek` from <https://github.com/j178/prek> if you don't have it.

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

When you run `llmenv export`, it returns shell commands to set up your environment. The shell hook runs this automatically on every prompt, keeping your config in sync as you move between projects and networks.

## Commands

| Command | Purpose |
|---------|---------|
| `llmenv init` | Create configuration |
| `llmenv export` | Export environment variables |
| `llmenv hook {zsh\|bash}` | Generate shell integration code |
| `llmenv doctor [--gc]` | Diagnostics and cache cleanup |
| `llmenv scope-ls` | List scopes |
| `llmenv bundle-ls` | List bundles |
| `llmenv sync` | Push config to GitHub |

## Version

Version 1.0.0 — Ready for production use.

