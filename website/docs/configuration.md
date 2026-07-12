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
| `lsp:` | list | LSP server declarations (Crush + Claude Code; no-op on engines without an LSP surface) |
| `features:` | map | Feature flags; holds `memory:` (ICM backend topology), `throttle:` (usage throttling), and `upgrade:` (upgrade release track) |
| `session_log:` | map | Session-activity logging: local JSONL file and/or ICM transcript |
| `state:` | map | Durable per-tool state relocation (survives cache folder churn) |
| `marketplace:` | list | Plugin marketplaces (git URL or local path) |
| `plugin-collection:` | list | Named bags of plugins, selected by tag |
| `host:` | map | Host name → reachable address (used by `features.memory:`) |

All blocks are optional. Scopes (except project), bundles, MCP servers, plugin
collections, and the memory backend all share the same selection model: they
activate when one of their `tags` is in the active tag set.

## `cache:`

```yaml
cache:
  cache_dir: "~/.cache/llmenv"      # where materialized configs are stored
  sync_interval_minutes: 15         # how often `export` pulls config from git
  cache_retention_hours: 168        # GC retention window (default: 7 days)
  hashing: normal                   # loose | normal | strict (default: normal)
```

Defaults: `cache_dir` = `~/.cache/llmenv`, `sync_interval_minutes` = `15`,
`cache_retention_hours` = `168`. Set `cache_retention_hours` to `null` to
disable age-based GC.

### `hashing` — how materialized folders are named

A single dial with three positions. The folder path is:

| Mode | Folder layout | When to use |
|------|---------------|-------------|
| `loose` | `<adapter>/<shape>/` | Maximum cache reuse across upgrades |
| `normal` (default) | `<adapter>/<version_mm>/<shape>/` | Balanced: stable within a release, churns on minor bumps |
| `strict` | `<adapter>/<VERSION_TAG>-<content_hash>/` | Maximum isolation; new folder on any input change |

`shape` is a 12-hex SHA-256 over the active tags ∪ enabled bundles. Config edits
always **re-render into the same folder** in `loose` and `normal` modes, so a
running agent only loads them when you relaunch it (`llmenv check-stale` nudges
you on the next `SessionStart`). The folder is the agent's live config dir for the
whole session, so in-session state llmenv doesn't own — Claude's runtime files,
third-party plugin state — is preserved across re-renders. `settings.json` is
merged rather than clobbered, so a plugin's self-registered hooks survive.

Each materialized folder carries a `.llmenv-manifest.json` dotfile (the content
hash + the files llmenv owns). It is what `check-stale`/`doctor` use to detect
drift and what re-renders use to clean up files llmenv no longer renders without
touching foreign state.

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

A bundle is a named content set that fires when one of its tags is active, or
when a project marker force-enables it via `enable_bundles` — unless a project
marker force-disables it via `disable_bundles`, which always wins. Its content
directory lives at `<config_dir>/bundles/<name>/` and its files are merged
into the agent config. A bundle's `bundle.yaml` inside its content directory
may declare `env:` and other `capabilities:` fields.

```yaml
bundle:
  - name: base
    when: [me]
  - name: office-tools
    when: [office]
```

A bundle entry with only `name` and `when` (no content directory) is valid and
participates in tag matching. To inject environment variables, declare them in the
bundle's `bundle.yaml` under `capabilities.env`.

## `mcp:`

MCP servers selected by tag, rendered into the agent's MCP config. Each is
**stdio** (a launch command) or **remote** (an HTTP/SSE URL).

```yaml
mcp:
  - name: playwright
    when: [me]
    type: stdio                          # stdio (default) | http | sse
    command: npx
    args: ["-y", "@playwright/mcp@latest"]
    env:
      DISPLAY: ":0"
  - name: weather
    when: [me]
    type: http
    url: "https://weather.example.com/mcp"
```

| Field | Required | Notes |
|-------|----------|-------|
| `name` | yes | Registration name in the agent's MCP config |
| `when` | no | Activation tags |
| `type` | no | `stdio` (default), `http`, or `sse` |
| `command` | for stdio | Executable to launch |
| `args` | no | Arguments for `command` |
| `env` | no | Environment for the launched process |
| `url` | for http/sse | Remote endpoint |

See [MCP & Memory](mcp.md) for the full model.

## `lsp:`

Language servers selected by tag, rendered into the agent's LSP config. Only
engines whose adapter reports `supports_lsp() == true` render these — today
that's Crush and Claude Code; other engines silently ignore `lsp:` entries,
so it's safe to declare in a bundle shared across engines.

```yaml
lsp:
  - name: rust-analyzer
    when: [me]
    command: rust-analyzer
    filetypes: ["rust"]           # Crush
    root_markers: ["Cargo.toml"]  # Crush
    extension_to_language:        # Claude Code
      ".rs": rust
    init_options:
      check:
        command: clippy
    timeout: 30
```

