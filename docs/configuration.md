# Configuration Reference

llmenv configuration is defined in TOML format at `~/.config/llmenv/config.toml`.

## Top-Level Sections

- `[settings]` — Global behavior (cache, sync intervals)
- `[scope.*]` — Scope definitions (network, host, user, project)
- `[[bundle]]` — Environment variable bundles
- `[icm]` — MCP server integration (optional)

## Settings

```toml
[settings]
cache_dir = "~/.cache/llmenv"          # Where to store cached manifests
sync_interval_minutes = 60              # How often to pull config from GitHub
cache_retention_hours = 168             # GC retention (default: 7 days)
```

### Defaults

- `cache_dir`: `~/.cache/llmenv`
- `sync_interval_minutes`: `15`
- `cache_retention_hours`: `168` (7 days)

## Scopes

Scopes are conditions that match your current environment. When matched, their tags become active.

### Network Scope

Match based on WiFi network:

```toml
[[scope.network]]
id = "office"
match = { ssid = "OfficeWiFi" }
tags = ["office", "office-ci"]
```

### Host Scope

Match based on hostname:

```toml
[[scope.host]]
id = "workstation"
match = { hostname = "my-work-machine" }
tags = ["workstation"]
```

### User Scope

Match based on OS user:

```toml
[[scope.user]]
id = "personal"
match = { user = "alice" }
tags = ["personal"]
```

### Project Scope

Match based on project markers (files in current directory):

```toml
[[scope.project]]
id = "myapp"
match = { marker = ".llmenvrc" }
tags = ["myapp", "myapp-dev"]
```

## Bundles

Bundles define sets of environment variables. When a bundle's tag matches the current scope, its variables are exported.

```toml
[[bundle]]
name = "base"
tags = []  # Always active

[bundle.vars]
AGENT = "claude"
AGENT_VERSION = "1.0.0"

[[bundle]]
name = "office-tools"
tags = ["office"]  # Only active when in office network

[bundle.vars]
OFFICE_CI_URL = "https://ci.internal"
PROXY_HOST = "proxy.office"

[[bundle]]
name = "project-config"
tags = ["myapp"]

[bundle.vars]
PROJECT_ROOT = "/Users/alice/code/myapp"
```

## MCP Server Integration (Optional)

Configure MCP proxy for AI agent access to tools:

```toml
[icm]
server_tag = "icm-server"           # Tag that activates server
server_bind = "127.0.0.1:9092"      # Server address
```

The MCP proxy ensures model context protocol servers are available to Claude Code and other agents when the `server_tag` is active in your current scope.

## Complete Example

```toml
[settings]
cache_dir = "~/.cache/llmenv"
sync_interval_minutes = 60
cache_retention_hours = 168

[[scope.network]]
id = "office"
match = { ssid = "OfficeWiFi" }
tags = ["office", "office-ci"]

[[scope.project]]
id = "llmenv"
match = { marker = ".llmenvrc" }
tags = ["llmenv-dev"]

[[bundle]]
name = "base"
tags = []

[bundle.vars]
AGENT = "claude"
EDITOR = "code"

[[bundle]]
name = "office-config"
tags = ["office"]

[bundle.vars]
OFFICE_CI_URL = "https://ci.internal"

[icm]
server_tag = "icm-server"
server_bind = "127.0.0.1:9092"
```

## Validation

Validate your configuration:

```bash
llmenv status
llmenv doctor
```

Both commands will report any parsing errors or missing required fields.
