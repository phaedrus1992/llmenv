# Getting Started with llmenv

llmenv is a universal scope-aware environment manager for AI coding agents. It provides context-aware configuration, caching, and synchronization for Claude Code and other AI tools.

## Installation

Install the latest release:

```bash
cargo install llmenv
```

Or build from source:

```bash
git clone https://github.com/phaedrus1992/llmenv.git
cd llmenv
cargo build --release
./target/release/llmenv --help
```

## Quick Start

### 1. Initialize Configuration

Create your first llmenv configuration:

```bash
llmenv init
```

This creates `~/.config/llmenv/config.toml` with a template structure. You can customize it to match your environment.

### 2. Set Up Shell Integration

Generate shell hook code for your shell (zsh or bash):

```bash
eval "$(llmenv hook zsh)"
```

Add this line to your shell profile (`.zshrc` or `.bashrc`) to enable automatic scope detection.

### 3. Export Environment Variables

Export variables for the current scope:

```bash
llmenv export
```

Or with a tag filter:

```bash
llmenv export --tag dev
```

### 4. Check Your Setup

Run diagnostics to validate your configuration:

```bash
llmenv doctor
```

This checks:
- Configuration file parsing
- Cache directory writability
- Git remote connectivity

## Example Configuration

See `docs/configuration.md` for complete schema documentation.

A minimal example with network and project scopes:

```toml
[settings]
cache_dir = "~/.cache/llmenv"
sync_interval_minutes = 60

[[scope.network]]
id = "office"
match = { ssid = "OfficeWiFi" }
tags = ["office", "ci-enabled"]

[[scope.project]]
id = "myproject"
match = { marker = ".llmenvrc" }
tags = ["project-local"]

[[bundle]]
name = "base"
tags = []

[bundle.vars]
AGENT = "claude"
```

## Commands Reference

| Command | Purpose |
|---------|---------|
| `llmenv init [--repo URL]` | Initialize configuration |
| `llmenv export [--scope ID] [--tag TAG]` | Export environment variables |
| `llmenv hook {zsh\|bash}` | Generate shell hook code |
| `llmenv status` | Show configuration status |
| `llmenv scope-ls` | List available scopes |
| `llmenv tag-ls` | List available tags |
| `llmenv bundle-ls` | List available bundles |
| `llmenv doctor [--gc]` | Run diagnostics (optionally with GC) |
| `llmenv sync` | Commit and push config to GitHub |

## Next Steps

- Read `docs/configuration.md` for detailed scope and bundle configuration
- Review `docs/icm-topology.md` to understand MCP server integration
- Check GitHub issues for advanced use cases and examples