| Field | Required | Notes |
|-------|----------|-------|
| `name` | yes | Registration name in the agent's LSP config |
| `when` | no | Activation tags |
| `command` | yes | Executable to launch |
| `args` | no | Arguments for `command` |
| `env` | no | Environment for the launched process |
| `disabled` | no | Excludes the server from every engine when `true` |
| `filetypes` | no | Crush only: language identifiers the server handles (e.g. `["rust"]`) |
| `root_markers` | no | Crush only: filenames/patterns that anchor the workspace root |
| `extension_to_language` | no | Claude Code only (**required** there): file extension → language id, e.g. `{".rs": "rust"}` |
| `init_options` | no | Opaque data forwarded verbatim as the LSP `initialize` handshake options |
| `timeout` | no | Crush only: per-server request timeout in seconds |

Each engine only understands the fields it needs — Crush ignores
`extension_to_language`, and Claude Code ignores `filetypes`/`root_markers`/`timeout`
(it has no equivalents: a single `workspaceFolder` path and a startup-only timeout,
not a request timeout). A server with no `extension_to_language` is skipped (with a
warning) when rendering for Claude Code, since Claude Code's `lspServers` schema
requires it and `filetypes` language ids don't reliably convert to file extensions.

## `features:`

Feature flags. Holds `memory:` (llmenv's ICM memory backend), `throttle:`
(usage throttling), and `upgrade:` (upgrade release track). Additional feature
flags may be nested here in future versions.

### `features.memory:`

llmenv's own memory backend (ICM). A list of tag-scoped topology entries: each
declares one host that runs the daemon and the tag set that activates it (same
model as bundles and MCP servers). At most one entry may be active per scope —
the resolver errors if two entries' tags match simultaneously. Zero active
entries means memory is disabled for that scope.

```yaml
host:
  home-server:
    addr: "home-server.local"  # IP or resolvable hostname
  work-server:
    addr: "work-server.local"

features:
  memory:
    - server_host: home-server   # key into the host: table
      port: 9092
      when: [home]               # activates the backend (same model as bundles)
      default_topics: ["context-{project}", preferences]
    - server_host: work-server
      port: 9092
      when: [work]
```

| Field | Required | Notes |
|-------|----------|-------|
| `server_host` | yes | Key into `host:` for the daemon host |
| `port` | yes | Port the proxy listens on / clients connect to |
| `listen_host` | no | IP address to listen on (`127.0.0.1` for loopback, `0.0.0.0` for all interfaces); default `127.0.0.1` |
| `when` | no | Activation tags |
| `default_topics` | no | Documentation only; preserved across round-trips |

See [MCP & Memory](mcp.md) for the topology, security model, and `mcp-proxy`
requirements.

### `features.throttle:`

Usage throttling for an LLM backend. A list of tag-scoped entries (same
selection model as `memory:` — at most one active per scope, resolver errors on
two simultaneously active). When an entry is active, llmenv injects `PreToolUse`
and `UserPromptSubmit` hooks that poll the backend's request budget and sleep a
capped, adaptive delay as the budget runs low — keeping the session under the
backend's rate limit instead of hitting a hard 429. Each entry names a
`backend` that supplies usage data; `umans` is the only backend today.

```yaml
features:
  throttle:
    - backend: umans                  # backend that supplies usage data
      when: [host-personal-laptop]    # activation tags (same model as bundles)
      cache_ttl: 30                   # seconds a polled snapshot is cached
      max_wait: 300                   # hard cap (seconds) on any single delay
      soft_threshold: 20              # remaining-request level where delays begin
```

| Field | Required | Notes |
|-------|----------|-------|
| `backend` | yes | Usage-data backend; currently only `umans` |
| `when` | no | Activation tags (an entry with none never activates) |
| `cache_ttl` | no | Seconds a polled usage snapshot is cached; default `30` |
| `max_wait` | no | Hard cap in seconds on any single delay; default `300` |
| `soft_threshold` | no | Remaining-request level where adaptive delays start; default `20` |

The delay is always capped at `max_wait`; the throttle never blocks for a
backend-reported penalty window that could be hours long. The `umans` backend
reads `~/.umans/config.json` for its endpoint and token. Throttling is
fail-soft: any error (missing config, network failure) skips the delay rather
than blocking the session.

### `features.upgrade:`

Controls which release track `llmenv upgrade` uses. The CLI `--track` flag
overrides this on a per-run basis.

```yaml
features:
  upgrade:
    track: beta    # "release" (default) or "beta"
```

| Field | Required | Notes |
|-------|----------|-------|
| `track` | no | `"release"` (default) or `"beta"`. `release` uses the GitHub latest-stable endpoint; `beta` uses the first non-draft release from the recent list. |

## `session_log:`

llmenv records session activity — lifecycle events, the active scope, and
(optionally) every prompt/tool call — into a single event stream that fans out
to two **independent** sinks: a local JSONL file and ICM's transcript store,
reached over the **ICM MCP** (never the `icm` CLI, so this works even when the
machine running llmenv isn't the primary ICM host). Either sink can be on
without the other; an unreachable ICM backend never blocks the file sink, and
vice versa.

```yaml
session_log:
  transcript: true    # ICM transcript sink (default ON)
  file: false          # local JSONL file sink
  verbose: false        # also capture per-hook prompts and tool use
  # path: "~/.local/state/llmenv/session-log.jsonl"  # override the file path
  # max_content_bytes: 16384                          # cap per-event content size
```

| Field | Required | Notes |
|-------|----------|-------|
| `transcript` | no | Record into ICM's transcript store via the ICM MCP; default `true` |
| `file` | no | Mirror the same event stream to a local JSONL file; default `false` |
| `verbose` | no | Also capture `UserPromptSubmit`/`PreToolUse`/`PostToolUse`/`Notification`/`Stop`/`SubagentStop`/`PreCompact` events, not just the lifecycle + scope header; default `false` |
| `path` | no | Override the file sink's path; default `<state_dir>/session-log.jsonl` |
| `max_content_bytes` | no | Cap each event's `content` field to this many bytes before it's written/recorded; default `16384` |

Omitting the `session_log:` block entirely is equivalent to `transcript: true`
(everything else off) — ICM transcript logging is **on by default**. To turn
logging off entirely, set both flags to `false`:

```yaml
session_log:
  transcript: false
  file: false
```

> Breaking change in 3.0: `session_log:` used to be a bare path string (the
> file sink only). That form is now rejected with a migration hint — wrap the
> path in `path:` under the new table shape.

### What gets logged

Two layers, gated by `verbose`:

- **Baseline** (always, when a sink is enabled): one `lifecycle_start` event at
  session start, one `scope` event carrying the active tags/bundles/project,
  and one `lifecycle_end` event at session end.
- **Verbose** (`verbose: true`): every prompt submission, tool call (before and
  after), notification, stop, subagent stop, and pre-compact event, each
  tagged with its role and (for tool events) the tool name.

> **Privacy note:** `verbose: true` captures the *raw* text of every prompt
> you submit and every tool call's input/output — including any secrets,
> credentials, or personal data that text happens to contain. That content is
> written to disk (`file: true`) and/or sent to ICM (`transcript: true`)
> unredacted, capped only by `max_content_bytes` (default 16 KiB, not a
> sensitivity filter). Treat a `session-log.jsonl` with `verbose: true` enabled
> the same way you'd treat shell history that might contain pasted secrets.

### Finding a session later

The scope-header event embeds the same `llmenv-tag:<tag>` / `llmenv-bundle:<bundle>`
tokens the memory-recall hooks use, so a transcript is discoverable the same
way stored memory is. From the ICM MCP:

```
icm_transcript_search { query: "llmenv-tag:rust" }                      # sessions scoped to the rust tag
icm_transcript_search { query: "llmenv-bundle:base" }                   # sessions where the base bundle fired
icm_transcript_search { query: "llmenv session", project: "my-project" } # sessions for one project
icm_transcript_show { session_id: "..." }                                # full transcript for one session
icm_transcript_stats {}                                                  # global session/message counts
```

`icm_transcript_search` matches message **content** only (ICM's FTS index
doesn't cover session metadata), which is why the scope header embeds the
tokens directly in its content rather than only in structured metadata. The
structured metadata (tags/bundles/project/cwd/adapter/llmenv version) is still
attached to the session for exact inspection via `icm_transcript_show`.

## `state:`

Durable per-tool state relocation. The materialized cache folder is renamed on
every version or config change, so tool state written under `CLAUDE_CONFIG_DIR`
is lost on each churn. llmenv always exports `LLMENV_STATE_DIR` pointing at a
stable sibling directory (no content hash; never garbage-collected). Each entry
under `state.tools` additionally emits one env var pointing a specific tool's
state into a per-tool subdirectory of that stable dir.

```yaml
state:
  tools:
    - env: CONTEXT_MODE_DATA_DIR   # var the tool reads to locate its state
      subdir: context-mode          # → $LLMENV_STATE_DIR/context-mode
```

| Field | Required | Notes |
|-------|----------|-------|
| `env` | yes | Env var the tool honors (e.g. `CONTEXT_MODE_DATA_DIR`) |
| `subdir` | yes | Single path component under `$LLMENV_STATE_DIR` (no separators) |

`env` names must be `[A-Z][A-Z0-9_]*`. A handful of system-reserved names
(`HOME`, `PATH`, `USER`, etc.) are rejected.

## `marketplace:` and `plugin-collection:`

```yaml
marketplace:
  - name: superpowers
    source: "https://github.com/obra/superpowers.git"   # git URL or local path

plugin-collection:
  - name: dev
    when: [me]
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
disable_bundles: [yaks]         # force-disable bundles even if a scope's tag enables them
```

All fields are optional; an empty file is valid. `disable_bundles` always wins
over any scope's tag-firing or `enable_bundles` for the named bundle,
including this same marker's own `enable_bundles` if it lists the same
name — see [Concepts → Precedence](concepts.md#precedence). Unknown fields
are reported by `llmenv doctor`, which also flags a `disable_bundles`/
`enable_bundles` entry referencing an unknown bundle or the same bundle
appearing in both lists. Malformed YAML degrades to defaults derived from the
folder basename. See [Concepts → Project markers](concepts.md#project-markers)
for discovery rules.

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
