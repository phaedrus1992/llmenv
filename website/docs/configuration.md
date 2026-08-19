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
| `features:` | map | Feature flags; holds `memory:` (ICM backend topology), `codebase_memory:` (codebase-memory-mcp integration), `throttle:` (usage throttling), `upgrade:` (upgrade release track), `read_once:` (re-read deduplication), `task_tracker:` (in-engine task tracker), `slippage:` (behavior-drift guardrails), `repeat_detect:` (loop detection), `cd_guard:` (Bash `cd` advisory), and `context_mode:` (context-mode built-in) |
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
| `normal` (default) | `<adapter>/<version_major>/<shape>/` | Balanced: stable across minor/patch releases, churns on major bumps (added in v3.10.0; before that, `<version_mm>`/minor bumps) |
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
- **Content** *(added in v3.3.0)* `match` fields: `glob` (matched against paths relative to the
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
order least-to-most specific is **network → host → user → content → project**
(`content` joined this ranking in v3.10.0 — see below). List-shaped
values concatenate and de-duplicate instead of overriding. Two contributors at
the **same** precedence disagreeing on a scalar's value is a hard error naming
both — there's no rank to break the tie, so llmenv fails loudly rather than
silently picking one (added in v3.8.0 for every scalar; `default_mode` always
had this).

`content` ranks just below `project`: it's an environment signal derived from
file patterns incidentally present under the current directory (like
network/host/user), not authored intent — but more specific than a bare
user-level match, since it's derived from the actual project's file layout. An
explicit `.llmenv.yaml` (`project`) still outranks it: deliberately authored
project config beats an incidental glob match (#845; before v3.10.0, a bundle
firing only via a `content` scope always landed at the lowest rank,
unconditionally losing every scalar conflict regardless of how specific its
match was).

## `capabilities:`

Engine-neutral capabilities. The same shape is valid here (global) and inside a
bundle's `bundle.yaml` (bundle-scoped); contributors are merged by value shape.

```yaml
capabilities:
  permissions:
    default_mode: acceptEdits           # acceptEdits | plan | default | bypassPermissions
    preset: safe-readonly               # added in v3.8.0 — see below
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
  native_model_providers:               # added in v3.7.0
    opencode: { ... }                    # deep-merged onto the provider block
  native_default_models:                # added in v3.10.0
    crush: { large: { reasoning_effort: high } }  # deep-merged onto the per-role model block
```

- `permissions.default_mode` and `permissions.preset` are scalars (resolved
  by precedence); `allow`/`ask`/`deny` are lists (concatenated + deduped).
- A **permission rule** has a `tool` plus either a glob `pattern` or a list of
  `paths`.
- `permissions.preset` (added in v3.8.0) expands, at merge time, into a
  curated set of `allow` rules — `safe-readonly` is the only preset today. It
  covers the read-only CLI tools this project's own bundled rules recommend
  (`rg`, `ast-grep`, `shellcheck`, `shfmt`) plus safe read-only `git`
  subcommands (`status`, `diff`, `log`, `show`, `blame`) and `ls`, so agents
  stop hitting a permission prompt for tools the rules themselves told them
  to prefer. `git status`/`diff`/`log`/`show`/`blame` and `ls` each get both a
  bare form and a `*`-suffixed one, since the bare form is their dominant
  invocation. `rg`/`ast-grep`/`shfmt` (not `shellcheck`) each ship a `deny`
  companion for the flags that turn them from read-only into arbitrary
  command execution or an in-place write (`rg --pre`/`--hostname-bin`,
  `ast-grep -U`, `shfmt -w`) — Claude Code checks `deny` before `allow`, so
  those specific invocations still prompt. `fd` is deliberately not in the
  preset despite `rg`'s sibling recommendation in the CLI-tools table: its
  own escape (`fd -x`/`-X`) can hide behind any of its ~11 other boolean
  short flags in a single clustered token (e.g. `fd -Lx cmd .`), so a plain
  glob `deny` can't actually close it — see
  [#1219](https://github.com/phaedrus1992/llmenv/issues/1219). A rule the
  preset already covers doesn't duplicate one an explicit `allow`/`deny`
  entry also declares. Run `llmenv doctor` to get flagged when a config
  allows a legacy tool (`grep`, `find`) without also allowing its
  recommended replacement (`rg`, `fd`) — a nudge toward the preset even
  without adopting it.
- A **hook** has an `event`, optional `matcher`, and a `handler` of type
  `command` (with `command:`) or `mcp_tool` (with `tool:`). Hook command paths
  declared in a bundle are bundle-relative and resolved at materialize time.
- `plugins` are `<marketplace>:<plugin>` strings.
- `native_<feature>` maps are per-engine raw fragments emitted verbatim. They are
  the escape hatch for engine-specific rules with no neutral form. See
  [Engines](engines.md).

### `model_providers` / `default_models`

(added in v3.3.0; Crush rendering added in v3.6.1, opencode rendering added in
v3.7.0)

Custom or self-hosted model provider endpoints (Ollama, vLLM, LM Studio, a
proxy, or an override of a built-in provider), and default-model selection by
role. Rendered by the Crush and opencode adapters (`api_type` maps to
opencode's AI SDK package name, e.g. `openai` → `@ai-sdk/openai-compatible`);
Claude Code has no multi-provider concept and silently skips these entries,
so declaring one in a shared bundle is safe.

```yaml
capabilities:
  model_providers:
    - id: ollama
      name: Ollama (local)
      when: ["home"]                     # tag-intersected against active scope tags
      base_url: "http://localhost:11434/v1" # loopback only — use https:// for any remote host
      api_type: openai                   # wire format: openai | anthropic | google | ...
      api_key: "$OLLAMA_API_KEY"         # $VAR/!command reference resolved by the target
                                          # engine at its own runtime — never a literal key
      models:
        - id: llama3.1:70b
          name: "Llama 3.1 70B"
          reasoning: false
          context_window: 128000
          max_tokens: 8192
          cost: { input: 0, output: 0 }
          modalities: ["text"]
  default_models:
    large: { provider: ollama, model: "llama3.1:70b" }
    small: { provider: anthropic, model: "claude-haiku-4-5" }  # built-in provider id, unvalidated
```

- `model_providers[].id` is the stable identifier, used as the map key on
  render and as the `default_models[].provider` target.
- `when` intersects with active scope tags — same selection mechanism as
  `mcp`/`lsp`/`skills`.
- `api_key`/`headers` are passthrough strings — llmenv writes exactly what you
  put here into the materialized config verbatim, with no resolution or
  interpretation of its own. **Use a `$VAR` or `!command` reference, not a
  literal key**, so the credential isn't committed to your config repo or
  synced by `llmenv sync`; the *target engine* resolves the reference at its
  own runtime.
- `disabled: true` excludes the provider from the resolved set for all engines.
- **Per-model request options** (e.g. opencode's `reasoningEffort`, or a local
  server's `enable_thinking`) have no dedicated `model_providers[].models[]`
  field, but reach opencode through `native_model_providers.opencode` (#1007)
  — its rendered `provider.<id>.models` is object-keyed by model id, so a
  fragment can deep-merge onto an *existing* modeled model's options:
  `native_model_providers: { opencode: { <provider-id>: { models: { <model-id>: { options: { reasoningEffort: high } } } } } }`.
  Crush has no equivalent: its rendered `providers.<id>.models` is a JSON
  array, so `native_model_providers.crush` can only *append* a model entry,
  not patch an existing one's fields, and Crush's underlying schema
  (`catwalk.Model`) has no generic options field to route extras through even
  if it could.
- `default_models` is a role-keyed map (`large`, `small`, or any role name the
  target engine recognizes) pointing at a `{ provider, model }` pair.
  `provider` may reference a `model_providers[].id` declared alongside it, or
  an engine builtin (e.g. Crush's built-in `anthropic`) that llmenv doesn't
  validate against. opencode only has two default-model slots (`model` and
  `small_model`), so only the `large` and `small` roles have a destination
  there — any other role name is a no-op for that engine.
- `native_default_models.crush` (added in v3.10.0) deep-merges onto the
  rendered per-role `models` block for Crush's own per-role extras that
  `default_models` has no field for (`reasoning_effort`, `think`,
  `max_tokens`) — opencode has no equivalent slot, since its `model`/
  `small_model` are bare `"provider/model"` strings with no room for extra
  fields. Both `model_providers` and `default_models` may also be declared
  inside a bundle's `bundle.yaml`, same as at the top level.

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

### Deleting a key with `null`

Setting a key to `null` removes it from the generated config entirely, so the
engine falls back to its own default. This works even for keys llmenv itself
renders — those are emitted before the `native:` overlay precisely so you can
override them:

```yaml
capabilities:
  auto_memory_enabled: true
native:
  claude_code:
    autoMemoryEnabled: null   # omit the key; let Claude Code decide
```

The generated `settings.json` contains no `autoMemoryEnabled` key at all,
rather than `"autoMemoryEnabled": null`. Nulls nested inside an object value
are stripped the same way. (behavior changed in v3.10.0; before that a `null`
on a key llmenv had already rendered emitted an explicit JSON `null`)

This is a uniform rule across every engine and every write path that overlays
a `native*` catch-all fragment onto already-rendered output: Claude Code's
`settings.json`, Crush's `crush.json` (`native.crush`), opencode's
`opencode.json` (`native.opencode`), and the `mcpServers` block llmenv merges
into the real, persistent `.claude.json` (`native_mcp.claude_code`) all strip
a `null` down to a deleted key rather than persisting an explicit JSON `null`
(added in v3.10.0).

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

(added in v3.0.0)

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
(codebase-memory-mcp integration), `throttle:` (usage throttling), `upgrade:`
(upgrade release track), `read_once:` (re-read deduplication), `task_tracker:`
(in-engine task tracker), `slippage:` (behavior-drift guardrails),
`repeat_detect:` (loop detection), `cd_guard:` (Bash `cd` advisory), and
`context_mode:` (context-mode built-in). Additional feature flags may be
nested here in future versions.

### `features.memory:`

(added in v1.0.0; `listen_host` added in v1.0.8)

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
| `mcp_permissions` | no | Per-tier permission override for the ICM MCP's tools — see [`mcp_permissions`](#featuresmcp_permissions) below |
| `wakeup_max_tokens` | no | Token budget for the `SessionStart` wake-up call, `20`-`4000` (added in v3.8.0) |

`wakeup_max_tokens` (added in v3.8.0) controls the size of the wake-up pack
injected at session start. When unset, llmenv omits the argument entirely and
icm's own MCP handler falls back to its hardcoded 200-token default — **not**
the 500 tokens icm's own `config.toml` may configure, since that file is never
consulted on this path. Set it explicitly to request a different budget; out-
of-range values fail `llmenv doctor`/materialize validation instead of being
silently clamped.

See [MCP & Memory](mcp.md) for the topology, security model, and `mcp-proxy`
requirements.

### `features.codebase_memory:`

(added in v3.6.0)

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

(added in v3.8.0) A failed `index_repository` run's stderr is captured to
`<index_path (or its default)>/index.log` — size-bounded (rotated past 512
KiB, one prior generation kept) and owner-only (`0o600`) — so a failing
multi-minute index build is diagnosable instead of silently discarding its
output.

| Field | Required | Notes |
| ------- | ---------- | ------- |
| `when` | yes | Activation tags; an entry with none is rejected at validate time |
| `index_path` | no | Override the index storage directory; defaults to `<state_dir>/codebase-memory` |
| `mcp_permissions` | no | (added in v3.10.0) Per-tier permission override for codebase-memory-mcp's tools — see [`mcp_permissions`](#featuresmcp_permissions) below |

(added in v3.8.0) The default index storage directory (`<state_dir>/codebase-
memory`) is created owner-only (`0o700`). An explicit `index_path` override
is not: llmenv leaves its permissions exactly as its owner set them, so a
directory intentionally shared with a `codebase-memory-mcp` process running
under a different uid (a separate service account, or a container with a
different uid mapping) keeps working. If you rely on this sharing, secure the
directory yourself — llmenv won't tighten or loosen it for you.

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

(added in v3.10.0) Claude Code renders a tiered allow/ask policy for
`mcp__codebase-memory-mcp__*`, mirroring the ICM memory MCP's tiering —
read-only/query tools (`search_code`, `search_graph`, `trace_path`,
`get_architecture`, `index_status`, `list_projects`, ...) and non-destructive
mutations (`index_repository`, `ingest_traces`) are pre-approved by default;
`delete_project` and `manage_adr` (both genuinely destructive — the former
irreversibly removes a project's index, the latter is an unversioned
overwrite of the project's ADR document with no history) ask. Previously
every codebase-memory-mcp tool call prompted individually. Override the
default per tier with `codebase_memory[].mcp_permissions` (same shape as
`features.memory[].mcp_permissions`; see that section for the field
reference). A `SKILL.md` reference
(`skills/llmenv/references/codebase-memory.md`) is materialized whenever this
feature is enabled, teaching the agent when to reach for codebase-memory-mcp
instead of a plain `grep`/`find` sweep.

Two caveats worth knowing before relying on the pre-approved tools:

- **The pre-approved read tools are cross-project, not workspace-scoped.**
  `search_code`/`get_code_snippet` take a free-form `project` parameter and
  read straight off disk rooted at whichever indexed project that names —
  not just the one active in the current session. With the default shared
  `CBM_CACHE_DIR`, an agent working in project A can read source out of any
  other project you've ever indexed, without a prompt. Set a per-project
  `index_path` if you need to contain that (the tradeoff: codebase-memory-mcp
  then can't cross-reference other projects for you).
- **`delete_project`'s prompt is not a complete backstop.** `index_repository`
  (pre-approved) accepts a `name` override with no check that the name is
  already bound to a different project's root — a call naming an existing,
  unrelated project silently replaces that project's index. Tracked
  upstream/here as [#1331](https://github.com/phaedrus1992/llmenv/issues/1331).
  An index is re-buildable (re-indexing the correct repo recovers it), so this
  is a nuisance rather than data loss, but it is not gated by the `ask` tier
  the way `delete_project` itself is.

### `features.throttle:`

(added in v2.3.0)

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

(added in v3.3.0)

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

### `features.repeat_detect:`

(added in v3.7.0)

Engine-neutral repeat-loop detection, **on by default** (opt-*out*, not
opt-in — omitting `features.repeat_detect` entirely resolves the same as
`enabled: true` with defaults). Some models — small/local ones especially —
can get stuck re-issuing the exact same tool call turn after turn with no
progress, or ignoring the task tracker's own "you still have a task in
progress" reminder every single turn instead of pausing it. Two independent
trackers share this one setting:

- **Tool calls**: llmenv tracks the most recent tool name + input per
  session and, once the same call repeats `threshold` times in a row,
  injects an advisory nudging the model to stop and try a different
  approach. This still fires even when another feature (e.g. `read_once`)
  already had something to say about the same call — the two aren't
  mutually exclusive, since a model can get stuck retrying a call the other
  feature already denied or warned about.
- **Task-tracker Stop reminder**: if `features.task_tracker` is on and the
  task tracker's "you still have a task in progress" reminder fires
  identically `threshold` times in a row, this appends a pointer to
  `llmenv task wait <slug> "<reason>"` — the actual way to silence the
  reminder while genuinely blocked — instead of just repeating the same
  "keep working" imperative forever. Past `threshold * 3` repeats (added in
  v3.9.0), the reminder stops firing entirely rather than escalating
  further — the listed tasks are often none of the current session's own
  (a different, concurrently active session's), so the `task wait` pointer
  is moot advice and the reminder itself had become the loop. A changed
  task set (a different reminder) resets the streak and re-arms this
  normally.

Both are warnings, never blocks — llmenv never denies the repeated call or
the reminder, so a deliberately re-run command (e.g. re-running `cargo test`
after an unrelated fix) is never blocked. Fires for any adapter/model since
the detector lives in the shared `hook_run` lifecycle layer, not per-adapter
code.

```yaml
features:
  repeat_detect:
    enabled: false  # opt out entirely; omit the block (or `enabled: true`) to keep it on
    threshold: 3    # consecutive identical calls/reminders before the warning fires
```

| Field       | Required | Notes                                                              |
|-------------|----------|--------------------------------------------------------------------|
| `enabled`   | no       | Default `true`.                                                    |
| `threshold` | no       | Consecutive identical calls/reminders before warning; default `3`. |

### `features.cd_guard:`

(added in v3.8.0)

Warn-only `PreToolUse` advisory for Bash commands that `cd`, **on by
default** (opt-*out*, not opt-in — omitting `features.cd_guard` entirely
resolves the same as `enabled: true`). Claude Code resets the working
directory after every Bash call, so a `cd` — whether standalone or the
leading step of a compound command (`cd X && …`) — silently breaks any
*following* command that assumed the new directory. Prose guidance alone
("prefer absolute paths") doesn't reliably stop this; the advisory
mechanizes the reminder instead.

A lightweight heuristic, not a shell parser: it flags any top-level segment
(split on `&&`, `||`, `;`, `|`, or newline) whose first word is literally
`cd`. Never blocks — a deliberate `cd` still runs; the model just gets a
one-line nudge toward absolute paths.

```yaml
features:
  cd_guard:
    enabled: false  # opt out entirely; omit the block (or `enabled: true`) to keep it on
```

| Field     | Required | Notes           |
|-----------|----------|-----------------|
| `enabled` | no       | Default `true`. |

### `features.context_mode:`

(added in v3.0.0)

Built-in context-saving support (#490). When enabled, llmenv wires the
context-mode plugin automatically — marketplace, plugin registration, durable
`CONTEXT_MODE_DATA_DIR` state dir, and MCP permission grants — replacing the
manual `plugin-collection` / `state` / `native_permissions` boilerplate.

```yaml
features:
  context_mode:
    enabled: true
```

| Field             | Required | Notes                                                                                                                   |
|-------------------|----------|-------------------------------------------------------------------------------------------------------------------------|
| `enabled`         | no       | Default `false`. Set to `true` to activate the built-in plugin.                                                         |
| `mcp_permissions` | no       | Per-tier permission override for the context-mode MCP's tools — see [`mcp_permissions`](#featuresmcp_permissions) below |

### `features.mcp_permissions:`

(added in v3.6.1)

Every feature-enabled MCP (`features.context_mode`, each `features.memory`
entry, and — added in v3.10.0 — each `features.codebase_memory` entry)
exposes its tools in three risk tiers — read-only, mutation, and destructive —
and llmenv renders one coherent `allow`/`ask`/`deny` policy for them, never a
wildcard grant that a more specific rule can silently shadow. The default
policy:

| Tier          | Action  |
|---------------|---------|
| `read_only`   | `allow` |
| `mutation`    | `allow` |
| `destructive` | `ask`   |

Override any tier by nesting `mcp_permissions` under the feature. Each key
takes `allow`, `ask`, or `deny`; an omitted key falls back to the default
above.

```yaml
features:
  context_mode:
    enabled: true
    mcp_permissions:
      read_only: allow
      mutation: allow
      destructive: ask   # or "deny" to block destructive tools outright

  memory:
    - server_host: home-server
      port: 9092
      when: [home]
      mcp_permissions:
        destructive: deny

  codebase_memory:
    - when: [my-project]
      mcp_permissions:
        mutation: ask   # e.g. to keep index_repository prompting too
```

An unrecognized value (anything other than `allow`/`ask`/`deny`) is a config
error at load time.

### `features.read_once:`

(added in v3.3.0)

Reduces redundant context usage: tracks files read via the `Read` tool within a
session and warns or denies re-reads of an unchanged file within a TTL window.
Opt-in (disabled by default). Only the `Read` tool is tracked; other tools are
unaffected. A `Read` call with an `offset` or `limit` (a partial read) always
bypasses the cache — only whole-file reads are tracked and deduplicated.
Fail-soft — any cache/IO error passes the read through silently rather than
blocking.

```yaml
features:
  read_once:
    enabled: true
    mode: warn        # "warn" (default) or "deny"
    ttl_seconds: 1200 # cache TTL in seconds; default 1200 (20 min)
```

| Field         | Required | Notes                                                                        |
|---------------|----------|------------------------------------------------------------------------------|
| `enabled`     | no       | Default `false` (opt-in).                                                    |
| `mode`        | no       | `"warn"` (default) — advisory only, or `"deny"` — blocks the re-read.        |
| `ttl_seconds` | no       | Seconds a tracked read stays cached before it counts as new; default `1200`. |

### `features.task_tracker:`

(added in v3.6.0)

In-engine task tracker (#231): durable, agent-native "what am I working on"
state that survives compaction and session restarts. The `llmenv task` CLI
subcommands always work regardless of this flag — it only gates the injected
`llmenv` skill guidance and the SessionStart/Stop lifecycle reminders. Each
`wip` task in a reminder is tagged with the session that started it, and
resuming/finishing it (or closing out a fully-done session) is conditioned on
the agent recognizing that session as its own — a hook can't tell whether a
listed task belongs to this conversation or a different, concurrently running
one.

```yaml
features:
  task_tracker:
    enabled: true
    block_engine_task_tools: true  # default; set false to opt out
```

| Field                     | Required | Notes                                                                                                                                                                                                                                                                                                                                                        |
|---------------------------|----------|--------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| `enabled`                 | no       | Default `false`. When `true`, also redirects the engine's built-in task tools into this tracker via an auto-injected `PreToolUse` hook — Claude Code's `TaskCreate`/`TaskList`/`TaskUpdate`, and opencode's `todowrite` (added in v3.11.0). See [Commands](commands.md#task) for opencode's list-reconciliation rules.                                       |
| `block_engine_task_tools` | no       | (added in v3.10.0) Default `true`. Set `false` to keep the tracker's CLAUDE.md fragment and reminders while still letting the engine's native task tools through — e.g. for genuine multi-agent teammate coordination that isn't solo step tracking. Gates opencode's `todowrite` redirect too (added in v3.11.0). Has no effect while `enabled` is `false`. |

See [Commands](commands.md#task) for the full `llmenv task` CLI reference.

### `features.slippage:`

(added in v3.3.0)

Guardrails against model behavior drift across long sessions (effort decay,
forgetting rules after context compaction). The master switch `enabled` gates
every sub-layer: with it off, no layer runs regardless of its own setting.

All layers are wired to behavior as of v3.11.0. Before that, only
`effort_level`, `compact_survival`, and `diagnose_command` had any effect —
the other fields parsed but did nothing.

```yaml
features:
  slippage:
    enabled: true
    effort_level: xhigh     # injected into generated engine settings; omit to leave untouched
    compact_survival: true  # CLAUDE.md fragment: re-read rules after compaction
    diagnose_command: true  # materializes a /diagnose skill (evidence-first debugging checklist)
    rule_reinjection: true  # short standing-rules digest on every prompt
    read_before_edit: true  # deny Write to an existing file not read this session
    self_critique: true     # checklist appended at Stop
    metrics: true           # count tool use; store a read:edit summary at session end
    explain_before_act: false  # opt-in: deny a modifying command with no explanation yet
    answer_before_act: false   # opt-in: deny a tool call while a question is unanswered
```

| Field                | Required | Notes                                                                                                                                                                                                                                       |
|----------------------|----------|---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| `enabled`            | no       | Default `false` (opt-in master switch).                                                                                                                                                                                                     |
| `effort_level`       | no       | Reasoning-effort value injected into generated engine settings (e.g. `"xhigh"`, `"high"`); omitted means untouched.                                                                                                                         |
| `compact_survival`   | no       | Default `true`. Merges a short rules fragment into the generated CLAUDE.md reminding the agent to re-read its rules after context compaction.                                                                                               |
| `diagnose_command`   | no       | Default `true`. Materializes a `/diagnose` skill: a structured symptoms → evidence → hypotheses → test → act checklist.                                                                                                                     |
| `rule_reinjection`   | no       | Default `true` (added in v3.11.0). Injects a short standing-rules digest on each `UserPromptSubmit`. Deliberately small — it is re-sent every turn. Not sent at session start, where CLAUDE.md already carries the rules.                   |
| `read_before_edit`   | no       | Default `true` (added in v3.11.0). Denies a `Write` to a file that exists but hasn't been read this session; `Write` replaces the whole file. Files that don't exist yet are always allowed, and `Edit` is left to Claude Code's own guard. |
| `self_critique`      | no       | Default `true` (added in v3.11.0). Appends a short checklist at `Stop` (tests run, anomalies explained, scope finished). Advisory — it never blocks.                                                                                        |
| `metrics`            | no       | Default `true` (added in v3.11.0). Counts tool calls and stores a read-to-edit summary to memory at session end, folded into the store that already happens there.                                                                          |
| `explain_before_act` | no       | Default `false` (added in v3.11.0). Denies a *modifying* Bash command when nothing has been said yet this turn. Off by default: a transcript heuristic.                                                                                     |
| `answer_before_act`  | no       | Default `false` (added in v3.11.0). Denies a tool call while the user's question sits unanswered. Off by default: a transcript heuristic.                                                                                                   |

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
  transcript:            # ICM transcript sink (on by default)
    enabled: true
    level: info
  file:                  # local JSONL file sink
    enabled: false
    level: info
  # max_content_bytes: 16384   # cap per-event content size
```

Each sink is a mapping with its own `enabled` and `level`, so one can capture
prompts and tool calls while the other records only lifecycle events.

| Field | Required | Notes |
| ------- | ---------- | ------- |
| `transcript` | no | ICM transcript sink, recorded via the ICM MCP. Enabled by default; omit the block entirely and you get this sink at `info` |
| `file` | no | Mirror the same event stream to a local JSONL file; disabled by default |
| `max_content_bytes` | no | Cap each event's `content` field to this many bytes before it's written/recorded; default `16384` |

Both sinks take the same sub-fields, plus one each of their own:

| Sub-field | Required | Applies to | Notes |
| --------- | -------- | ---------- | ----- |
| `enabled` | no | both | Turn the sink on or off |
| `level` | no | both | Minimum event level (`info`, `debug`, `trace`); default `info`. `debug` is what adds tool calls — prompts are already captured at the default `info` level, see [What gets logged](#what-gets-logged) |
| `path` | no | `file` | Override the file sink's path; default `<state_dir>/session-log.jsonl` |
| `retention_days` | no | `transcript` | Stale file-sink transcripts on disk are best-effort removed when older than this many days; `null` = disabled; must be >= 1 |

Omitting the `session_log:` block entirely enables the transcript sink at
`info` — ICM transcript logging is **on by default**. To turn logging off
entirely, disable both sinks:

```yaml
session_log:
  transcript:
    enabled: false
  file:
    enabled: false
```

> Breaking change in 4.0 (added in v4.0.0): the boolean form — `transcript: true`,
> `file: true`, `verbose: true` — is rejected rather than translated. Each sink
> is now a mapping, and `verbose: true` became `level: debug` on whichever sink
> should capture prompts and tool use, so the two sinks can differ. llmenv names
> the replacement in the parse error; it does not migrate the file for you.
>
> Breaking change in 3.0: `session_log:` used to be a bare path string (the
> file sink only). That form is now rejected with a migration hint — wrap the
> path in `path:` under the new table shape.

### What gets logged

Two layers, gated by each sink's `level`:

- **Baseline** (`level: info`, the default, whenever a sink is enabled):
  `lifecycle_start` at session start, `scope` carrying the active
  tags/bundles/project, `lifecycle_end` at session end — **and also every
  prompt submission, notification, stop, subagent stop, and pre-compact
  event**, tagged with its role. `info` is not a summary-only level: it's
  everything except the two tool-call events below. A user's prompt text
  reaches whichever sink is enabled (the transcript sink, over the ICM MCP,
  by default) unless that sink is turned off.
- **Verbose** (`level: debug`): the two tool-call events on top of the above —
  `tool_use` (before) and `tool_result` (after), each tagged with the tool
  name.

Because `level` is per sink, a common setup is `debug` on the local file and
`info` on the transcript — full tool-call detail stays on the machine, while
ICM still receives everything at `info`, prompts included. To keep prompt text
off ICM entirely, disable the transcript sink rather than relying on `level`.

> **Privacy note:** `level: debug` captures the *raw* text of every prompt
> you submit and every tool call's input/output — including any secrets,
> credentials, or personal data that text happens to contain. That content is
> written to disk (the `file` sink) and/or sent to ICM (the `transcript` sink)
> unredacted, capped only by `max_content_bytes` (default 16 KiB, not a
> sensitivity filter). Treat a `session-log.jsonl` recorded at `debug` the same
> way you'd treat shell history that might contain pasted secrets.

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

If `config.yaml` itself fails to parse, the statusline can't read this section
at all and renders an error row instead — see
[Broken config renders an error row](commands.md#broken-config-renders-an-error-row)
(added in v3.8.0).

```yaml
statusline:
  rows:
    - "{model} │ {context} │ {budget}"
    - "⎿ {scopes} · {plugins} {config_stale}"
  style:
    icon_set: auto            # auto | nerd | simple | none
  widgets:
    model:
      format: "{short_name} {version}"
      style: "bold cyan"
    scopes:
      format: "{tags}"
      max_len: 40
      style: "dim"
  icons:
    config_stale: "◌"
```

| Field | Required | Notes |
| ------- | ---------- | ------- |
| `rows` | no | One row template per rendered status line, each a string with `{widget_name}` placeholders. Default (when `statusline:` is omitted entirely): a single row, `"{model} │ {folder} │ {branch} │ {context} │ {budget}"` |
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
| `display` | Named display mode for widgets that offer presets instead of a free-form `format`: `model` accepts `short` (family only, `Opus`), `version` (family + version, `Opus 4.8`, the default), or `full` (verbatim `display_name`); `pr` accepts `number` (`#834`, default) or `url` (full PR URL, falling back to `#<number>` when the engine sends none). Overridden by `format` when both are set; ignored by widgets without a display mode |
| `width` | Bar cell width for `context`/`cache_usage`/`usage_5h`/`usage_7d` (default `10`). Ignored by other widgets |
| `thresholds` | Two ascending percentages `[warn, crit]` for value-based coloring. Ignored by widgets without threshold coloring |

A row template can also write `{widget_name:t}` — accepted syntax, but it is a
no-op beyond what `max_len` already does; truncation is driven entirely by
`max_len`, not by this shorthand. A recognized widget with no data to render
(e.g. `pr` with no open PR) renders as an empty string (not an error). An
**unknown** widget name — a typo, or a config still referencing a widget
that's since been renamed or removed — renders `⚠️` instead, so a
misconfigured row is visibly flagged rather than silently vanishing. If every
widget in a row renders empty, that row's line in the output is empty too —
never a line of bare separator literals; a row with an unknown-widget warning
is not empty, so it still prints.

### Widget reference

Two widget sources, resolved in this order: **engine-sourced** widgets read
the stdin JSON the engine pipes in every render; **llmenv-sourced** widgets
read `llmenv-status.json`. A name that matches neither renders empty.

#### Engine-sourced (from the engine's stdin JSON)

All twelve honor `format:` — set on any of them, it replaces the default layout below.

| Widget | Format? | Default output | Example | `format` placeholders |
| -------- | --------- | ----------------- | --------- | ------------------------ |
| `model` | yes | `{short_name} {version}` | `Opus 4.8` | `short_name`, `version`, `full_name` |
| `folder` | yes | 📁 + basename of the working directory | `📁 llmenv` | `basename`, `path` |
| `branch` | yes | 🌿 + git branch name | `🌿 release/3.x` | `name` |
| `pr` | yes | `#<number>` (or the URL in `display: url`) | `#834` | `number`, `url`, `review_state` |
| `context` | yes | used-context `<pct>%` + block bar (`width` cells, default 10), threshold-colored (default `[50, 80]`) | `35% ▓▓▓░░░░░░░` | `pct`, `bar` — use either alone, or both, in a custom `format` |
| `tokens` | yes | total context tokens, `k`/`m`-suffixed | `10k` | `total`, `input`, `cache_read`, `cache_create` |
| `budget` | yes | `<used>/<max>`, `k`/`m`-suffixed | `35k/200k` | `used`, `max` |
| `duration` | yes | ⏱ + elapsed (h+m past an hour, else m+s, else s) | `⏱ 3h 42m` | `h`, `m`, `s`, `total_ms` |
| `cache_usage` | yes | ↻ + cache-hit `<pct>%` (no bar by default — unlike `context`, a *high* cache percentage is good, so this doesn't threshold-color) | `↻44%` | `pct`, `bar` (opt-in — e.g. `format: "↻{pct}% {bar}"`) |
| `usage_5h` | yes | Claude.ai 5-hour usage window | `5h 8% (+4.5) ⇡3% ➡23m` | `pct`, `bar`, `reset`, `pace`, `delta` |
| `usage_7d` | yes | Claude.ai 7-day usage window | `7d 41% ➡3d4h` | `pct`, `bar`, `reset`, `pace`, `delta` |
| `peak` | yes | peak / off-peak billing window (local clock) | `△ peak 3h03m` | `symbol`, `label`, `countdown` |

`context` merges what used to be two separate widgets (`context_pct` and
`progress_bar`) into one — the percentage and the bar are just two
placeholders of the same widget now, so a custom `format` can show either
alone or both together, instead of needing two widget entries in a row
template to combine them. Every percentage-based widget (`context`,
`cache_usage`, `usage_5h`, `usage_7d`) shares this same percent/bar
rendering backend, so `width` and the `pct`/`bar` placeholders behave
identically across all four.

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
  empty within ±0.5%). `{delta}` is the change in used percentage since the
  last render (`(+4.5)`), tracked in a small state file under
  `$CLAUDE_CONFIG_DIR/statusline-state/` and rewritten at most once a minute so
  it reflects real movement, not per-render noise (empty when `CLAUDE_CONFIG_DIR`
  is unset). The bar is per-cell (filled in the threshold color, empty dim) with
  a bright pace-target marker (`│`); both windows are threshold-colored by used
  percentage (`usage_5h` default `[70, 90]`, `usage_7d` `[60, 80]`; override with
  `thresholds`).
- `peak` is computed entirely from the local clock (Anthropic's peak window is
  weekdays 05:00–11:00 America/Los_Angeles) — Claude Code sends no peak data on
  stdin. `{countdown}` counts down to the window boundary (peak ending, or the
  next peak starting).
- `pr` is colored by `{review_state}` when the engine sends one: `approved`
  green, `changes_requested` red, `pending`/`review_required` yellow. When a PR
  URL is present and color is on, `pr` (and the `branch` widget) render as an
  OSC 8 terminal hyperlink to the PR — the URL is validated (`http`/`https`
  only, no control chars) before it's embedded.
- `pr` self-resolves when the engine sends none — Claude Code never sends a
  `pr` field on stdin, so llmenv runs `gh pr view` for the branch `branch`
  resolves (git, from the working directory) and maps `gh`'s `reviewDecision`
  onto the same `review_state` values above. The result is cached for 60
  seconds (keyed by repo + branch, alongside the `usage_5h`/`usage_7d` state
  under `$CLAUDE_CONFIG_DIR/statusline-state/`) so the statusline — re-rendered
  on every prompt — doesn't shell out on every render. `pr` renders empty,
  with no error output, whenever `gh` isn't installed, isn't authenticated,
  there's no remote, HEAD is detached, or there's no open PR for the branch.
  An engine-supplied `pr` always takes precedence over the derived one. The
  `branch` widget's OSC 8 hyperlink uses this same resolution (engine-supplied
  first, then derived), so the branch text links to its PR under Claude Code
  too — sharing the same cache, so enabling both widgets doesn't double the
  `gh` lookups.
- Untrusted free-text (model/folder/branch names, PR URL, tags, throttle
  backend) is stripped of control characters at the point each widget
  interpolates it, so a hostile directory or branch name can't inject terminal
  escapes. Widgets emit only their own trusted escapes (colors, hyperlinks).

`pr` and `tokens` only expose the fields above — the engine's stdin contract has no PR title or
per-output-type token breakdown today, so those aren't invented placeholders.

#### llmenv-sourced (from `llmenv-status.json`)

All nine honor `format:`.

| Widget | Default `format` | Example | Placeholders |
| -------- | ------------------- | --------- | -------------- |
| `scopes` | `{tags}` | `dev · rust` | `tags` (tag list, joined with ` · `) |
| `plugins` | `🔌 {total}` | `🔌 12` | `total`, `errors` |
| `mcps` | `MCP {total}` | `MCP 12` | `total`, `errors` |
| `icm` | `🧠 {memories}` | `🧠 142` | `memories`, `concepts` |
| `cache` | `{prunable}` | `15 MB` | `prunable` (humanized), `prunable_raw` (bytes) |
| `config_stale` | `{stale_icon} stale` | `⚙️ stale` | `stale_icon` (resolves from the icon set, gear emoji by default — a `statusline.icons.config_stale` override applies even without a custom `format`). Config out of date — relaunch to reload. Renders empty when the config isn't stale — there's no "fresh" variant |
| `throttle` | `{raw}` | `umans: 45s` | `raw` (`"<backend>: <cooldown_secs>s"`), `cooldown_secs`, `reason` (the backend name) |
| `session_log` | `{icon} {entries}` | `📝 8` | `icon`, `entries` |
| `tasks` | `☑ {done}/{total}` (summed across the current project's open sessions, #905); renders empty when no session is open for this project | `☑ 2/5` | `done`, `total` (summed across every session open for the current project), `current` (title of the task currently `wip`/`waiting` among those sessions; empty when none). The default doesn't show `current` — combine it yourself, e.g. `format: "{done}/{total} — {current}"` |

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

### Inherited Claude Code state

(added in v3.8.0)

Some Claude Code state has no env var to relocate it — it is hardcoded to live
inside `CLAUDE_CONFIG_DIR`. llmenv inherits that state into each newly
materialized folder automatically; there is nothing to configure.

| State | Where it lives | How it's inherited |
| ----- | -------------- | ------------------ |
| `/resume` transcripts | `projects/<escaped-cwd>/<session-uuid>.jsonl` | The folder's `projects/` is a symlink to `$LLMENV_STATE_DIR/projects`, so every folder shares one transcript store |
| Prompt history (`↑` recall) | `history.jsonl` | Copied in from `$LLMENV_STATE_DIR` when the folder has none |
| MCP "needs auth" record | `mcp-needs-auth-cache.json` | Copied in when the folder has none, so Claude Code doesn't re-probe every OAuth MCP server |
| OAuth credential | macOS keychain, service name keyed by the config-dir path; `.credentials.json` elsewhere | Cached in `$LLMENV_STATE_DIR/auth/credentials.json` (owner-only, `0600`) and written into a folder that has none |
| Claude Code's internal session logs (added in v3.9.0) | `session-logs/` (one file per calendar day) | The folder's `session-logs/` is a symlink to `$LLMENV_STATE_DIR/session-logs`, same one-store-shared-by-every-folder treatment as `/resume` transcripts |

Transcripts are linked rather than copied, so a session started under one config
hash stays visible to `/resume` after a config edit or version bump — and there
is one store on disk instead of a copy per folder.

`history.jsonl` is copied instead of linked because a single file rewritten via
write-then-rename would replace the symlink with a regular file. A copy is only
made when the folder has none; llmenv never overwrites a folder's own history.

On first run after upgrading, transcripts stranded in older hashed folders (from
before this behavior existed) are folded into the shared store, with the newest
copy of a given session winning.

#### OAuth credential inheritance

(added in v3.8.0)

Staying logged in needs two separate things. The account identity
(`oauthAccount` in `.claude.json`) says *who* you are; the OAuth token says you
are authenticated. llmenv has inherited the identity since v1.0.0 — the token is
inherited as of v3.8.0, so a config edit or version bump no longer produces a
login prompt.

The token is not stored in a stable place by Claude Code on either platform. On
Linux and WSL it is `.credentials.json` inside `CLAUDE_CONFIG_DIR`, so it dies
with the folder. On macOS it is a keychain generic password whose *service name
embeds a hash of the config-dir path* — a different path is a different keychain
item, so the keychain is no more stable across hash changes than a file is.
llmenv handles both.

Two rules govern the cache, and both exist to avoid destroying a working login:

- **On `export`:** the folder's token is copied into the cache only when the
  cache is empty or the cached token is dead. A live cached token is never
  overwritten by whatever a possibly-stale folder happens to hold.
- **On materialization:** the cached token is written into the new folder only
  when that folder has none. A token the folder already holds is never replaced.

"Dead" means the access token is past `expiresAt` *and* no live refresh token
remains. An expired access token with a valid refresh token is still worth
keeping — Claude Code renews it on next use.

`llmenv login` caches the token alongside the account identity, and writes both
into the current folder when one is active.

`llmenv doctor` reports whether a token is cached and whether it has expired.

On macOS, a keychain lookup that fails for any reason other than "no matching
item" (most commonly a locked keychain) surfaces as an explicit error rather
than being treated as "no credential stored" (added in v3.8.0).

#### Third-party MCP server logins

(added in v3.8.0)

Authenticating an OAuth-backed MCP server — Slack, Notion, Linear, and the like —
also survives a hash change, and needs nothing extra configured.

Claude Code keeps those tokens under an `mcpOAuth` key **in the same store as the
login token**, keyed per server as
`<server-name>|<sha256({type,url,headers})[..16]>`. Because llmenv caches and
re-injects that store verbatim, MCP tokens come along with the login token
automatically. The per-server key includes the server's URL and headers, so
changing either invalidates just that server's entry rather than the rest.

Two consequences worth knowing:

- A dead login token does **not** discard live MCP tokens. They authenticate
  different things and expire independently, so a store holding MCP tokens is
  kept even when the Claude login in it has lapsed.
- `llmenv doctor` appends the MCP token count to its credential line, e.g.
  `OAuth credential cached at … (+3 MCP server tokens)`.

The `claude.ai` connectors are managed by the Claude desktop app rather than
Claude Code, so they're outside llmenv's scope.
`llmenv doctor --gc` additionally drops the macOS keychain item belonging to each
cache folder it deletes, since that item would otherwise outlive the folder it
was keyed to. Entries are matched by folder path, so your default `~/.claude`
login is never touched. This runs only under `--gc`, never on `export`.

## `marketplace:` and `plugin-collection:`

(added in v1.0.0)

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
    path: "./path/to/skill/dir"    # local path or marketplace-relative
```

| Field  | Required | Notes                                                                         |
|--------|----------|-------------------------------------------------------------------------------|
| `name` | yes      | Registration name; deduplicated first-bundle-wins                             |
| `when` | no       | Activation tags (empty = always active)                                       |
| `path` | yes      | Path to skill directory — absolute, `~/`-relative, or bundle-content-relative |

Skills declared here are merged with per-bundle skills from `bundle.yaml`; the
union is what gets wired up for the active scope. Name collisions are resolved
by declaration order (first wins).

## `output_styles:`

(added in v3.10.0)

Output styles change *how* Claude Code responds (role, tone, format) by
editing the system prompt — not what it knows, unlike `CLAUDE.md`/rules
content. Declared at the top level or per-bundle, selected onto scopes by tag
intersection — same model as `skills:`/`lsp:`.

```yaml
output_styles:
  - name: concise
    description: Terse, no preamble
    content: |
      Answer in as few words as possible. No explanations unless asked.
    when: [me]
```

| Field                      | Required | Notes                                                                                                                                                             |
|----------------------------|----------|-------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| `name`                     | yes      | Registration name; deduplicated first-bundle-wins                                                                                                                 |
| `description`              | yes      | One-line description                                                                                                                                              |
| `content`                  | yes      | Markdown body appended to the system prompt (Claude Code) or the skill's body (fallback adapters)                                                                 |
| `when`                     | no       | Activation tags (empty = always active)                                                                                                                           |
| `keep_coding_instructions` | no       | Keep Claude Code's built-in coding instructions alongside this style. Default `false` — see the warning below. No effect on the fallback path                     |
| `force_for_plugin`         | no       | Claude Code plugin styles only — auto-activate whenever the plugin is enabled. `llmenv doctor` flags it set outside a plugin bundle, since it has no effect there |

With the default `keep_coding_instructions: false`, Claude Code's built-in
coding instructions — including its git-safety guidance (don't commit unless
asked, don't force-push, don't touch git config) — are replaced entirely by
`content`, not merged with it. Set `keep_coding_instructions: true` to keep
those guardrails active alongside the style.

Claude Code renders each tag-active entry to `output-styles/<name>.md` with
the corresponding YAML frontmatter, and sets `outputStyle` in `settings.json`
to the *one* non-`force_for_plugin` style, when exactly one is active. Zero or
more than one leaves the selector untouched — unlike `memory`/
`codebase_memory` (which resolve to a single MCP registration slot), holding
multiple style **files** simultaneously is not a conflict; only the selector
is single-valued.

Every other engine (Crush, opencode) has no native output-style concept, so
the same `name`/`description`/`content` renders as a generated skill instead
(`skills/<name>/SKILL.md`) — automatic, no config-author-side fallback logic.
`keep_coding_instructions`/`force_for_plugin` have no effect on this path. A
style `name` that collides with a first-class skill, a reserved built-in
skill name, or a skill projected from an installed plugin is rejected at
materialize time, instead of silently overwriting or being shadowed by that
skill.

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

Disabling a bundle withdraws *everything* it contributes, not just its
permissions and instruction files. That includes any `features.memory` or
`host:` entry declared in its `bundle.yaml` — so if the ICM memory backend is
declared only by a bundle you disable, memory recall/store and session logging
are inactive in that project. Declare `features.memory` at the top level of
`config.yaml` if it should survive a bundle being turned off.

(added in v3.8.0) llmenv names `disable_bundles` as the cause rather than
leaving you to guess. Lifecycle hooks report `no memory backend active for this
scope: features.memory is supplied only by bundle(s) <name>, which this project
turns off via disable_bundles`, and `llmenv doctor --all` warns about the same thing —
previously both were silent, so memory worked in `~/` and stopped the moment you
`cd`'d into the project with a green `doctor`. If a *top-level*
`features.memory` entry's `server_host` was declared in the disabled bundle's
`host:` table, the resulting error names the bundle too instead of only the
missing host key.

Each tag (and each `enable_bundles`/`disable_bundles` entry) must be
alphanumeric plus `-`/`_` and no longer than 64 bytes; entries outside that
charset or length, and any beyond the first 64 from a given source, are
dropped with a `warning:` line naming the tag rather than breaking the session
(the warning became visible by default in v3.11.0; before that it went to a
log level nothing displayed). The same rule applies to `$LLMENV_EXTRA_TAGS`
below and to tags declared on `config.yaml`'s network/host/user/content
scopes.

### Activating tags without a committed marker

`$LLMENV_EXTRA_TAGS` (comma-separated) unions additional tags into the active
set without requiring a `.llmenv.yaml` at all — useful for a client repo you
can't add config files to, a throwaway clone, or a personal preference you
don't want to share with collaborators via a checked-in file:

```bash
export LLMENV_EXTRA_TAGS="rust,personal"
```

These tags are additive on top of whatever `.llmenv.yaml` already contributes
(or on top of nothing, if there's no marker file present). See
[`docs/env-vars.md`](https://github.com/phaedrus1992/llmenv/blob/main/docs/env-vars.md)
for the full variable reference.

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
