<!-- markdownlint-disable MD013 -->

# Configuration Reference

llmenv's central configuration is a YAML file at
`~/.config/llmenv/config.yaml`. Project-specific configuration lives in
`.llmenv.yaml` marker files inside each project (see [Project markers](#project-markers)).

The config directory is resolved in this order:

1. `$LLMENV_CONFIG_DIR`, if set.
2. The platform config dir (`~/.config/llmenv` on Linux/macOS).

## Top-level blocks

| Block | Shape | Purpose |
| ------- | ------- | --------- |
| `cache:` | map | Local materialization cache + sync behavior |
| `scope:` | map of lists | Network / host / user scope definitions |
| `capabilities:` | map | Engine-neutral permissions, hooks, plugins (+ `native_*` overrides) |
| `native:` | map (per engine) | Opaque per-engine passthrough for keys no feature models |
| `bundle:` | list | Environment-variable + file bundles |
| `mcp:` | list | MCP server declarations |
| `lsp:` | list | LSP server declarations (Crush + Claude Code; no-op on engines without an LSP surface) |
| `features:` | map | Feature flags; holds `memory:` (ICM backend topology), `throttle:` (usage throttling), and `upgrade:` (upgrade release track) |
| `session_log:` | map | Session-activity logging: local JSONL file and/or ICM transcript |
| `statusline:` | map | Widget layout, formatting, and colour config for `llmenv statusline` |
| `state:` | map | Durable per-tool state relocation (survives cache folder churn) |
| `marketplace:` | list | Plugin marketplaces (git URL or local path) |
| `plugin-collection:` | list | Named bags of plugins, selected by tag |
| `skills:` | list | First-class skill declarations, selected by tag (same model as `lsp:`) |
| `host:` | map | Host name → reachable address (used by `features.memory:`) |
| `init:` | map | Settings seeded into new materialized folders by `llmenv init` |
| `disabled_engines` | list | Engine IDs to skip during materialization (#562) |

All blocks are optional. Scopes (except project), bundles, MCP servers, plugin
collections, skills, LSP servers, and the memory backend all share the same
selection model: they activate when one of their `tags` is in the active tag
set.

## `disabled_engines`

A list of engine IDs whose adapters are skipped during materialization, even
when the engine's binary is on `PATH` (#562). Uses the underscore form (e.g.
`claude_code`, `crush`, `opencode`), matching the `native.<engine>` and
`--engine` flag convention.

```yaml
disabled_engines:
  - crush            # skip Crush materialization even when `crush` is on PATH
  - opencode         # skip opencode materialization even when `opencode` is on PATH
```

## `cache:`

```yaml
cache:
  cache_dir: "~/.cache/llmenv"      # where materialized configs are stored
  sync_interval_minutes: 15         # how often `export` pulls config from git
  cache_retention_hours: 168        # GC retention window (default: 7 days)
  remote_sync: true                 # enable remote git ops (fetch, pull, push)
  hashing: normal                   # loose | normal | strict (default: normal)
```

Defaults: `cache_dir` = `~/.cache/llmenv`, `sync_interval_minutes` = `15`,
`cache_retention_hours` = `168`, `remote_sync` = `true`. Set
`cache_retention_hours` to `null` to disable age-based GC.

### `remote_sync` — toggle background remote git operations

When enabled (default), llmenv fetches and pulls config from git on `export`.

Set to `false` to disable *background* remote git operations (the throttled
pull that runs during `llmenv export`). Manual commands like `llmenv sync` and
`llmenv plugin-sync` are unaffected — they always perform remote operations
regardless of this setting.

Useful when your SSH credential helper (e.g. 1Password's SSH agent) is locked
and an SSH askpass prompt would hang terminal-based git operations during
startup:

### `hashing` — how materialized folders are named

A single dial with three positions. The folder path is:

| Mode | Folder layout | When to use |
| ------ | --------------- | ------------- |
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
join the active set. Four kinds are declared here; the fifth (`project`) is
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
  content:
    - id: rust-project
      match: { glob: "*.rs", depth: 2 }    # depth omitted = unbounded
      tags: [lang-rust]
```

Each scope has an `id` (used in diagnostics and `LLMENV_ACTIVE_SCOPES`), a
`match` block, and a `tags` list.

- **Network** `match` fields: `gateway_mac`, `ssid`, `cidr`. Only `gateway_mac`
  is evaluated today; `ssid`/`cidr` parse but are ignored.
- **Host** `match` field: `hostname` (compared case-insensitively).
- **User** `match` field: `user` (exact match against `$USER`).
- **Content** `match` fields: `glob` (matched against paths relative to the
  working directory) and `depth` (optional; caps how many directories deep
  the search descends — omit for an unbounded search). Unlike `network`/
  `host`/`user`, which check environment facts (network gateway, hostname,
  `$USER`), `content` scopes activate based on what files exist in the
  working tree — e.g. gating a bundle's hooks to only fire when `*.rs` files
  are present. All active content scopes are evaluated together in a single
  directory walk, so adding more content scopes doesn't multiply the cost of
  the walk.

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
| ------- | ---------- | ------- |
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
| ------- | ---------- | ------- |
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

Feature flags. Holds `memory:` (llmenv's ICM memory backend), `codebase_memory:`
(codebase-memory-mcp integration), `throttle:` (usage throttling), and
`upgrade:` (upgrade release track). Additional feature flags may be nested
here in future versions.

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
| ------- | ---------- | ------- |
| `server_host` | yes | Key into `host:` for the daemon host |
| `port` | yes | Port the proxy listens on / clients connect to |
| `listen_host` | no | IP address to listen on (`127.0.0.1` for loopback, `0.0.0.0` for all interfaces); default `127.0.0.1` |
| `when` | no | Activation tags |
| `default_topics` | no | Documentation only; preserved across round-trips |

See [MCP & Memory](mcp.md) for the topology, security model, and `mcp-proxy`
requirements.

### `features.codebase_memory:`

First-class integration for
[codebase-memory-mcp](https://github.com/DeusData/codebase-memory-mcp), a
local code-intelligence MCP server. A list of tag-scoped entries: each
declares the tag set that activates a local instance for a project. Unlike
`memory:`, this always resolves to a **local stdio process** — codebase-
memory-mcp has no remote/network-serve mode — so there's no `server_host` or
`port` to configure, and multiple entries may be active simultaneously (each
is an independent local process, not a shared network resource).

```yaml
features:
  codebase_memory:
    - when: [my-project]        # activates the server (same model as bundles)
      index_path: null          # optional override; default <state_dir>/codebase-memory
```

| Field | Required | Notes |
| ------- | ---------- | ------- |
| `when` | yes | Activation tags; an entry with none is rejected at validate time |
| `index_path` | no | Override the index storage directory; defaults to `<state_dir>/codebase-memory` |

llmenv always computes two environment variables for the launched process,
never left to the user:

- `CBM_CACHE_DIR` — the index storage directory (`index_path`, or the default
  above)
- `CBM_ALLOWED_ROOT` — the current working directory, restricting
  `index_repository` to the intended project so a misbehaving agent can't be
  tricked into indexing/reading arbitrary paths outside it

On `SessionStart`, llmenv fires a fire-and-forget
`codebase-memory-mcp cli index_repository` call for the active project. This
both indexes it and registers it with the server's own background
auto-watch (`auto_watch`, on by default upstream), which keeps the index
current as files change — llmenv doesn't re-implement reindex scheduling.

`llmenv doctor` checks that the `codebase-memory-mcp` binary is on `PATH`
whenever this feature is configured, and flags entries whose tags no scope
emits.

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
| ------- | ---------- | ------- |
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

| Field   | Required | Notes                                                                                                                                              |
|---------|----------|----------------------------------------------------------------------------------------------------------------------------------------------------|
| `track` | no       | `"release"` (default) or `"beta"`. `release` uses the GitHub latest-stable endpoint; `beta` uses the first non-draft release from the recent list. |

### `features.context_mode:`

Built-in context-saving support (#490). When enabled, llmenv wires the
context-mode plugin automatically — marketplace, plugin registration, durable
`CONTEXT_MODE_DATA_DIR` state dir, and MCP permission grants — replacing the
manual `plugin-collection` / `state` / `native_permissions` boilerplate.

```yaml
features:
  context_mode:
    enabled: true
```

| Field     | Required | Notes                                                           |
|-----------|----------|-----------------------------------------------------------------|
| `enabled` | no       | Default `false`. Set to `true` to activate the built-in plugin. |

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
| ------- | ---------- | ------- |
| `transcript` | no | Record into ICM's transcript store via the ICM MCP; default `true` |
| `file` | no | Mirror the same event stream to a local JSONL file; default `false` |
| `verbose` | no | Also capture `UserPromptSubmit`/`PreToolUse`/`PostToolUse`/`Notification`/`Stop`/`SubagentStop`/`PreCompact` events, not just the lifecycle + scope header; default `false` |
| `path` | no | Override the file sink's path; default `<state_dir>/session-log.jsonl` |
| `max_content_bytes` | no | Cap each event's `content` field to this many bytes before it's written/recorded; default `16384` |

For finer control, `transcript` can be a mapping instead of a boolean:

```yaml
session_log:
  transcript:
    enabled: true
    retention_days: 30    # best-effort delete stale file transcripts after 30 days
  file:
    enabled: false
    path: "~/custom/path.jsonl"
```

| Sub-field | Required | Notes |
| --------- | -------- | ----- |
| `enabled` | yes | Enable/disable the ICM transcript sink |
| `level` | no | Minimum event level (`info`, `debug`, `trace`); default `info` |
| `retention_days` | no | Stale file-sink transcripts on disk are best-effort removed when older than this many days; `null` = disabled; must be >= 1 |

In this shape, `file` is also a mapping (`FileSinkConfig: enabled, level, path`) and the
shorthand `verbose` flag is unavailable — set `level: debug` on each sink instead.

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

```text
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

## `statusline:`

`llmenv statusline` is a statusline renderer built into the `llmenv` binary —
no separate statusline plugin or binary to install. It reads the engine's
session JSON from stdin, llmenv's own stats from the materialized
`llmenv-status.json`, and this config section, then prints one ANSI-styled
line per row to stdout. See [`statusline`](commands.md#statusline) for how
it's wired into an engine.

```yaml
statusline:
  rows:
    - "{model} │ {context_pct} │ {budget}"
    - "{scopes} · {plugins} {config_stale}"
  style:
    icon_set: auto            # auto | nerd | simple | none
  widgets:
    model:
      format: "{short_name} {version}"
      style: "bold cyan"
    scopes:
      format: "║ {tags}"
      max_len: 40
      style: "dim"
  icons:
    config_stale: "◌"
```

| Field | Required | Notes |
| ------- | ---------- | ------- |
| `rows` | no | One row template per rendered status line, each a string with `{widget_name}` placeholders. Default (when `statusline:` is omitted entirely): a single row, `"{model} │ {folder} │ {branch} │ {context_pct} │ {budget}"` |
| `style.icon_set` | no | `auto`, `nerd`, `simple`, or `none` — see [`icon_set`](#icon_set) below. Default `auto` |
| `style.color` | no | Master colour switch. `true` (default) lets each widget render its default (or configured) colour; `false` forces the whole statusline to plain text, on top of the runtime `--color`/`NO_COLOR` gate |
| `widgets` | no | Map of widget name (`model`, `scopes`, ...) to a `format` / `max_len` / `style` override — see the reference table below for each widget's default format and placeholders |
| `icons` | no | Named icon overrides, merged over the resolved `icon_set` defaults (a name set here always wins) |

Each entry under `widgets:` accepts:

| Sub-field | Notes |
| --------- | ----- |
| `format` | Custom display template for the widget's own placeholders (see the table below). Only honored by widgets marked "yes" in the **Format?** column — set on a widget that doesn't support it, it's silently ignored |
| `max_len` | Max character length; longer output is truncated with `…` (U+2026), UTF-8-safe. Default: no limit |
| `style` | ANSI style string applied to the widget's entire rendered output — see [Style tokens](#style-tokens) below. Every widget has a sensible **default colour** when this is unset; set it to `none` (or `""`) to render that one widget in plain text |
| `display` | Named display mode for widgets that offer presets instead of a free-form `format`: `model` accepts `short` (family only, `Opus`), `version` (family + version, `Opus 4.8`, the default), or `full` (verbatim `display_name`). Overridden by `format` when both are set; ignored by widgets without a display mode |
| `width` | `progress_bar` cell width (default `10`). Ignored by other widgets |
| `thresholds` | Two ascending percentages `[warn, crit]` for value-based coloring. Ignored by widgets without threshold coloring |

A row template can also write `{widget_name:t}` — accepted syntax, but it is a
no-op beyond what `max_len` already does; truncation is driven entirely by
`max_len`, not by this shorthand. An unknown widget name in a template, or a
widget with no data to render, renders as an empty string (not an error). If
every widget in a row renders empty, that row's line in the output is empty
too — never a line of bare separator literals.

### Widget reference

Two widget sources, resolved in this order: **engine-sourced** widgets read
the stdin JSON the engine pipes in every render; **llmenv-sourced** widgets
read `llmenv-status.json`. A name that matches neither renders empty.

#### Engine-sourced (from the engine's stdin JSON)

All thirteen honor `format:` — set on any of them, it replaces the default layout below.

| Widget | Format? | Default output | Example | `format` placeholders |
| -------- | --------- | ----------------- | --------- | ------------------------ |
| `model` | yes | `{short_name} {version}` | `Opus 4.8` | `short_name`, `version`, `full_name` |
| `folder` | yes | 📁 + basename of the working directory | `📁 llmenv` | `basename`, `path` |
| `branch` | yes | 🌿 + git branch name | `🌿 release/3.x` | `name` |
| `pr` | yes | `#<number>` | `#834` | `number` |
| `progress_bar` | yes | `<pct>%` + block bar (`width` cells, default 10) | `35% ▓▓▓░░░░░░░` | `pct`, `bar` |
| `tokens` | yes | total context tokens, `k`/`m`-suffixed | `10k` | `total`, `input`, `cache_read`, `cache_create` |
| `context_pct` | yes | used-context percentage | `35%` | `pct` |
| `budget` | yes | `<used>/<max>`, `k`/`m`-suffixed | `35k/200k` | `used`, `max` |
| `duration` | yes | ⏱ + elapsed (h+m past an hour, else m+s, else s) | `⏱ 3h 42m` | `h`, `m`, `s`, `total_ms` |
| `cache_pct` | yes | ↻ + cache-hit percentage | `↻44%` | `pct` |
| `usage_5h` | yes | Claude.ai 5-hour usage window | `5h 8% ⇡3% ➡23m` | `pct`, `bar`, `reset`, `pace` |
| `usage_7d` | yes | Claude.ai 7-day usage window | `7d 41% ➡3d4h` | `pct`, `bar`, `reset`, `pace` |
| `peak` | yes | peak / off-peak billing window (local clock) | `△ peak 3h03m` | `symbol`, `label`, `countdown` |

Notes:

- `branch` reads the branch from git (`.git/HEAD`, following a worktree
  `.git`-file pointer) resolved from the working directory — Claude Code does
  **not** send a branch on stdin for a regular repo. A `worktree.branch` in the
  stdin JSON (worktree sessions) takes precedence. Detached HEAD renders empty.
- `model` strips a trailing `(…)` qualifier (e.g. `Opus 4.8 (1M context)` →
  `Opus 4.8`) and, when the engine sends no separate `version`, derives it from
  `display_name`.
- Numeric counts (`tokens`, `budget`) use `k` at a thousand and `m` at a
  million, dropping a redundant trailing `.0` (`1000000` → `1m`, `200000` →
  `200k`, `109200` → `109.2k`).
- `usage_5h`/`usage_7d` require the Claude.ai subscription `rate_limits` block,
  which the engine sends only after the first API response in a session; before
  that (or on API/enterprise plans) they render empty. `{reset}` is the time
  until the window resets; `{pace}` is an over/under-pace indicator (`⇡N%`
  when usage is ahead of the time elapsed in the window, `⇣N%` when behind,
  empty within ±0.5%). Both windows are threshold-colored by used percentage
  (`usage_5h` default `[70, 90]`, `usage_7d` `[60, 80]`; override with
  `thresholds`).
- `peak` is computed entirely from the local clock (Anthropic's peak window is
  weekdays 05:00–11:00 America/Los_Angeles) — Claude Code sends no peak data on
  stdin. `{countdown}` counts down to the window boundary (peak ending, or the
  next peak starting).

`pr` and `tokens` only expose the fields above — the engine's stdin contract has no PR title or
per-output-type token breakdown today, so those aren't invented placeholders.

#### llmenv-sourced (from `llmenv-status.json`)

All eight honor `format:`.

| Widget | Default `format` | Example | Placeholders |
| -------- | ------------------- | --------- | -------------- |
| `scopes` | `║ {tags}` | `║ dev · rust` | `tags` (tag list, joined with ` · `) |
| `plugins` | `🔌 {total}` | `🔌 12` | `total`, `errors` |
| `mcps` | `MCP {total}` | `MCP 12` | `total`, `errors` |
| `icm` | `🧠 {memories}` | `🧠 142` | `memories`, `concepts` |
| `cache` | `{prunable}` | `15 MB` | `prunable` (humanized), `prunable_raw` (bytes) |
| `config_stale` | `{stale_icon}` | `◌` | `stale_icon`. Renders empty when the config isn't stale — there's no "fresh" variant |
| `throttle` | `{raw}` | `umans: 45s` | `raw` (`"<backend>: <cooldown_secs>s"`), `cooldown_secs`, `reason` (the backend name) |
| `session_log` | `{icon} {entries}` | `📝 8` | `icon`, `entries` |

An unrecognized placeholder inside a custom `format` string (e.g. `{title}`
on `pr`, or `{count}` on `scopes`) is left in the output literally rather than
being stripped — only the placeholders listed above are substituted.

### `icon_set`

- `simple` — ASCII/Unicode glyphs (`*`, `~`, `!`, `x`, `#`, `log`, ...)
- `nerd` — Nerd Font glyphs (Private Use Area codepoints)
- `none` — every icon resolves to an empty string
- `auto` (default) — there's no portable way to probe a terminal for a Nerd
  Font, so `auto` keys off the `LLMENV_NERD_FONT` environment variable: set it
  to `1` or `true` (case-insensitive) to get Nerd Font glyphs; unset (or any
  other value) falls back to `simple`. Set this the same way you'd set it for
  a shell prompt that has its own Nerd Font auto-detect convention.

Only two icon names are currently consulted by any widget: `config_stale`
(the `config_stale` widget) and `session_log` (the `session_log` widget). The
other names resolvable via `icon_set` (`config_ok`, `icm_ok`, `throttle`,
`plugin_ok`, `plugin_error`, `cache_ok`, `cache_prunable`) are defined and can
be overridden under `icons:`, but no current widget format reads them.

### Style tokens

`style` (on a widget, or via `finish()` internally) is a space-separated list
of tokens applied to the widget's entire output:

- Text attributes: `bold`, `dim`, `italic`, `underline`, `blink`, `reverse`,
  `hidden`, `strikethrough`
- 16-colour foreground names: `black`, `red`, `green`, `yellow`, `blue`,
  `magenta`, `cyan`, `white`
- 256-colour: `color-<n>` (`0`-`255`)
- True colour: `#rrggbb` hex

Unknown tokens are ignored rather than erroring — a typo in a `style` string
degrades to no styling for that token, not a broken render. With
`--color never` (or, absent an explicit `--color`, a non-TTY — which is what
every host UI's captured-stdout pipe looks like), all style tokens are
skipped entirely and widgets render as plain text.

### Claude Code / Crush support

Claude Code gets `llmenv statusline` wired in automatically: the adapter
seeds `"statusLine": {"type": "command", "command": "llmenv statusline --color always"}`
into `settings.json` once, only when that key is absent — a user's own
`/statusline` customization is never overwritten. The `--color always` is
required because Claude Code invokes the command with stdout captured
(never a TTY), and `--color`'s default (`auto`) would otherwise disable every
`style:` widget override in that exact path. Crush has no statusline-hook
concept in its adapter today, so `statusline:` config has no effect there yet
([#855](https://github.com/phaedrus1992/llmenv/issues/855) tracks adding it).

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

| Field     | Required | Notes                                                                |
|-----------|----------|----------------------------------------------------------------------|
| `env`     | yes      | Env var the tool honors (e.g. `CONTEXT_MODE_DATA_DIR`)               |
| `subdir`  | yes      | Single path component under `$LLMENV_STATE_DIR` (no separators)      |

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

## `init:`

Settings pre-seeded into new materialized folders during `llmenv init` (#172).
The interactive setup wizard lets you import keys from your global
`~/.claude/settings.json`; selected keys are stored here and survive every
re-materialization.

```yaml
init:
  seeded_settings:
    enabledPlugins:
      superpowers@claude-plugins-official: true
    autoMemoryEnabled: false
```

`llmenv init` writes this block automatically during the interactive import
step; it is not normally hand-authored.

## `skills:`

First-class skill declarations at the top level, selected onto scopes by tag
intersection — the same model as `mcp:` and `lsp:`. Skills are supported by
every adapter with a skills-directory concept; adapters without one silently
skip them (#661).

```yaml
skills:
  - name: my-skill
    when: [me]
    source: "./path/to/skill/dir"    # local path or marketplace-relative
```

| Field    | Required | Notes                                                                         |
|----------|----------|-------------------------------------------------------------------------------|
| `name`   | yes      | Registration name; deduplicated first-bundle-wins                             |
| `when`   | no       | Activation tags (empty = always active)                                       |
| `source` | yes      | Path to skill directory — absolute, `~/`-relative, or bundle-content-relative |

Skills declared here are merged with per-bundle skills from `bundle.yaml`; the
union is what gets wired up for the active scope. Name collisions are resolved
by declaration order (first wins).

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
