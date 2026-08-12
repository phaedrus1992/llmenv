<!-- markdownlint-disable MD013 -->

# Engines

llmenv emits agent-native configuration through pluggable **adapters**. The
configuration you write is engine-neutral; each adapter translates it into one
engine's native shape. Anything that can't be expressed neutrally drops through a
per-engine escape hatch.

Two adapters ship today: **Claude Code** and **Crush**. Both activate when their
binary is on `PATH`; users who only have one binary on PATH see no output from the
other adapter. The design doc behind this model is
[`docs/design/engine-capabilities.md`](https://github.com/phaedrus1992/llmenv/blob/main/docs/design/engine-capabilities.md) (related: #34, #59).

## The principle

> Don't model the container. Model the capabilities inside it.

The portable concepts — which tools are allowed, which paths are reachable, which
hooks fire on which events, which plugins load — are engine-agnostic. Each
adapter renders them into its native config. Everything non-portable goes through
a per-engine `native` passthrough.

## Two layers

Every modeled feature has **both** of these:

1. **Generic capability** — an engine-neutral declaration, translated per
   adapter. Lives under `capabilities:` (`permissions`, `hooks`, `plugins`) and
   under `mcp:` for servers.
2. **Per-engine `native_<feature>` override** — a raw fragment in the engine's
   own language, emitted verbatim. Named as a top-level sibling under
   `capabilities:`: `native_permissions`, `native_hooks`, `native_plugins`,
   `native_mcp`, `native_model_providers`.

A feature with only layer 1 is considered incomplete — there is always some
platform-specific need (a Claude-only permission grammar, a Codex-only hook
event) that requires the override.

### Engine keys are validated

(added in v3.8.0)

Every `native_<feature>` map is keyed by an engine id, and each adapter reads
only its own key. Two kinds of key are therefore never rendered:

- **An unknown engine id** — a typo like `native_mcp.opencde`. Engine ids are
  matched exactly, so `Claude_Code` counts as unknown too: adapters look the key
  up verbatim.
- **A real engine whose adapter doesn't read that map.** Each adapter declares
  which `native_<feature>` maps it consumes. `native_model_providers.claude_code`
  is dead because Claude Code is Anthropic-only with no provider block;
  `native_hooks.opencode` and `native_plugins.opencode` are dead because opencode
  renders hooks from the neutral `capabilities.hooks` through its shim and
  plugins from the resolved plugin list, never from a per-engine fragment.

The current matrix of which adapter reads which map:

| Map | `claude_code` | `crush` | `opencode` |
| --- | --- | --- | --- |
| `native_permissions` | yes | yes | yes |
| `native_hooks` | yes | yes | no |
| `native_plugins` | yes | no | no |
| `native_mcp` | yes | yes | yes |
| `native_model_providers` | no | yes | yes |
| `native` | yes | yes | yes |

`llmenv export`, `llmenv regenerate`, and `llmenv doctor` warn about both kinds,
reading the *merged* config so a key contributed by a `bundle.yaml` is covered
too. `llmenv validate` goes further and **fails** on an unknown engine id, since
there is no legitimate reason to write one; a key naming a real engine that
doesn't read the map stays a warning there, because sharing one config across
engines makes it a deliberate no-op.

`native_permissions` is keyed by engine like every other map — an MCP server name
is not a valid key. Per-MCP-server permissions are expressed as
`mcp__<server>__<tool>` rule strings under an engine key, or through
`features.<name>.mcp_permissions`.

```yaml
capabilities:
  permissions:
    default_mode: acceptEdits
    deny:
      - { tool: Read, paths: ["./.env", "./.env.*"] }
  native_permissions:
    claude_code:
      deny: ["WebFetch(domain:internal.example.com)"]
```

The neutral `{tool, pattern}` / `{tool, paths}` form covers the common case; the
adapter *generates* Claude's `Bash(...)` / `Read(...)` string grammar — you never
author it. `native_permissions` appends raw rule strings for the long tail.

For a given tool+pattern, `deny` always wins over `ask`/`allow` regardless of
whether it came from the structured `permissions:` block or the engine's
`native_permissions` override — a native `allow` can never silently unset a
structured `deny` for the same rule.

## The catch-all `native:` block

Separately, the top-level `native:` block is a per-engine catch-all for keys that
belong to **no modeled feature** (e.g. `alwaysThinkingEnabled`, `outputStyle`):

```yaml
native:
  claude_code:
    alwaysThinkingEnabled: true
```

It is overlaid onto the engine's config last. Putting a modeled-feature key
(`permissions`, `hooks`) here is a hard error — that belongs in the matching
`native_<feature>` sibling, so the security-rendered output is never silently
clobbered.

## What the Claude Code adapter emits

For each materialized environment, the adapter writes (all with `0600`
permissions):

| File | From |
| ------ | ------ |
| `CLAUDE.md` | the merged `AGENTS.md` / rules content — omitted entirely when that resolves to nothing (added in v3.10.0; earlier versions wrote a 0-byte file) |
| `settings.json` | permissions, hooks, plugins (+ `native_*` overrides, + `native:` catch-all) |
| `.claude.json` | resolved MCP servers upserted into `mcpServers`; foreign keys preserved (+ `native_mcp`) |
| `skills/llmenv-lsp/.claude-plugin/plugin.json` | `lsp:` entries with `extension_to_language` set, as a synthetic skills-directory plugin (#556) |

It also:

- sets `CLAUDE_CONFIG_DIR` to the materialized directory so Claude Code uses it;
- emits `autoMemoryEnabled: false` when the ICM memory server is present, so ICM
  and Claude's native auto-memory don't both write (a `native` override wins);
- registers a `SessionStart` hook running `llmenv check-stale` for drift
  detection.

## Where capabilities are declared

Capabilities can be declared at two levels with identical shape:

- **Globally** under `capabilities:` in `config.yaml`.
- **Per bundle** in an optional `bundle.yaml` inside the bundle's content
  directory — keeping a hook's script and its registration together so the bundle
  versions as a unit.

Contributors merge by value shape: scalars (like `default_mode`) resolve by
scope precedence (network → host → user → project); lists (allow/ask/deny, hooks,
plugins) concatenate and de-duplicate.

## The Crush adapter

(added in v3.0.0)

[Crush](https://github.com/charmbracelet/crush) is a second supported engine. It
is **PATH-gated**: `export`, `hook`, and `regenerate` skip Crush silently if
`crush` is not on `PATH`. When it is present, a separate `crush/` subtree is
materialized inside the llmenv cache directory.

### Env vars

| Variable               | Points to                                                              | Notes                                                                                          |
|------------------------|------------------------------------------------------------------------|------------------------------------------------------------------------------------------------|
| `CRUSH_GLOBAL_CONFIG`  | `<cache>/crush/...` (the directory containing `crush.json`)            | Crush joins `crush.json` onto this path itself — it must be a directory, not the file          |
| `CRUSH_GLOBAL_DATA`    | `<state_dir>/crush`                                                    | A dedicated subdir of the stable llmenv state dir; Crush needs no separate workaround          |

`CRUSH_GLOBAL_CONFIG` and `CLAUDE_CONFIG_DIR` use separate namespaces and can
coexist in a single shell session without conflict.

### Capability map

| Feature | Crush support | Notes |
| --------- | -------------- | ------- |
| Permissions (`allow`) | **Coarse — tool-level only** | An unscoped rule (no `pattern`/`paths`) is translated from llmenv's neutral tool vocabulary (`Bash`, `Read`, `WebFetch` — Claude Code's PascalCase names) to Crush's own tool identifiers (`bash`, `view`, `fetch`, ...) and rendered to `allowed_tools` (changed in v3.10.0 — previously rendered the neutral name verbatim, which never matched Crush's case-sensitive, differently-named tools; see [#1321](https://github.com/phaedrus1992/llmenv/issues/1321)). A neutral tool with no Crush equivalent (`Task`, `NotebookEdit`, ...) is dropped and logged, same as a `pattern`/`paths`-scoped rule below — the neutral permission list is shared across engines, so a Claude-Code-only tool name is a normal config, not a Crush-specific error. **`Edit`/`MultiEdit` also imply file creation** under Crush: its `edit`/`multiedit` tools create missing files and parent directories on an empty old-content diff, unlike Claude Code's `Edit`, which requires an existing path — allowing `Edit` for Crush is closer to allowing `Edit` **and** `Write` combined. A `pattern`/`paths`-scoped rule is dropped entirely rather than widened to a whole-tool grant, since Crush's matcher can't express scoping at all (changed in v3.10.0; see [#1306](https://github.com/phaedrus1992/llmenv/issues/1306)) |
| Permissions (`ask`/`deny`) | **No dedicated rendering, but cross-checked against `allow`** | Crush's `PermissionsConfig` has no `denied_tools`/`default_mode` concept, so `ask`/`deny` rules produce no key of their own. As of v3.10.0 they still suppress a same-tool entry in `allowed_tools` (Crush has nothing else to enforce a conflicting deny with) — before the `allow`-side name mapping landed, `allow` never matched a real Crush tool either, so this cross-check wasn't needed; see [#1321](https://github.com/phaedrus1992/llmenv/issues/1321) |
| Hooks — `PreToolUse` | Supported | `command`-kind handlers only |
| Hooks — other events | **Hard error** | Crush supports only `PreToolUse`; any other event in config is an error |
| Hooks — `mcp_tool` kind | **Hard error** | No Crush equivalent; use `command`-kind instead |
| MCP servers | Supported | Includes `headers`, `disabled_tools`, `timeout` |
| LSP servers | Supported | Rendered to `lsp.<name>` entries |
| Skills (first-class) | Supported | Written via `options.skills_paths` |
| Skills (plugin-projected) | Supported | Plugin `skills/` subdirs are projected into Crush's skill paths |
| Plugins / marketplace | **Hard error** | Crush has no plugin or marketplace concept; non-skill plugin content (custom `agents/`, `commands/`) produces an actionable error naming the plugin |
| Custom agents | **Unsupported** | Crush hardcodes exactly two agent roles (coder/task); `agents/*.md` from plugins cannot be loaded |
| Model providers (`model_providers`/`default_models`) | Supported | Rendered to `providers`/`models` using catwalk's field names; `api_type` passes through as `type` verbatim |

### The `native.crush` escape hatch

Keys that no modeled feature owns go under `native.crush`:

```yaml
native:
  crush:
    model: claude-opus-4-5
    provider: anthropic
```

`capabilities.model_providers`/`default_models` (see
[Configuration](configuration.md)) is the first-class, engine-agnostic home
for provider/model config — use this escape hatch only for Crush-specific
fields it doesn't cover. The fragment is deep-merged verbatim into
`crush.json` at highest precedence.

The `native_permissions.crush`, `native_hooks.crush`, `native_mcp.crush`, and
`native_model_providers.crush` siblings work the same way for their respective
domains.

### `native_model_providers.<engine>`

`capabilities.model_providers` models the fields every engine has in common,
but Crush and opencode each accept provider and per-model keys it has no field
for. `native_model_providers.<engine>` is the escape hatch: a raw fragment,
keyed by provider id, deep-merged onto the rendered provider block —
`providers` in `crush.json`, `provider` in `opencode.json`. The fragment is the
higher-precedence layer, so a key it sets wins over the one rendered from
`model_providers`; sibling keys are preserved.

It also works on its own — with no `model_providers` entries at all, the
fragment alone renders the provider block, so a hand-written provider survives
`llmenv regenerate`. The fragment must be a mapping; a scalar or list is
rejected with an error rather than replacing the whole rendered block. Don't
declare both a fragment and a `disabled: true` provider for the same id — the
fragment renders regardless.

:::caution Per-model keys: opencode only
Crush renders `models` as a list, so a fragment's model entry is appended, not
patched — use `model_providers[].models` for per-model config on Crush.
opencode renders it as an object keyed by model id, so patching works.
:::

```yaml
capabilities:
  model_providers:
    - id: mtplx
      base_url: http://localhost:8080/v1
      api_type: openai
      models:
        - { id: gpt-oss }
  native_model_providers:
    opencode:
      mtplx:
        models:
          gpt-oss:
            reasoningEffort: high   # no neutral equivalent — opencode-only
```

## The opencode adapter

(added in v3.6.1)

[opencode](https://opencode.ai) is a third supported engine. Like Crush it is
**PATH-gated**: `export`, `hook`, and `regenerate` skip opencode silently if
`opencode` is not on `PATH`. When present, opencode's config is materialized
into the llmenv cache directory and discovered via `OPENCODE_CONFIG_DIR`.

Unlike Crush, opencode is a full-featured target: it supports plugins, LSP,
custom agents/commands, and six hook events, so it reaches near-parity with the
Claude Code adapter.

### Env vars

| Variable              | Points to                                         | Notes                                                                         |
|-----------------------|---------------------------------------------------|-------------------------------------------------------------------------------|
| `OPENCODE_CONFIG_DIR` | `<cache>` (the directory holding `opencode.json`) | opencode reads `opencode.json`, `AGENTS.md`, and the `plugin/` shim from here |

### What the opencode adapter emits

| Output | Contents |
| ------ | -------- |
| `opencode.json` | `$schema` (points at the `opencode.schema.json` sidecar below), `instructions`, `mcp`, `lsp`, `permission`, `plugin` — structured render, then `native_*.opencode` overlays deep-merged at the value level |
| `opencode.schema.json` | JSON Schema (draft 2020-12) generated from the same typed structs that render `opencode.json`, so it always matches what llmenv actually writes. Root allows `additionalProperties`, so passthrough/native-overlay keys never fail IDE validation. |
| `AGENTS.md` | the merged rules document opencode loads as project instructions — omitted entirely when that resolves to nothing (added in v3.10.0; earlier versions wrote a 0-byte file) |
| `rules/*.md` | rule files copied verbatim and listed in `instructions` |
| skills (`SKILL.md`) | first-class and plugin-projected skills, in opencode's claude-compatible format |
| `command/*.md`, `agent/*.md` | plugin commands and agents translated (agents gain `mode: subagent`) |
| `plugin/llmenv.js` | a generated ES-module shim bridging opencode's JS plugin API to llmenv's `hook-run` subprocess |

### Capability map

| Feature | opencode support | Notes |
| --------- | ---------------- | ------- |
| Permissions (`allow`/`ask`/`deny`) | Supported for the documented neutral tool vocabulary | Rendered as per-tool `pattern → action` maps; a bare tool emits a plain action string. `ask` is native (no fail-closed collapse). The neutral tool name is mapped to opencode's own permission key, source-verified against opencode's `permission.ts` schema (`bash`, `read`, `glob`, `grep`, `webfetch`, `websearch`, `todowrite`, `task` are a straight lowercase; `Write`/`MultiEdit` both map to `edit`, `LS` maps to `list` — opencode has no separate `write`/`multiedit`/`ls` key). A neutral tool with no confirmed opencode equivalent is dropped with a logged warning rather than guessing at a key (fixed in v3.10.0, [#1326](https://github.com/phaedrus1992/llmenv/issues/1326)) |
| Hooks — `SessionStart`, `SessionEnd`, `UserPromptSubmit`, `PreToolUse`, `PostToolUse`, `Stop` | Supported | Bridged through the generated `plugin/llmenv.js` shim |
| Hooks — other events | **Warned, skipped** | Unsupported events are dropped with an actionable warning rather than a hard error |
| Hooks — `mcp_tool` kind | **Warned, skipped** | No opencode equivalent; use a `command`-kind handler |
| MCP servers | Supported | Local (`command`, `${HOME}`-expanded) and remote (`http`/`sse`) transports |
| LSP servers | Supported | Rendered to `lsp.<name>` entries, with `initialization_options` |
| Skills (first-class + plugin-projected) | Supported | Native `SKILL.md` format |
| Plugins / marketplace | Supported | Plugin commands, agents, MCP, skills, and hooks are translated |
| Custom agents | Supported | Plugin `agent/*.md` are emitted with `mode: subagent` |
| Model providers (`model_providers`/`default_models`) | Supported | Rendered to `provider.<id>` / `model` / `small_model`; `api_type` maps to the AI SDK `npm` package (e.g. `openai` → `@ai-sdk/openai-compatible`). `default_models` only has `large`/`small` slots — other role names are a no-op |

### The `native.opencode` escape hatch

Keys that no modeled feature owns go under `native.opencode`, deep-merged into
`opencode.json` at highest precedence:

```yaml
native:
  opencode:
    theme: opencode
```

The modeled keys `instructions`, `mcp`, `lsp`, `permission`, `provider`,
`model`, and `small_model` are **rejected** in the top-level `native.opencode`
block — overlaying them last would clobber the security-rendered output. Route
them through the sibling that merges in the safe direction instead:
`native_permissions.opencode`, `native_hooks.opencode`,
`native_mcp.opencode`, or
[`native_model_providers.opencode`](#native_model_providersengine) for
`provider`. `model` and `small_model` are plain `provider_id/model_id` strings
with no engine-specific extras — use `capabilities.default_models` for those.

## Other engines

The capability model is engine-neutral by design, so additional adapters (e.g.
Codex) can render the same neutral config into their own shape and expose their
own `native_*` overrides.
