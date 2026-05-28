# Configuration Reference

llmenv's central configuration is a YAML file at
`~/.config/llmenv/config.yaml`. Project-specific configuration lives in
`.llmenv.yaml` marker files inside each project (see [Project markers](#project-markers)).

The config directory is resolved in this order:

1. `$LLMENV_CONFIG_DIR`, if set.
2. The platform config dir (`~/.config/llmenv` on Linux/macOS).

## Top-level blocks

| Block | Shape | Purpose |
|-------|-------|---------|
| `cache:` | map | Local materialization cache + sync behavior |
| `scope:` | map of lists | Network / host / user scope definitions |
| `capabilities:` | map | Engine-neutral permissions, hooks, plugins (+ `native_*` overrides) |
| `native:` | map (per engine) | Opaque per-engine passthrough for keys no feature models |
| `bundle:` | list | Environment-variable + file bundles |
| `mcp:` | list | MCP server declarations |
| `memory:` | map | llmenv's memory backend topology |
| `marketplace:` | list | Plugin marketplaces (git URL or local path) |
| `plugin-collection:` | list | Named bags of plugins, selected by tag |
| `host:` | map | Host name → reachable address (used by `memory:`) |

All blocks are optional. Scopes (except project), bundles, MCP servers, plugin
collections, and the memory backend all share the same selection model: they
activate when one of their `tags` is in the active tag set.

## `cache:`

```yaml
cache:
  cache_dir: "~/.cache/llmenv"      # where materialized configs are stored
  sync_interval_minutes: 15         # how often `export` pulls config from git
  cache_retention_hours: 168        # GC retention window (default: 7 days)
```

Defaults: `cache_dir` = `~/.cache/llmenv`, `sync_interval_minutes` = `15`,
`cache_retention_hours` = `168`. Set `cache_retention_hours` to `null` to
disable age-based GC.

## `scope:`

Scopes are conditions on the current environment. When a scope matches, its tags
join the active set. Three kinds are declared here; the fourth (`project`) is
discovered from marker files — see [Project markers](#project-markers).

```yaml
scope:
  network:
    - id: office
      match: { gateway_mac: "aa:bb:cc:dd:ee:ff" }
      tags: [office]
  host:
    - id: workstation
      match: { hostname: "work-mbp" }     # case-insensitive
      tags: [workstation]
  user:
    - id: me
      match: { user: "alice" }            # matches $USER
      tags: [me]
```

Each scope has an `id` (used in diagnostics and `LLMENV_ACTIVE_SCOPES`), a
`match` block, and a `tags` list.

- **Network** `match` fields: `gateway_mac`, `ssid`, `cidr`. Only `gateway_mac`
  is evaluated today; `ssid`/`cidr` parse but are ignored.
- **Host** `match` field: `hostname` (compared case-insensitively).
- **User** `match` field: `user` (exact match against `$USER`).

> There is no `scope.project` block. Project scopes come from `.llmenv.yaml`
> markers, not `config.yaml`.

### Precedence

When scopes of different kinds set conflicting scalar capability values, the
order least-to-most specific is **network → host → user → project**. List-shaped
values concatenate and de-duplicate instead of overriding.

## `capabilities:`

Engine-neutral capabilities. The same shape is valid here (global) and inside a
bundle's `bundle.yaml` (bundle-scoped); contributors are merged by value shape.

```yaml
capabilities:
  permissions:
    default_mode: acceptEdits           # acceptEdits | plan | default | bypassPermissions
    allow:
      - { tool: Bash, pattern: "git *" }
      - { tool: Read, paths: ["~/code"] }
    ask:
      - { tool: WebFetch }
    deny:
      - { tool: Bash, pattern: "rm -rf *" }
  hooks:
    - event: SessionStart
      matcher: "*"                       # optional
      handler: { type: command, command: "./hooks/start.sh" }
    - event: PreToolUse
      handler: { type: mcp_tool, tool: "my-server:check" }
  plugins:
    - "superpowers:caveman"              # <marketplace>:<plugin>

  # Per-engine raw overrides — appended verbatim, never translated:
  native_permissions:
    claude_code:
      allow: ["WebFetch(domain:example.com)"]
  native_hooks:
    claude_code: { ... }                 # engine-shaped, opaque to llmenv
  native_plugins:
    claude_code: { ... }
  native_mcp:
    claude_code: { ... }
```

- `permissions.default_mode` is a scalar (resolved by precedence);
  `allow`/`ask`/`deny` are lists (concatenated + deduped).
- A **permission rule** has a `tool` plus either a glob `pattern` or a list of
  `paths`.
- A **hook** has an `event`, optional `matcher`, and a `handler` of type
  `command` (with `command:`) or `mcp_tool` (with `tool:`). Hook command paths
  declared in a bundle are bundle-relative and resolved at materialize time.
- `plugins` are `<marketplace>:<plugin>` strings.
- `native_<feature>` maps are per-engine raw fragments emitted verbatim. They are
  the escape hatch for engine-specific rules with no neutral form. See
  [Engines](engines.md).

## `native:`

A per-engine catch-all for top-level keys that **no modeled feature owns** (e.g.
Claude Code's `alwaysThinkingEnabled`, `outputStyle`). Keyed by engine name;
values are opaque and overlaid onto the engine's config last.

```yaml
native:
  claude_code:
    alwaysThinkingEnabled: true
```

Putting a modeled-feature key (`permissions`, `hooks`) here is a hard error — use
the `native_<feature>` siblings under `capabilities:` instead.

## `bundle:`

A bundle is a named set of environment variables, plus (optionally) a content
directory at `<config_dir>/bundles/<name>/` whose files are merged into the agent
config. A bundle fires when one of its tags is active, or when a project marker
force-enables it via `enable_bundles`.

```yaml
bundle:
  - name: base
    tags: [me]
    vars:
      EDITOR: "code"
  - name: office-tools
    tags: [office]
    vars:
      OFFICE_CI_URL: "https://ci.internal"
```

A vars-only bundle (no content directory) is valid. A bundle's `bundle.yaml`
inside its content directory may declare the same `capabilities:` shape as the
top level.

## `mcp:`

MCP servers selected by tag, rendered into the agent's MCP config. Each is
**stdio** (a launch command) or **remote** (an HTTP/SSE URL).

```yaml
mcp:
  - name: playwright
    tags: [me]
    type: stdio                          # stdio (default) | http | sse
    command: npx
    args: ["-y", "@playwright/mcp@latest"]
    env:
      DISPLAY: ":0"
  - name: weather
    tags: [me]
    type: http
    url: "https://weather.example.com/mcp"
```

| Field | Required | Notes |
|-------|----------|-------|
| `name` | yes | Registration name in the agent's MCP config |
| `tags` | no | Activation tags |
| `type` | no | `stdio` (default), `http`, or `sse` |
| `command` | for stdio | Executable to launch |
| `args` | no | Arguments for `command` |
| `env` | no | Environment for the launched process |
| `url` | for http/sse | Remote endpoint |

See [MCP & Memory](mcp.md) for the full model.

## `memory:`

llmenv's own memory backend (ICM), modeled as a single networked service. One
host runs the daemon; every host connects to it over HTTP. The server host's
address comes from the `host:` table.

```yaml
host:
  fixed:
    addr: "fixed.local"        # IP or resolvable hostname

memory:
  server_host: fixed           # key into the host: table
  port: 7878
  tags: [me]                   # activates the backend (same model as bundles)
  default_topics: ["context-{project}", preferences]
```

| Field | Required | Notes |
|-------|----------|-------|
| `server_host` | yes | Key into `host:` for the daemon host |
| `port` | yes | Port the proxy listens on / clients connect to |
| `tags` | no | Activation tags |
| `default_topics` | no | Documentation only; preserved across round-trips |

See [MCP & Memory](mcp.md) for the topology, security model, and `mcp-proxy`
requirements.

## `marketplace:` and `plugin-collection:`

```yaml
marketplace:
  - name: superpowers
    source: "https://github.com/obra/superpowers.git"   # git URL or local path

plugin-collection:
  - name: dev
    tags: [me]
    plugins:
      - "superpowers:caveman"
```

A marketplace `source` is classified as **git** (cloned into
`<cache_dir>/marketplaces/<name>/`, refreshed by `plugin-sync`) or a **local
path** (used in place). Recognized git schemes: `https://`, `http://`, `ssh://`,
`git://`, `git+ssh://`, plus scp-style `git@host:owner/repo`. Anything starting
with `/`, `~`, `./`, or `../` is a path.

A `plugin-collection` fires by tag like a bundle; its plugins are
`<marketplace>:<plugin>` references. See [Plugins](plugins.md).

## `host:`

A static table mapping host names to reachable addresses, consumed by `memory:`.

```yaml
host:
  fixed:
    addr: "fixed.local"
```

## Project markers

Per-project configuration lives in a `.llmenv.yaml` file at the project root —
**not** in `config.yaml`. llmenv discovers it by walking the current directory
upward to `$HOME`.

```yaml
id: myapp                       # defaults to the folder basename
name: MyApp                     # defaults to the folder basename
description: "Customer API"     # capped at 1024 bytes
tags: [myapp, rust]             # joined into the active tag set
enable_bundles: [base]          # force-enable bundles regardless of their tags
```

All fields are optional; an empty file is valid. Unknown fields are reported by
`llmenv doctor`. Malformed YAML degrades to defaults derived from the folder
basename. See [Concepts → Project markers](concepts.md#project-markers) for
discovery rules.

## YAML gotchas

YAML coerces unquoted scalars. Quote values that could be misread:

- Addresses like `"0.0.0.0:7878"` or anything with `colon + space` — otherwise
  YAML parses a nested mapping.
- Boolean-looking strings (`yes`, `no`, `on`, `off`, `true`, `false`).
- MAC addresses, SSIDs, and URLs.

## Validation

```bash
llmenv status      # active scopes/tags + parse status
llmenv doctor      # full wiring validation (orphan scopes/tags/bundles/plugins)
```

Both report parsing errors and missing required fields. `doctor` additionally
flags orphans — scopes whose tags no contributor consumes, contributors whose
tags no scope emits, a memory `server_host` missing from `host:`, and unknown
fields in project markers.
