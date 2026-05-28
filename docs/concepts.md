# Concepts

llmenv resolves your environment through one fixed pipeline:

```
                 ┌─────────┐   tags   ┌──────────┐  fire   ┌────────────┐
 environment ──▶ │ scopes  │ ───────▶ │  active  │ ──────▶ │ contributors│
 (net/host/      │ match   │          │   tags   │         │ bundles,   │
  user/cwd)      └─────────┘          └──────────┘         │ mcp, plugins│
                                                           │ memory     │
                                                           └─────┬──────┘
                                                                 │ merge
                                                                 ▼
                                                          ┌────────────┐
                                                          │ materialize│  content-hashed
                                                          │  cache dir │  config directory
                                                          └─────┬──────┘
                                                                │ render
                                                                ▼
                                                          ┌────────────┐
                                                          │  adapter   │  CLAUDE.md,
                                                          │   emit     │  settings.json,
                                                          └─────┬──────┘  mcp.json + env vars
                                                                ▼
                                                            the agent
```

## Scopes

A **scope** answers "where am I?". There are four kinds:

| Kind | Matches on | Declared in |
|------|-----------|-------------|
| `network` | gateway MAC address | `config.yaml` under `scope.network` |
| `host` | hostname (case-insensitive) | `config.yaml` under `scope.host` |
| `user` | `$USER` | `config.yaml` under `scope.user` |
| `project` | a `.llmenv.yaml` marker file | the project tree itself (not `config.yaml`) |

Network, host, and user scopes are statically configured. The **project** scope
is *discovered*: llmenv walks the current working directory upward — stopping at
`$HOME` — looking for a `.llmenv.yaml` marker. The deepest marker found wins.
This keeps per-project configuration with the project, not in a central file. See
[Project markers](#project-markers) below.

Each scope carries a list of `tags`. A scope is *active* when its match
condition holds; all active scopes contribute their tags to the active set.

> Note: `network` matching currently uses `gateway_mac` only. The `ssid` and
> `cidr` match fields parse but are not yet evaluated.

## Tags

**Tags** are the join key between scopes and everything they select. They have no
meaning of their own — they are arbitrary labels you choose (e.g. `office`,
`rust`, `me`). A contributor activates when **any** of its tags is in the active
set.

## Contributors

Four things select onto scopes by tag intersection, all with identical
semantics:

- **Bundles** (`bundle:`) — environment variables plus an optional content
  directory of files (`<config_dir>/bundles/<name>/`) merged into the agent
  config.
- **MCP servers** (`mcp:`) — stdio or remote Model Context Protocol servers.
- **Plugin collections** (`plugin-collection:`) — named bags of
  `marketplace:plugin` references.
- **Memory backend** (`memory:`) — llmenv's own networked memory service.

A bundle can also be force-enabled by name through a project marker's
`enable_bundles` list, independent of its tags — the escape hatch for "always
load this bundle in this repo".

## Materialize

All firing contributors are merged into a single **manifest**, which is written
to a content-addressed directory under the cache:

```
<cache_dir>/<adapter>/<VERSION_TAG>-<content_hash>/
```

- `VERSION_TAG` is `<pkg_version>-<git_short_hash>` (baked in at build time), so
  folders group by binary version and pruning is "remove anything not starting
  with the current tag".
- `<content_hash>` is a SHA-256 of the merged manifest — identical inputs reuse
  the same folder, so re-materializing is free.
- Plugin marketplaces are cloned once into `<cache_dir>/marketplaces/<name>/` and
  shared across scopes; the resolved git HEAD is mixed into the content hash so a
  marketplace update re-renders.
- Partial writes stage as `*.tmp` directories and are cleaned by `prune` /
  `doctor --gc`.

## Adapter emit

An **adapter** renders the materialized manifest into an engine's native shape.
The Claude Code adapter writes `CLAUDE.md` (rules), `settings.json` (permissions,
hooks, plugins), and `mcp.json` (MCP servers) — all with `0600` permissions —
then returns the env vars that point the agent at the directory
(`CLAUDE_CONFIG_DIR`). It also registers a `SessionStart` hook running
`llmenv check-stale`, which warns you to restart when the booted config drifts
from what llmenv would materialize now.

See [Engines](engines.md) for the capability model and per-engine escape
hatches.

## Precedence

When scopes of different kinds set conflicting **scalar** values (e.g. a
permission `default_mode`), the more specific scope wins. The order, least to
most specific:

```
network → host → user → project
```

List-shaped values (permission allow/ask/deny lists, hooks, plugins) concatenate
and de-duplicate across all contributors rather than overriding.

## Introspection

After resolution, `llmenv export` emits the active context as environment
variables so the agent and your shell can read it back:

| Variable | Format |
|----------|--------|
| `LLMENV_ACTIVE_SCOPES` | `kind:id,…` |
| `LLMENV_ACTIVE_TAGS` | `tag,…` (sorted) |
| `LLMENV_ACTIVE_BUNDLES` | `name,…` (declaration order) |
| `LLMENV_ACTIVE_PROJECT` | deepest project scope id (omitted if none) |
| `LLMENV_PROJECT_ROOT` | absolute path of the deepest marker (omitted if none) |
| `LLMENV_ICM_CONTEXT` | tags/bundles encoded for tag-scoped memory |

## Project markers

A project marker is a `.llmenv.yaml` file at the root of a project. Every field
is optional; an empty file is valid.

```yaml
id: myapp                       # defaults to the folder basename
name: MyApp                     # defaults to the folder basename
description: "Customer API"     # capped at 1024 bytes; surfaced into agent context
tags: [myapp, rust]             # joined into the active tag set
enable_bundles: [base]          # force-enable bundles regardless of their tags
```

Discovery rules:

- The walk starts at the current directory and ascends **to `$HOME` inclusive**,
  then stops. A marker above `$HOME` (e.g. in `/tmp` on a shared host) is
  ignored — a deliberate guard against hostile markers.
- When `$HOME` is unknown, only the current directory itself is checked.
- Malformed YAML degrades gracefully: a warning is logged and a minimal project
  is used, with `id`/`name` taken from the folder basename.
- Unknown fields are collected and reported by `llmenv doctor`.
