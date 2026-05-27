# Configuration Reference

llmenv configuration is defined in YAML format at `~/.config/llmenv/config.yaml`.

## Top-Level Sections

- `cache:` — Global behavior (cache directory, sync intervals)
- `scope:` — Scope definitions (network, host, user, project)
- `bundle:` — Environment variable bundles
- `mcp:` — MCP server declarations (optional)
- `memory:` — llmenv's memory backend topology (optional)
- `host:` — Host address table, used by the memory backend (optional)

## Cache

```yaml
cache:
  cache_dir: "~/.cache/llmenv"          # Where to store cached manifests
  sync_interval_minutes: 60             # How often to pull config from GitHub
  cache_retention_hours: 168            # GC retention (default: 7 days)
```

### Defaults

- `cache_dir`: `~/.cache/llmenv`
- `sync_interval_minutes`: `15`
- `cache_retention_hours`: `168` (7 days)

## Scopes

Scopes are conditions that match your current environment. When matched, their tags become active.

### Network Scope

Match based on WiFi network:

```yaml
scope:
  network:
    - id: office
      match: { ssid: "OfficeWiFi" }
      tags: [office, office-ci]
```

### Host Scope

Match based on hostname:

```yaml
scope:
  host:
    - id: workstation
      match: { hostname: "my-work-machine" }
      tags: [workstation]
```

### User Scope

Match based on OS user:

```yaml
scope:
  user:
    - id: personal
      match: { user: "alice" }
      tags: [personal]
```

### Project Scope

Match based on project markers (files in current directory):

```yaml
scope:
  project:
    - id: myapp
      match: { marker: ".llmenvrc" }
      tags: [myapp, myapp-dev]
```

## Bundles

Bundles define sets of environment variables. When a bundle's tag matches the current scope, its variables are exported.

```yaml
bundle:
  - name: base
    tags: [base]
    vars:
      AGENT: "claude"
      AGENT_VERSION: "1.0.0"

  - name: office-tools
    tags: [office]  # Only active when in office network
    vars:
      OFFICE_CI_URL: "https://ci.internal"
      PROXY_HOST: "proxy.office"

  - name: project-config
    tags: [myapp]
    vars:
      PROJECT_ROOT: "/Users/alice/code/myapp"
```

## MCP Servers (Optional)

MCP servers are declared under `mcp:` and attached to scopes via `tags` — the
same selection model as bundles. A server is rendered into the agent config
(for Claude Code, `mcp.json`) when any of its tags is active.

A static server is **stdio** (a launch command) or **remote** (an HTTP/SSE URL):

```yaml
mcp:
  - name: playwright
    tags: [base]
    type: stdio                        # stdio (default) | http | sse
    command: npx
    args: ["-y", "@playwright/mcp@latest"]

  - name: weather
    tags: [base]
    type: http
    url: "https://weather.example.com/mcp"
```

### Memory backend (`memory:`)

`memory:` configures llmenv's own memory backend as a single network service:
one host runs the daemon, and every agent — on every host — connects to it over
the network. The server host's address is looked up in the top-level `host:`
table.

```yaml
host:
  fixed:
    addr: "fixed.local"

memory:
  server_host: fixed                   # key into the `host:` table
  port: 7878
  tags: [base]                         # activates the backend, like a bundle
  default_topics: ["context-{project}", preferences]
```

On the host matching `server_host`, llmenv launches a local `mcp-proxy` bound to
`0.0.0.0:<port>` that bridges the stdio daemon onto the network. Every agent —
including the one on the server host — is configured with a remote client at
`http://<addr>:<port>`. See [icm-topology.md](icm-topology.md) for details.

## YAML Gotchas

YAML coerces unquoted scalars, which can surprise you. Quote values that could be
misread:

- Values containing a colon followed by whitespace, or addresses like
  `"0.0.0.0:7878"`, should be quoted so YAML doesn't try to parse them as a
  nested mapping.
- Values that look like booleans (`yes`, `no`, `on`, `off`, `true`, `false`) but
  should stay strings — quote them.
- MAC addresses, SSIDs, and URLs should always be quoted.

## Complete Example

```yaml
cache:
  cache_dir: "~/.cache/llmenv"
  sync_interval_minutes: 60
  cache_retention_hours: 168

scope:
  network:
    - id: office
      match: { ssid: "OfficeWiFi" }
      tags: [office, office-ci]
  project:
    - id: llmenv
      match: { marker: ".llmenvrc" }
      tags: [llmenv-dev]

bundle:
  - name: base
    tags: [base]
    vars:
      AGENT: "claude"
      EDITOR: "code"
  - name: office-config
    tags: [office]
    vars:
      OFFICE_CI_URL: "https://ci.internal"

host:
  fixed:
    addr: "fixed.local"

memory:
  server_host: fixed
  port: 7878
  tags: [base]
```

## Validation

Validate your configuration:

```bash
llmenv status
llmenv doctor
```

Both commands will report any parsing errors or missing required fields.
