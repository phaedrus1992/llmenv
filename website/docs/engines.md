<!-- markdownlint-disable MD013 -->

# Engines

llmenv emits agent-native configuration through pluggable **adapters**. The
configuration you write is engine-neutral; each adapter translates it into one
engine's native shape. Anything that can't be expressed neutrally drops through a
per-engine escape hatch.

Four adapters ship today: **Claude Code**, **Codex**, **Crush**, and
**opencode**. Each activates when its binary is on `PATH`; users who only have
one of those binaries see no output from the other adapters. The design doc behind this model is
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

| Map                      | `claude_code` | `codex` | `crush` | `opencode` |
|--------------------------|---------------|---------|---------|------------|
| `native_permissions`     | yes           | no      | yes     | yes        |
| `native_hooks`           | yes           | yes     | yes     | no         |
| `native_plugins`         | yes           | no      | no      | no         |
| `native_mcp`             | yes           | yes     | yes     | yes        |
| `native_model_providers` | no            | no      | yes     | yes        |
| `native`                 | yes           | yes     | yes     | yes        |

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

### The neutral tool vocabulary

(table consolidated in v3.11.1; the mappings themselves predate it)

`permissions[].tool` names a tool in one neutral vocabulary — Claude Code's
PascalCase tool names — and each adapter renders it in its own engine's grammar.
Claude Code receives the name as written. opencode and crush have closed key
sets, so their names are translated:

| Neutral tool | opencode key | crush tool | Not one-to-one? |
| ------------ | ------------ | ---------- | --------------- |
| `Bash` | `bash` | `bash` | |
| `Read` | `read` | `view` | |
| `Edit` | `edit` | `edit` | crush's edit tools also create missing files and parent directories, so allowing `Edit` on crush also allows file creation |
| `Write` | `edit` | `write` | opencode gates every file mutation through the single `edit` key, so a `Write` rule there also covers `Edit` |
| `MultiEdit` | `edit` | `multiedit` | opencode has no separate multi-edit key; same file-creation caveat as `Edit` on crush |
| `Glob` | `glob` | `glob` | |
| `Grep` | `grep` | `grep` | |
| `LS` | `list` | `ls` | |
| `WebFetch` | `webfetch` | `fetch` | crush's more specialized `agentic_fetch` is not used |
| `WebSearch` | `websearch` | — | crush's `web_search` is more specialized and isn't treated as a direct equivalent; the rule is dropped for crush |
| `TodoWrite` | `todowrite` | `todos` | |
| `Task` | `task` | — | dropped for crush |
| `Skill` | `skill` | — | dropped for crush |
| `NotebookEdit` | — | — | neither engine has an equivalent; the rule takes effect on Claude Code only |

A `—` means that engine has no analog llmenv will map to, so the rule is
**dropped** for that engine and takes effect on the others. This is deliberate:
guessing at a lowercase pass-through renders a key opencode's schema rejects
(which makes it discard the whole config file) or a name crush's exact-match
allowlist never matches. Use `native_permissions.<engine>` to target that
engine's own tool directly.

A tool name that isn't in this table is **not** rejected — Claude Code gains
tools llmenv has no reason to know about, and its adapter passes the name
straight through, so such a rule still works there. `export` and `regenerate`
report it once, because opencode and crush can only drop it. `llmenv doctor`
additionally lists any tool in your config whose mapping onto an active engine
falls in the "not one-to-one" column above.

## The catch-all `native:` block

Separately, the top-level `native:` block is a per-engine catch-all for keys that
belong to **no modeled feature** (e.g. `alwaysThinkingEnabled`, `outputStyle`):

```yaml
native:
  claude_code:
    alwaysThinkingEnabled: true
```

It is overlaid onto the engine's config last. `hooks` is a hard error here — it
belongs in the matching `native_<feature>` sibling, so the security-rendered
output is never silently clobbered.

### `native.claude_code.permissions`

(added in v4.0.0)

`permissions` used to be a hard error here too. It is now accepted and layered
over the rendered `permissions` object, which makes the catch-all the escape
hatch for Claude-Code-only permission keys llmenv doesn't model —
`additionalDirectories`, `disableBypassPermissionsMode`,
`skipDangerousModePermissionPrompt`, and whatever Claude Code ships next —
without waiting on a neutral-schema field and an llmenv release:

