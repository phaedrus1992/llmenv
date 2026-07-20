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
   `native_mcp`.

A feature with only layer 1 is considered incomplete — there is always some
platform-specific need (a Claude-only permission grammar, a Codex-only hook
event) that requires the override.

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
| `CLAUDE.md` | the merged `AGENTS.md` / rules content |
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

[Crush](https://github.com/nicholasgasior/crush) is a second supported engine. It
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
| Permissions (`allow`/`deny`) | Supported | Rendered to `allowed_tools` / `denied_tools` |
| Permissions (`ask`) | **Lossy, fail-closed** | `ask` rules collapse to `denied_tools` — Crush has no interactive-ask concept |
| Hooks — `PreToolUse` | Supported | `command`-kind handlers only |
| Hooks — other events | **Hard error** | Crush supports only `PreToolUse`; any other event in config is an error |
| Hooks — `mcp_tool` kind | **Hard error** | No Crush equivalent; use `command`-kind instead |
| MCP servers | Supported | Includes `headers`, `disabled_tools`, `timeout` |
| LSP servers | Supported | Rendered to `lsp.<name>` entries |
| Skills (first-class) | Supported | Written via `options.skills_paths` |
| Skills (plugin-projected) | Supported | Plugin `skills/` subdirs are projected into Crush's skill paths |
| Plugins / marketplace | **Hard error** | Crush has no plugin or marketplace concept; non-skill plugin content (custom `agents/`, `commands/`) produces an actionable error naming the plugin |
| Custom agents | **Unsupported** | Crush hardcodes exactly two agent roles (coder/task); `agents/*.md` from plugins cannot be loaded |

### The `native.crush` escape hatch

Keys that no modeled feature owns go under `native.crush`:

```yaml
native:
  crush:
    model: claude-opus-4-5
    provider: anthropic
```

This is the current home for provider/model configuration — first-class
provider config is tracked in #508. The fragment is deep-merged verbatim into
`crush.json` at highest precedence.

The `native_permissions.crush`, `native_hooks.crush`, and `native_mcp.crush`
siblings work the same way for their respective domains.

## The opencode adapter

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
| `opencode.json` | `$schema`, `instructions`, `mcp`, `lsp`, `permission`, `plugin` — structured render, then `native_*.opencode` overlays deep-merged at the value level |
| `AGENTS.md` | the merged rules document opencode loads as project instructions |
| `rules/*.md` | rule files copied verbatim and listed in `instructions` |
| skills (`SKILL.md`) | first-class and plugin-projected skills, in opencode's claude-compatible format |
| `command/*.md`, `agent/*.md` | plugin commands and agents translated (agents gain `mode: subagent`) |
| `plugin/llmenv.js` | a generated ES-module shim bridging opencode's JS plugin API to llmenv's `hook-run` subprocess |

### Capability map

| Feature | opencode support | Notes |
| --------- | ---------------- | ------- |
| Permissions (`allow`/`ask`/`deny`) | Supported | Rendered as per-tool `pattern → action` maps; a bare tool emits a plain action string. `ask` is native (no fail-closed collapse) |
| Hooks — `SessionStart`, `SessionEnd`, `UserPromptSubmit`, `PreToolUse`, `PostToolUse`, `Stop` | Supported | Bridged through the generated `plugin/llmenv.js` shim |
| Hooks — other events | **Warned, skipped** | Unsupported events are dropped with an actionable warning rather than a hard error |
| Hooks — `mcp_tool` kind | **Warned, skipped** | No opencode equivalent; use a `command`-kind handler |
| MCP servers | Supported | Local (`command`, `${HOME}`-expanded) and remote (`http`/`sse`) transports |
| LSP servers | Supported | Rendered to `lsp.<name>` entries, with `initialization_options` |
| Skills (first-class + plugin-projected) | Supported | Native `SKILL.md` format |
| Plugins / marketplace | Supported | Plugin commands, agents, MCP, skills, and hooks are translated |
| Custom agents | Supported | Plugin `agent/*.md` are emitted with `mode: subagent` |

### The `native.opencode` escape hatch

Keys that no modeled feature owns go under `native.opencode`, deep-merged into
`opencode.json` at highest precedence:

```yaml
native:
  opencode:
    theme: opencode
    model: anthropic/claude-opus-4-5
```

The modeled keys `instructions`, `mcp`, `lsp`, and `permission` are **rejected**
in the top-level `native.opencode` block — overlaying them last would clobber the
security-rendered output. Route those through the `native_permissions.opencode`,
`native_hooks.opencode`, and `native_mcp.opencode` siblings instead, which merge
in the safe direction.

## Other engines

The capability model is engine-neutral by design, so additional adapters (e.g.
Codex) can render the same neutral config into their own shape and expose their
own `native_*` overrides.