```yaml
native:
  claude_code:
    permissions:
      additionalDirectories: ["/srv/shared"]
      disableBypassPermissionsMode: "disable"
```

It is accepted because the merge is **additive, not a replacement**:

- `allow`, `ask`, and `deny` are appended to what was rendered and deduped —
  never replaced. A fragment that omits `deny`, or sets it to `null`, leaves the
  rendered `deny` intact.
- `deny > ask > allow` authority is re-applied afterwards, so a native `allow`
  of an already-denied rule is dropped rather than honoured.
- Every other key overwrites — those carry no rendered security decision to
  weaken.
- `defaultMode` is the exception and is **rejected here**. It is a modeled key
  (`capabilities.permissions.default_mode`), and setting it from the catch-all
  would override the rendered mode — including to `bypassPermissions`, which
  switches the permission system off entirely. Anything that can author a
  `native:` block, bundles included, would otherwise have a one-line escalation
  past every rendered `ask` and `deny`. Use the modeled field instead.

Rule strings here get the same `Write` → `Edit` normalization the
`native_permissions` sibling applies, so a `deny: ["Write(~/.ssh/**)"]` isn't
silently rendered as a rule that matches nothing.

The net effect is that a native fragment can tighten permissions or add keys
llmenv doesn't model, but cannot loosen what the renderer produced. `hooks`
keeps the hard error because it is an array of matcher groups, where "additive"
has no unambiguous meaning; use `native_hooks` for those.

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
- registers a `SessionStart` hook running `llmenv hook-run session_start`, which
  performs the drift check alongside memory wake-up (folded into one process in
  v3.11.0 — it was a separate `llmenv check-stale` hook before).

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
| Output styles | **Fallback — generated skill** | Crush has no output-style concept; `output_styles` entries render as `skills/<name>/SKILL.md` instead, same `name`/`description`/`content` (added in v3.10.0, [#1130](https://github.com/phaedrus1992/llmenv/issues/1130)) |
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

### Hooks

(per-turn hook parity added in v4.0.0, [#1439](https://github.com/phaedrus1992/llmenv/issues/1439))

llmenv wires its own hooks for opencode, the same baseline it gives Claude Code
and Codex: the config-source context and managed-cache write guard/read-once
dedup on `PreToolUse`, and the ICM memory/session-log lifecycle events on
`SessionStart`/`SessionEnd`. These route through `plugin/llmenv.js`, the
generated shim bridging opencode's JS plugin API to `llmenv hook-run`
subprocess calls — there's no nested matcher-group config to write, since
opencode dispatches by table entry in the shim itself.

The per-turn hooks are gated on configuration, exactly as they are for Claude
Code and Codex:

- `turn_start` on `UserPromptSubmit` — when a memory backend resolved for the
  scope (`features.memory`).
- `stop` on `Stop` — when `features.task_tracker` is enabled, or when
  `features.slippage` has `self_critique` on.
- `user_prompt_submit` on `UserPromptSubmit` — when `features.slippage` has
  `rule_reinjection` on and nothing else already claimed that event.
- The session-log turn capture set — `UserPromptSubmit`, `PreToolUse`,
  `PostToolUse`, `Stop` — when any `session_log` sink is enabled.

The session-log set is narrower than Claude Code's and Codex's: opencode has
no `Notification`, `SubagentStop`, or `PreCompact` event (see the supported
event list above), so none of the three are emitted — a shim table entry for
an event opencode never dispatches would look wired and never fire.
`llmenv doctor` reports which of these are wired for the active scope,
alongside Claude Code and Codex.

### Capability map

| Feature | opencode support | Notes |
| --------- | ---------------- | ------- |
| Permissions (`allow`/`ask`/`deny`) | Supported for the documented neutral tool vocabulary | Rendered as per-tool `pattern → action` maps; a bare tool emits a plain action string. `ask` is native (no fail-closed collapse). The neutral tool name is mapped to opencode's own permission key, source-verified against opencode's `permission.ts` schema (`bash`, `read`, `glob`, `grep`, `webfetch`, `websearch`, `todowrite`, `task`, `skill` are a straight lowercase; `Write`/`MultiEdit` both map to `edit`, `LS` maps to `list` — opencode has no separate `write`/`multiedit`/`ls` key). A neutral tool with no confirmed opencode equivalent is dropped rather than guessing at a key (fixed in v3.10.0, [#1326](https://github.com/phaedrus1992/llmenv/issues/1326)), with a `warning:` line naming the tool (fixed in v3.11.0, [#1345](https://github.com/phaedrus1992/llmenv/issues/1345) — before that it went to a log level nothing displayed, so the rule vanished silently). This mapping applies only to `capabilities.permissions`; `native_permissions.opencode` strings are opencode's own vocabulary already (`lsp`, `question`, `doom_loop`, `external_directory`, a bare `*` deny-all, ...) and are lowercased verbatim, never mapped. Because `Write` and `MultiEdit` collapse onto the same `edit` key as `Edit`, rules for any of the three now interact with each other under one shared key — allowing `Write` and denying `Edit` (or vice versa) resolves against the combined `edit` pattern map, not two independent tools. Two rule shapes opencode cannot represent are rejected at regeneration time rather than rendered (fixed in v3.11.0, [#1328](https://github.com/phaedrus1992/llmenv/issues/1328)) — see [Permission rules opencode cannot represent](#permission-rules-opencode-cannot-represent) |
| Hooks — `SessionStart`, `SessionEnd`, `UserPromptSubmit`, `PreToolUse`, `PostToolUse`, `Stop` | Supported | Bridged through the generated `plugin/llmenv.js` shim |
| Hooks — other events | **Warned, skipped** | Unsupported events are dropped with an actionable warning rather than a hard error |
| Hooks — `mcp_tool` kind | **Warned, skipped** | No opencode equivalent; use a `command`-kind handler |
| MCP servers | Supported | Local (`command`, `${HOME}`-expanded) and remote (`http`/`sse`) transports |
| LSP servers | Supported | Rendered to `lsp.<name>` entries, with `initialization_options` |
| Skills (first-class + plugin-projected) | Supported | Native `SKILL.md` format |
| Output styles | **Fallback — generated skill** | opencode has no output-style concept; `output_styles` entries render as `skills/<name>/SKILL.md` instead, same `name`/`description`/`content` (added in v3.10.0, [#1130](https://github.com/phaedrus1992/llmenv/issues/1130)) |
| Plugins / marketplace | Supported | Plugin commands, agents, MCP, skills, and hooks are translated |
| Custom agents | Supported | Plugin `agent/*.md` are emitted with `mode: subagent` |
| Model providers (`model_providers`/`default_models`) | Supported | Rendered to `provider.<id>` / `model` / `small_model`; `api_type` maps to the AI SDK `npm` package (e.g. `openai` → `@ai-sdk/openai-compatible`). `default_models` only has `large`/`small` slots — other role names are a no-op |

### Permission rules opencode cannot represent

(added in v3.11.0)

Two permission shapes have no faithful rendering in `opencode.json`. Both used
to be emitted anyway and then silently misbehave, so `llmenv regenerate` now
fails with an error naming the offending rules instead.

**Scoped rules on an action-only key.** opencode types `todowrite`,
`question`, `webfetch`, `websearch`, and `doom_loop` as a bare
`"allow"`/`"ask"`/`"deny"` string — unlike `bash`, `read`, `edit`, and the
rest, they take no `pattern → action` map. opencode discards the *entire*
config file when any single key fails to decode, and reports nothing, so one
scoped rule here used to void every MCP server, LSP entry, and permission rule
in the file:

```yaml
capabilities:
  permissions:
    allow:
      - { tool: WebFetch, pattern: "https://example.com/*" }   # rejected
      - { tool: WebFetch }                                     # fine — covers the whole tool
```

Drop the `pattern`/`paths` so the rule covers the tool as a whole.

**Two overlapping patterns where the later-sorting one isn't the narrower.**
opencode applies the *last* matching rule in config key order, and llmenv emits
each tool's pattern map sorted by pattern. So for any two patterns that can
match the same input, whichever sorts last governs every input they share.
That's only what you meant if the last one is the more specific of the two:

```yaml
capabilities:
  permissions:
    allow:
      - { tool: Bash, pattern: "git *" }        # sorts after "* --force*"…
    deny:
      - { tool: Bash, pattern: "* --force*" }   # …so this never applied to "git push --force"
```

Writing a deny as a leading-`*` pattern to mean "anywhere in the command" is
the case that bites: `*` sorts before letters, so the deny lands first and the
allow wins on everything they share. Rewrite the later pattern so it only
covers inputs the earlier one doesn't, or give the two the same action.

The common shape — a wildcard baseline plus a narrower override — is
unaffected, because a pattern that fully contains another is exempt:

```yaml
capabilities:
  permissions:
    allow:
      - { tool: Bash }                       # renders as "*"
    deny:
      - { tool: Bash, pattern: "git push*" } # narrower, sorts last, still wins
```

This comparison spans permission *keys*, not just patterns within one key
(added in v3.11.0, [#1344](https://github.com/phaedrus1992/llmenv/issues/1344)).
opencode flattens every key into one ordered rule list and wildcard-matches the
key against the tool name as well, so a native rule keyed `*` applies to every
tool and is checked against the concrete keys that sort after it:

```yaml
capabilities:
  native_permissions:
    opencode:
      deny: ["*(git push --force*)"]   # key "*" sorts before "bash"…
  permissions:
    allow:
      - { tool: Bash }                 # …so this allow won for "git push --force"
```

A bare native `*` deny-all baseline is still fine alongside per-tool rules —
it's broader in both key and pattern, so the narrower per-tool rule winning is
what you asked for:

```yaml
capabilities:
  native_permissions:
    opencode:
      deny: ["*"]        # deny everything by default
  permissions:
    allow:
      - { tool: Bash }   # …except bash
```

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

## The Codex adapter

(added in v4.0.0)

[Codex](https://github.com/openai/codex) is a supported engine, PATH-gated the
same way as Crush and opencode: `export`, `hook`, and `regenerate` skip it
silently when `codex` is not on `PATH`.

This is the **first slice** of Codex parity
([#233](https://github.com/phaedrus1992/llmenv/issues/233)) — MCP servers and the
merged `AGENTS.md`. What isn't wired yet is listed below, with its tracking
issue, so nothing here is a silent gap.

### Env vars

| Variable     | Points to                                                    | Notes                                                                           |
|--------------|--------------------------------------------------------------|---------------------------------------------------------------------------------|
| `CODEX_HOME` | `<cache>/codex/...` (the directory containing `config.toml`) | Codex's analogue of `CLAUDE_CONFIG_DIR`; it must be the directory, not the file |

### What the Codex adapter emits

`config.toml`, in Codex's own TOML config format:

- **`mcp_servers`** — one table per resolved MCP server. A stdio server renders
  `command`/`args`/`env`; a streamable-HTTP server renders `url` (plus
  `http_headers`). There is deliberately **no `type` key**: Codex reads the
  transport from which key is present, and has no `type` field at all. (It would
  be ignored rather than rejected — Codex tolerates unknown keys — but writing a
  key the engine never reads is how a config drifts out of sync with reality.) A
  per-server `timeout` maps to `tool_timeout_sec`, since
  llmenv's timeout is a request timeout and Codex's `startup_timeout_sec` covers
  initialization instead.
- **`model_instructions_file`** — an absolute path to the merged `AGENTS.md`,
  which is written alongside `config.toml`. Codex finds a *project's* AGENTS.md
  on its own; this pointer is for llmenv's merged copy, which lives in the cache
  directory rather than a project root.

### SSE MCP servers are skipped

Codex speaks stdio and streamable HTTP. It has no SSE transport at all, so an
MCP server declared with `transport: sse` is skipped for Codex with a warning
rather than rendered as a `url` — which Codex would read as streamable HTTP and
then fail to talk to. Other engines still receive the server.

### Hooks

Codex takes the same nested matcher-group shape as Claude Code, under
`hooks.events.<Event>`, and its event names match — so llmenv's engine-neutral
hooks map across without a translation layer:

```toml
[[hooks.events.PreToolUse]]
matcher = "Bash"

[[hooks.events.PreToolUse.hooks]]
type = "command"
command = "…"
```

Two things are skipped with a warning rather than rendered:

- **An event Codex doesn't have.** `Notification` is the live case — Claude Code
  has it, Codex doesn't. Codex ignores unknown keys, so emitting it anyway would
  leave a hook that looks wired and never fires.
- **`mcp_tool` handlers.** Codex hooks run commands only.

llmenv also wires its own hooks for Codex, the same set it gives Claude Code
(added in v4.0.0): the config-source context at `SessionStart`, the managed-cache
write guard and read-once dedup on `PreToolUse`, the ICM memory and session-log
lifecycle events on `SessionStart`/`SessionEnd`, and the throttle hooks when a
throttle is configured.

The per-turn hooks are gated on configuration, exactly as they are for Claude
Code (added in v4.0.0):

- `turn_start` on `UserPromptSubmit` — when a memory backend resolved for the
  scope (`features.memory`). It runs on every prompt, so it stays off for a
  scope with no memory configured.
- `stop` on `Stop` — when `features.task_tracker` is enabled, or when
  `features.slippage` has `self_critique` on.
- `user_prompt_submit` on `UserPromptSubmit` — when `features.slippage` has
  `rule_reinjection` on and nothing else already claimed that event.
- The session-log turn capture set — `UserPromptSubmit`, `PreToolUse`,
  `PostToolUse`, `Stop`, `SubagentStop`, `PreCompact` — when any `session_log`
  sink is enabled.

The session-log set is Claude Code's minus `Notification`, for the reason above:
Codex has no such event, so emitting it would leave a hook that looks wired and
never fires. `llmenv doctor` reports which of these are wired for the active
scope.

Those point at `llmenv hook-run --engine codex`, which works because Codex reads
the same hook output shape Claude Code does — `hookSpecificOutput` carrying
`hookEventName` and `additionalContext` — so injected context reaches the model
without a translation layer.

### Capability map

| Capability                       | Status                                                                                                                                                                                 |
| -------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| MCP servers                      | rendered                                                                                                                                                                               |
| Merged `AGENTS.md`               | rendered, via `model_instructions_file`                                                                                                                                                |
| Permissions                      | filesystem access only (added in v4.0.0, [#1102](https://github.com/phaedrus1992/llmenv/issues/1102)) — see [Permissions](#permissions)                                                |
| Lifecycle hooks                  | rendered, including llmenv's own baseline hooks                                                                                                                                        |
| Seeded settings                  | applied to `config.toml` (added in v4.0.0, [#1107](https://github.com/phaedrus1992/llmenv/issues/1107)) — see [Seeded settings](#seeded-settings)                                      |
| Install-method seed              | n/a — Codex self-detects its own install method in-process ([#1107](https://github.com/phaedrus1992/llmenv/issues/1107))                                                               |
| Statusline                       | n/a — no external-command hook exists (added in v4.0.0, [#1104](https://github.com/phaedrus1992/llmenv/issues/1104)) — see [Statusline](#statusline)                                   |
| Session/history/auth inheritance | inherited across hash changes (added in v4.0.0, [#1105](https://github.com/phaedrus1992/llmenv/issues/1105)) — see [Durable state inheritance](#durable-state-inheritance)             |
| SQLite state DBs                 | not yet ([#1420](https://github.com/phaedrus1992/llmenv/issues/1420)) — `state`/`logs`/`goals`/`memories`/`queue`/`thread-history` databases under `$CODEX_HOME`                       |
| Plugins                          | n/a — verified-absent from Codex's own source, no analogue of `installed_plugins.json` (added in v4.0.0, [#1106](https://github.com/phaedrus1992/llmenv/issues/1106))                  |
| LSP                              | n/a — verified-absent from Codex's own source, no `[lsp]`/`Lsp` config surface (added in v4.0.0, [#1106](https://github.com/phaedrus1992/llmenv/issues/1106))                          |
| Skills                           | first-class + built-in `llmenv` skill, registered via `[[skills.config]]` (added in v4.0.0, [#1106](https://github.com/phaedrus1992/llmenv/issues/1106)) — see [Skills](#skills)       |
| Rules beyond merged AGENTS.md    | folded into `AGENTS.md` (added in v4.0.0, [#1103](https://github.com/phaedrus1992/llmenv/issues/1103)) — see [Rules](#rules)                                                           |
| `doctor` diagnostics             | permission profile status, SSE MCP servers, `config.toml` validity (added in v4.0.0, [#1100](https://github.com/phaedrus1992/llmenv/issues/1100)) — see [`doctor`](commands.md#doctor) |

### Seeded settings

(added in v4.0.0)

`init.seeded_settings` (see [`init:`](configuration.md#init) for Claude Code's
version of this feature) is merged into `config.toml` the same way:
once a key is present — llmenv's own render, a prior seed, or a value Codex
itself wrote — it is left alone. Seeding never touches a
[modeled key](#the-nativecodex-escape-hatch) (`mcp_servers`,
`model_instructions_file`, `hooks`, `permissions`, `default_permissions`,
`skills`); those are llmenv's own render surface.

A security-sensitive key — `approval_policy`, `sandbox_mode`,
`sandbox_workspace_write`, `trusted_projects`, `shell_environment_policy` — is
refused with a warning rather than seeded, even though none of them are
modeled keys. [Permissions](#permissions) only render as far as filesystem
access ([#1102](https://github.com/phaedrus1992/llmenv/issues/1102)) —
`approval_policy`/`sandbox_mode` remain unmodeled — so a seeded value there
could silently run Codex less restrictively than the posture
`capabilities.permissions` establishes on every other engine.

Codex needs no install-method seed the way Claude Code does: it detects its
own install method (`brew`, `npm`, standalone, …) in-process from its own
executable path, so there is no config key for llmenv to pre-seed.

### Statusline

(added in v4.0.0)

Claude Code's `statusLine` runs an external command and displays whatever it
prints, which is what lets llmenv seed `llmenv statusline` there. Codex's
`tui.status_line` is structurally different: a fixed, ordered list of built-in
item identifiers (`model-with-reasoning`, `git-branch`, `context-remaining`,
`five-hour-limit`, …) rendered natively by Codex's own TUI. There is no
"run a command and show its output" surface, so `llmenv statusline` has
nothing to attach to on Codex — this is a structural gap, not missing work.

### Durable state inheritance

(added in v4.0.0)

`CODEX_HOME` is llmenv's hashed cache dir, so anything Codex persists under it
— session transcripts, prompt history, the cached login — would otherwise be
lost on every config edit or version bump, the same problem Claude Code's
`/resume` transcripts have. llmenv relocates the same way:

- **`sessions/`** and **`archived_sessions/`** — Codex's transcript stores,
  the direct analogue of Claude Code's `projects/`. Relocated to the durable
  state dir and symlinked back in, so `/resume` history survives a hash
  change instead of a copy-per-hash.
- **`history.jsonl`** — Codex's prompt-recall file, copied in when a folder
  has none and never overwritten once one exists.
- **`auth.json`** — Codex's combined identity + OAuth token file. Copied in
  when a folder has none, same as `history.jsonl`, but capture uses a
  newest-`mtime`-wins rule instead of "only when the store has none": a
  re-login or token rotation replaces the store's copy, rather than pinning
  the first-ever captured credential forever and serving a stale or revoked
  token to every new folder indefinitely.

Every file this relocates is copied with permissions forced to owner-only
(`0o600`) regardless of the source's mode — `std::fs::copy` otherwise
propagates whatever mode the source had, which would carry a looser umask
into the durable store or a fresh folder alike.

Codex also writes six SQLite databases directly into `$CODEX_HOME`
(`state`/`logs`/`goals`/`memories`/`queue`/`thread-history`). Those are **not**
covered yet — symlinking or copying a live SQLite file risks corruption via its
`-wal`/`-shm` sidecar files, and deserves its own design pass. Tracked in
[#1420](https://github.com/phaedrus1992/llmenv/issues/1420).

### Permissions

(added in v4.0.0)

Codex models permissions as named `[permissions.<name>]` profiles, selected by
setting `default_permissions` to the profile's name — a profile that is never
selected is dead config, since nothing else applies it. A profile bundles
`filesystem` access (per-path `read`/`write`/`deny`) and `network` access
together; only `filesystem` is rendered here.

`capabilities.permissions` rules map onto a `[permissions.llmenv]` profile,
filesystem only:

- `Read` → `read`, `Edit`/`Write`/`MultiEdit` → `write`, each applied to the
  rule's `paths`.
- A `deny` rule wins over an `allow` rule at the same path, and `write` wins
  over `read` — Codex's own stated precedence (`deny` beats `write` beats
  `read`), so a path covered by more than one rule resolves the same way Codex
  itself would.
- `default_permissions = "llmenv"` is set alongside the profile, so it's
  actually applied rather than merely defined.

This is **all-or-nothing per config**. Codex's permission profiles have no
per-command allowlist (a `Bash` rule has nothing to map to) and no per-rule
`ask` posture (only the global `approval_policy`/`sandbox_mode` — `ask`-tier
rules are unconditionally unmappable regardless of tool). Rendering the
mappable `Read`/`Edit`/`Write`/`MultiEdit` subset while silently dropping a
`Bash`/`WebFetch`/`ask` rule would produce a profile that looks more complete
than it is — a config carrying an unrendered `deny` should never *look*
enforced. So a single unmappable rule anywhere in `allow`/`ask`/`deny` refuses
the **whole** profile: nothing renders, and a warning explains why (also
surfaced proactively by [`doctor`](commands.md#doctor), without needing an
`export`/`regenerate` run first).

`network.domains` stays unmodeled even for a rendering-eligible config.
Meaningfully rendering it also requires modeling `network.enabled`/
`network.mode` — Codex's network proxy is off by default under
`workspace-write`, so a domain entry alone can be dead config — a bigger,
separate sandbox/network-vocabulary gap this slice doesn't take on.
`approval_policy`/`sandbox_mode` remain unmodeled too; Codex's permission
profiles intersect with (never replace) those. See
[#1102](https://github.com/phaedrus1992/llmenv/issues/1102).

### Rules

(added in v4.0.0)

Codex has no `rules/*.md`-with-glob-frontmatter convention the way Claude Code
and opencode do — no per-file rule mechanism at all, conditional or otherwise.
`capabilities.rules` bodies (frontmatter stripped) fold into the same merged
content `model_instructions_file` points at, each preceded by a provenance
comment naming its source bundle and file. This is a lossy transform: a rule's
path-scoped, conditional application on Claude Code and opencode becomes
unconditional `AGENTS.md` prose on Codex. See
[#1103](https://github.com/phaedrus1992/llmenv/issues/1103).

### Skills

(added in v4.0.0)

Codex skills use the same `SKILL.md` convention (name + description
frontmatter, plus body) that Claude Code does, so llmenv's existing
`SKILL.md` validation carries over unchanged. Unlike Claude Code, Codex has
no auto-discovery for a `skills/` directory — each skill folder needs an
explicit `[[skills.config]]` entry naming its absolute `path` with
`enabled = true`, or Codex never sees it.

First-class skills (`capabilities.skills`) and the built-in `llmenv` skill are
written under `out/skills/` the same way as for Claude Code, then every
subdirectory found there is registered — scanning the materialized directory,
rather than tracking each skill by name through the several code paths that
can write one, so a skill can never go unregistered just because a future
writer forgot to also update a name list. See
[#1106](https://github.com/phaedrus1992/llmenv/issues/1106).

Plugin-installation metadata (an analogue of `installed_plugins.json`) and LSP
config are verified-absent from Codex's own source — not deferred work, and
not expected to land later without Codex itself gaining the surface.

### The `native.codex` escape hatch

`native.codex` merges arbitrary keys into `config.toml` — useful for anything
llmenv doesn't model yet (`model`, `approval_policy`, `sandbox_mode`, …).
`mcp_servers`, `model_instructions_file`, `hooks`, `permissions`,
`default_permissions`, and `skills` are rejected there, because each would
clobber a block this adapter renders itself; use `native_mcp.codex` /
`native_hooks.codex` to merge additively into the corresponding rendered
block instead.

A `null` value deletes the key it targets, matching the other adapters. That
matters more here than elsewhere: TOML has no null, so an unstripped one would
fail the whole render rather than removing a key.

## Other engines

The capability model is engine-neutral by design, so additional adapters can
render the same neutral config into their own shape and expose their own
`native_*` overrides.
