<!-- markdownlint-disable MD013 -->

# Engines

llmenv emits agent-native configuration through pluggable **adapters**. The
configuration you write is engine-neutral; each adapter translates it into one
engine's native shape. Anything that can't be expressed neutrally drops through a
per-engine escape hatch.

Three adapters ship today: **Claude Code**, **Crush**, and **opencode**. Each activates when its
binary is on `PATH`; users who only have one binary on PATH see no output from the
other adapters. The design doc behind this model is
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

### The neutral tool vocabulary

(table consolidated in v3.11.1; the mappings themselves predate it)

`permissions[].tool` names a tool in one neutral vocabulary — Claude Code's
PascalCase tool names — and each adapter renders it in its own engine's grammar.
Claude Code receives the name as written. opencode and crush have closed key
sets, so their names are translated:

| Neutral tool | opencode key | crush tool | Not one-to-one? |
| ------------ | ------------ | ---------- | --------------- |
| `Bash` | `shell` | `bash` | |
| `Read` | `read` | `view` | |
| `Edit` | `edit` | `edit` | crush's edit tools also create missing files and parent directories, so allowing `Edit` on crush also allows file creation |
| `Write` | `edit` | `write` | opencode gates every file mutation through the single `edit` key, so a `Write` rule there also covers `Edit` |
| `MultiEdit` | `edit` | `multiedit` | opencode has no separate multi-edit key; same file-creation caveat as `Edit` on crush |
| `Glob` | `glob` | `glob` | |
| `Grep` | `grep` | `grep` | |
| `LS` | — | `ls` | opencode 2.x has no list tool; the rule is dropped for opencode |
| `WebFetch` | `webfetch` | `fetch` | crush's more specialized `agentic_fetch` is not used |
| `WebSearch` | `websearch` | — | crush's `web_search` is more specialized and isn't treated as a direct equivalent; the rule is dropped for crush |
| `TodoWrite` | — | `todos` | opencode 2.x has no todo tool; the rule is dropped for opencode |
| `Task` | `subagent` | `agent` | crush's `agent` tool is read-only, so an allow rule for `Task` on crush grants only read-only subagent work (added in v3.14.0) |
| `AskUserQuestion` | `question` | `question` | crush has the `question` tool from v0.85.0; an allow rule matches nothing on older crush (added in v3.14.0) |
| `Skill` | `skill` | — | dropped for crush |
| `NotebookEdit` | — | — | neither engine has an equivalent; the rule takes effect on Claude Code only |

A `—` means that engine has no analog llmenv will map to, so the rule is
**dropped** for that engine and takes effect on the others. This is deliberate:
a guessed name is a rule that never matches a tool call.
Use `native_permissions.<engine>` to target that engine's own tool directly.
The opencode column follows opencode 2.x (changed in v3.13.0: `Bash` was `bash`, `Task` was `task`, and `LS` and `TodoWrite` mapped to 1.x tools that 2.x removed).

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
- emits `syncClaudeAiSkills: false` and `syncClaudeAiPlugins: false` (added in
  v3.12.0) — since Claude Code 2.1.275, a terminal session signed in with a
  claude.ai account downloads the skills and plugins enabled on that account
  and loads them into every scope, going around llmenv's own scope rules and
  risking a name collision with an llmenv-managed skill or plugin. A `native`
  override (e.g. `native.claude_code.syncClaudeAiSkills: true`) re-enables
  sync; on the first render after upgrade, Claude Code moves any already-synced
  skills/plugins into `skills/.trash/` and `plugins/.trash/` inside the
  rendered config dir rather than deleting them. `disableClaudeAiConnectors`
  (claude.ai MCP connectors) is a separate setting and is not touched by this
  default — set `native.claude_code.disableClaudeAiConnectors: true` directly
  if you want connectors off too;
- registers a `SessionStart` hook running `llmenv hook-run session_start`, which
  performs the drift check alongside memory wake-up (folded into one process in
  v3.11.0 — it was a separate `llmenv check-stale` hook before);
- registers `SessionEnd`, and (added in v3.12.0) `PostModelSwitch` running
  `llmenv hook-run post_model_switch` — see [Hook events](#hook-events-claude-code);
- renders `alwaysLoad` for an MCP server that sets `always_load` (added in v3.12.0; opencode and
  Crush have no such key and ignore the field).

### Skipping CLAUDE.md in a subagent (Claude Code)

(added in v3.12.0)

`omitClaudeMd: true` in an agent's frontmatter makes Claude Code start that subagent without the user, project and local `CLAUDE.md` files.
Under llmenv the user `CLAUDE.md` is the rendered file with every bundle rule, so such a subagent runs without llmenv's rules.
Use it only for narrow agents that get everything they need from the delegation prompt, such as report-only analyzers.
Other engines ignore the field; opencode prints a warning and drops it.

### Hook events (Claude Code)

(added in v3.12.0)

The Claude Code adapter registers these `hook-run` events by itself.
`SessionStart`, `SessionEnd`, and `PostModelSwitch` are always on.
The memory events need the ICM memory server: `UserPromptSubmit` (`turn_start`), and, with `adaptive_recall` on, `PostToolBatch`, `PostToolUseFailure`, and `SubagentStart`.

`PostModelSwitch` fires when you change the model in a session (Claude Code 2.1.251 and later).
It runs `llmenv hook-run post_model_switch`, which updates the `model` field of the session's agent-config document.
At `SessionStart`, llmenv writes that document to the state dir (engine, model, effort, project, tags, and config hash).
A resumed or compacted session starts with a one-line `[llmenv session]` summary of it.
See [Agent config](commands.md#agent-config).
No other engine has this event, so only Claude Code sessions keep an agent-config document.

See [Which hooks each engine supports](#which-hooks-each-engine-supports) for the other events.

## Which hooks each engine supports

(added in v3.12.0)

| Event | Claude Code | opencode | Crush |
| --- | --- | --- | --- |
| `SessionStart`, `SessionEnd` | yes | yes | no |
| `UserPromptSubmit` | yes | yes | no |
| `PreToolUse` | yes | yes | yes |
| `PostToolUse` | yes | yes | no |
| `Stop` | yes | yes | no |
| `Notification`, `SubagentStop`, `PreCompact` | yes | yes (added in v3.14.0) | no |
| `PostToolUseFailure` | yes | yes (added in v3.14.0) | no |
| `PostToolBatch`, `SubagentStart` | yes | yes (added in v3.14.0) | no |
| `PostModelSwitch` | yes | yes (added in v3.14.0) | no |

An event that an engine does not support is dropped with a warning on opencode and is a hard error on Crush.
Crush runs only `command`-kind `PreToolUse` handlers.

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

| Variable                       | Points to                                         | Notes                                                                                                            |
|--------------------------------|---------------------------------------------------|------------------------------------------------------------------------------------------------------------------|
| `OPENCODE_CONFIG_DIR`          | `<cache>` (the directory holding `opencode.json`) | opencode reads `opencode.json`, `AGENTS.md`, `service.json`, and the `plugin/` folder from here                  |
| `OPENCODE_DISABLE_CLAUDE_CODE` | `1`                                               | opencode does not load skills from `~/.claude` or a project `.claude` folder (added in v3.13.0)                  |
| `TMPDIR`, `TMP`, `TEMP`        | `<state>/tmp`                                     | temp files of opencode and its subprocesses stay in llmenv's state folder, as for Claude Code (added in v3.13.0) |

`CRUSH_GLOBAL_CONFIG` and `CLAUDE_CONFIG_DIR` use separate namespaces and can
coexist in a single shell session without conflict.

### Capability map

| Feature | Crush support | Notes |
| --------- | -------------- | ------- |
| Permissions (`allow`) | **Coarse — tool-level only** | An unscoped rule (no `pattern`/`paths`) is translated from llmenv's neutral tool vocabulary (`Bash`, `Read`, `WebFetch` — Claude Code's PascalCase names) to Crush's own tool identifiers (`bash`, `view`, `fetch`, ...) and rendered to `allowed_tools` (changed in v3.10.0 — previously rendered the neutral name verbatim, which never matched Crush's case-sensitive, differently-named tools; see [#1321](https://github.com/phaedrus1992/llmenv/issues/1321)). A neutral tool with no Crush equivalent (`NotebookEdit`, `Skill`, ...) is dropped and logged, same as a `pattern`/`paths`-scoped rule below — the neutral permission list is shared across engines, so a Claude-Code-only tool name is a normal config, not a Crush-specific error. **`Edit`/`MultiEdit` also imply file creation** under Crush: its `edit`/`multiedit` tools create missing files and parent directories on an empty old-content diff, unlike Claude Code's `Edit`, which requires an existing path — allowing `Edit` for Crush is closer to allowing `Edit` **and** `Write` combined. A `pattern`/`paths`-scoped rule is dropped entirely rather than widened to a whole-tool grant, since Crush's matcher can't express scoping at all (changed in v3.10.0; see [#1306](https://github.com/phaedrus1992/llmenv/issues/1306)) |
| Permissions (`ask`/`deny`) | **No dedicated rendering, but cross-checked against `allow`** | Crush's `PermissionsConfig` has no `denied_tools`/`default_mode` concept, so `ask`/`deny` rules produce no key of their own. As of v3.10.0 they still suppress a same-tool entry in `allowed_tools` (Crush has nothing else to enforce a conflicting deny with) — before the `allow`-side name mapping landed, `allow` never matched a real Crush tool either, so this cross-check wasn't needed; see [#1321](https://github.com/phaedrus1992/llmenv/issues/1321) |
| Permissions (`deny`, unscoped) | Supported | Rendered to `options.disabled_tools`, which removes the tool from the agent (added in v3.14.0). A deny on `Edit` also disables `multiedit`, and a deny on `WebFetch` also disables `agentic_fetch`. A deny on `Write` withholds the `edit` and `multiedit` grants, because they also create files. `native_permissions.crush.deny` entries are added to `disabled_tools`. A `native.crush` fragment may not set `options.disabled_tools`, and llmenv rejects it. A `pattern`/`paths`-scoped deny is not rendered and is logged. The tool is withheld from `allowed_tools`, so it prompts. |
| Hooks — `PreToolUse` | Supported | `command`-kind handlers only. llmenv's own guards also run on every tool call: the adapter registers `llmenv hook-run --engine crush pre_tool_use`, and hook-run maps Crush's tool names (`bash`, `view`) to neutral names (`Bash`, `Read`) before the guards match (added in v3.14.0). A Crush tool with no neutral name, such as `agentic_fetch` or `download`, gets no llmenv guard. |
| Bundle env (`capabilities.env`) | Supported | Rendered to the top-level `env` key (added in v3.14.0). Crush expands each value as a shell word at startup, so llmenv escapes `\\`, `$`, and backticks. Crush reads each value literally, as the other engines do. Crush before v0.88.0 ignores the key. |
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
| Global instructions (`agents_md`) | Supported | Written to `CRUSH.md` in the Crush config dir, which Crush loads as global context (added in v3.14.0) |

### Env values escape `$` (changed in v3.15.0)

Crush expands each `env` value as a shell word at startup.
llmenv escapes `$`, backticks, and backslashes in the top-level `env`, and in each MCP and LSP `env` value.
A `$(...)` in a bundle value therefore stays literal, and does not run a command.
An MCP `env` value that relied on `$VAR` expansion in Crush now stays literal.

### The `native.crush` escape hatch

Keys that no modeled feature owns go under `native.crush`:

```yaml
native:
  crush:
    model: claude-opus-5-5
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
custom agents/commands, and most Claude Code hook events, so it reaches near-parity with the
Claude Code adapter. The events and their opencode sources are listed under
[Hook events on opencode](#hook-events-on-opencode).

(changed in v3.13.0) The opencode adapter is updated for opencode 2.x, and llmenv no longer supports opencode 1.x.
The llmenv plugin uses the opencode 2.0 plugin API, which opencode 1.x cannot load.
`llmenv doctor` reports an installed opencode older than 2.0 as unsupported, with `opencode upgrade` as the fix.
context-mode has no opencode 2.x plugin yet, so opencode gets its MCP server and its hooks, and not its native plugin.

The plugin gives every hook the payload that Claude Code sends: a Claude Code tool name (`shell` arrives as `Bash`, `read` as `Read`), Claude Code argument keys (`path` arrives as `file_path`), the session id of the call, and the project folder as `cwd`.
An MCP tool arrives as `mcp__<server>__<tool>`.
A hook runs only for the tools that its `matcher` names.
A hook blocks a tool call by exit code 2, with stderr as the reason, or by `permissionDecision: "deny"` on stdout.
A `failClosed` hook also blocks the call when it exits with any other code, or when a signal stops it, and the block names the exit code (changed in v3.14.0).
A `failClosed` hook with a matcher that is not a valid pattern blocks every call it guards (changed in v3.14.0).
The `additionalContext` of a hook reaches the model as system text, and so does the error of a hook that cannot run.
A hook that rewrites the input with `updatedInput` passes the new input to the hooks after it.
A hook that runs past its timeout is stopped with its child processes.
The llmenv `hook-run pre_tool_use` gate blocks the call when it times out or cannot start.
The text of a `Stop` hook reaches the next request, because opencode cannot continue a finished turn.
opencode's `patch` tool keeps its own name, and the llmenv config guard checks each file in its patch text.
When `plugin/llmenv-hooks.json` cannot be read, no hook runs, and each request says so with the fix.
Hooks run under `/bin/sh`.

### Hook events on opencode

Each row names the Claude Code event and the opencode source that feeds it.
A bus event, such as `Notification` or `PreCompact`, cannot block the work it reports, so a block from its hook reaches the next request.

| Event | opencode source | Notes |
| ----- | --------------- | ----- |
| `SessionStart`, `SessionEnd` | session created and deleted | |
| `UserPromptSubmit` | session `prompt` hook | |
| `PreToolUse` | tool `execute.before` | |
| `PostToolUse`, `PostToolUseFailure` | tool `execute.after` | A failed call goes to `PostToolUseFailure` only. |
| `Stop` | `session.execution.succeeded` | The text reaches the next request. |
| `SubagentStart` | tool `execute.before` on `subagent` | Added in v3.14.0. Runs at launch. A block or `deny` stops the launch, as it does for `PreToolUse`. Its other text reaches the parent session's next request, not the subagent. The matcher names the agent. |
| `SubagentStop` | tool `execute.after` on `subagent` | Added in v3.14.0. Foreground runs only. A background run reports `running` at launch, so its stop is not reported. The matcher names the agent. |
| `Notification` | `permission.asked` event | Added in v3.14.0. The message is the permission action and its resources. |
| `PreCompact` | `session.compaction.started` event | Added in v3.14.0. `trigger` is `auto` or `manual`. opencode reports no overflow reason. |
| `PostModelSwitch` | `session.model.selected` event | Added in v3.14.0. `from_model` and `to_model` are `provider/model#variant`. |
| `PostToolBatch` | `session.step.ended` event | Added in v3.14.0. The closest boundary, not the same one: it ends each model step, including a step with no tool call. No `transcript_path` is sent, so the transcript layer of adaptive recall does nothing. |

Not bridged yet: a `PreToolUse` `ask` decision, a `Stop` block that continues a finished turn, `transcript_path`, `mcp_tool` handlers, and a stop for a background `SubagentStop`.
Each one has an open issue on the v3.14.0 milestone: #2997, #2998, #2999, #3000, and #3001.
A matcher on `Notification`, `PostModelSwitch`, or `PostToolBatch` is ignored, because those events pass no matcher subject yet (#3003).

### Env vars

| Variable                       | Points to                                         | Notes                                                                                           |
|--------------------------------|---------------------------------------------------|-------------------------------------------------------------------------------------------------|
| `OPENCODE_CONFIG_DIR`          | `<cache>` (the directory holding `opencode.json`) | opencode reads `opencode.json`, `AGENTS.md`, and the `plugin/` folder from here                 |
| `OPENCODE_DISABLE_CLAUDE_CODE` | `1`                                               | opencode does not load skills from `~/.claude` or a project `.claude` folder (added in v3.13.0) |

opencode 2.x also loads every skill in `~/.agents/skills`, and no setting turns that folder off. `llmenv doctor` warns when the folder exists and opencode is installed (added in v3.13.0).

### Matchers on opencode hooks (changed in v3.15.0)

A hook `matcher` is checked against a tool name.
`PreToolUse`, `PostToolUse`, `PostToolUseFailure`, `SubagentStart`, `SubagentStop`, `PreCompact`, and `Notification` pass a name.
For `Notification`, the name is the notification type, and a permission request has the type `permission_prompt`.
A `matcher` on any other event is rejected when the opencode config is generated, so it never runs unfiltered.
A `matcher` of `*` or an empty string matches every call on any event.

### What the opencode adapter emits

| Output | Contents |
| ------ | -------- |
| `opencode.json` | `$schema` (points at the `opencode.schema.json` sidecar below), `instructions`, `mcp`, `lsp`, `permissions` — structured render, then `native_*.opencode` overlays deep-merged at the value level |
| `opencode.schema.json` | JSON Schema (draft 2020-12) generated from the same typed structs that render `opencode.json`, so it always matches what llmenv actually writes. Root allows `additionalProperties`, so passthrough/native-overlay keys never fail IDE validation. |
| `AGENTS.md` | the merged rules document opencode loads as project instructions — omitted entirely when that resolves to nothing (added in v3.10.0; earlier versions wrote a 0-byte file) |
| `service.json` | `{"disabled":true}`, so each opencode client starts a private server with this environment instead of attaching to the shared background server, which can run with another scope's config (added in v3.13.0) |
| `rules/*.md` | rule files copied verbatim and listed in `instructions` by absolute path, because opencode 2.x resolves a relative entry from the project folder (changed in v3.13.0) |
| skills (`SKILL.md`) | first-class and plugin-projected skills, in opencode's claude-compatible format |
| `command/*.md`, `commands/*.md`, `agents/*.md` | plugin commands go to `command/`; bundle commands are translated in place in `commands/`; plugin and bundle agents go to `agents/` and gain `mode: subagent`. A bundle agent replaces a plugin agent of the same name (changed in v3.13.0: agents were in `command/`, so opencode showed them as slash commands) |
| `plugin/llmenv.js` | the llmenv plugin for the opencode 2.x plugin API: it runs the hooks of `plugin/llmenv-hooks.json` (changed in v3.13.0) |
| `plugin/llmenv-hooks.json` | the hook table: each hook's event, matcher, command, and timeout, the opencode-to-Claude tool name map, and the MCP server namespaces (added in v3.13.0) |

### Capability map

| Feature | opencode support | Notes |
| --------- | ---------------- | ------- |
| Permissions (`allow`/`ask`/`deny`) | Supported for the documented neutral tool vocabulary | Rendered as opencode 2.x's ordered `permissions` list of `{action, resource, effect}` rules; opencode applies the last rule that matches (changed in v3.13.0; earlier versions wrote the 1.x `permission` map). `ask` is native (no fail-closed collapse). The neutral tool name is mapped to opencode's action by the [neutral tool vocabulary](#the-neutral-tool-vocabulary); a tool with no opencode 2.x equivalent is dropped, with a `warning:` line naming the tool (fixed in v3.11.0, [#1345](https://github.com/phaedrus1992/llmenv/issues/1345)). `native_permissions.opencode` strings are opencode's own vocabulary (`lsp`, `question`, `external_directory`, a bare `*` deny-all, ...), lowercased; a 1.x key is renamed as opencode renames it (`bash` to `shell`, `task` to `subagent`, `write` and `patch` to `edit`, added in v3.13.0). A native rule is a bare `tool` or `tool(pattern)`, and text after the closing `)` is an error (changed in v3.13.0). Because `Write` and `MultiEdit` collapse onto the same `edit` action as `Edit`, rules for any of the three interact under that one action. A rule pair whose order inverts the user's intent is rejected at regeneration time (fixed in v3.11.0, [#1328](https://github.com/phaedrus1992/llmenv/issues/1328)) — see [Permission rules opencode cannot represent](#permission-rules-opencode-cannot-represent) |
| MCP tool tiers (ICM, codebase-memory-mcp, context-mode) | Supported | Rendered as `permissions` rules on `<server>_<tool>`: read-only and mutation tools `allow`, destructive tools `ask`, overridable with `mcp_permissions`. The context-mode code-running tools (`ctx_execute`, `ctx_execute_file`, `ctx_batch_execute`) ask by default, even when `mutation` is `allow` (added in v3.14.0) |
| Effort (`effort_level`, `model_effort`) | Supported for the `large` model | Appended to the `model` as a variant, `provider/model#<effort>`; the `small` model takes no effort (added in v3.14.0) |
| Model `reasoning` flag | **Not emitted** | opencode 2.x drops the field as unsupported, so llmenv no longer writes it (changed in v3.14.0) |
| Hooks — `SessionStart`, `SessionEnd`, `UserPromptSubmit`, `PreToolUse`, `PostToolUse`, `PostToolUseFailure`, `Stop` | Supported | Bridged through the generated `plugin/llmenv.js` shim. A failed tool call goes to `PostToolUseFailure` and no longer to `PostToolUse` (changed in v3.14.0) |
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

One permission shape has no faithful rendering in `opencode.json`.
It used to be emitted anyway and then silently misbehave, so `llmenv regenerate` now fails with an error naming the offending rules instead.
opencode 2.x takes a pattern on every action, so a scoped `WebFetch` or `WebSearch` rule renders (changed in v3.13.0; the 1.x schema took only a bare action for those keys).

**Two overlapping patterns where the later-sorting one isn't the narrower.**
opencode applies the *last* matching rule in the `permissions` list, and llmenv emits
the list sorted by action and then by pattern. So for any two patterns that can
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
opencode wildcard-matches the action against the tool name as well, so a native
rule keyed `*` applies to every tool and is checked against the concrete actions
that sort after it:

```yaml
capabilities:
  native_permissions:
    opencode:
      deny: ["*(git push --force*)"]   # key "*" sorts before "shell"…
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
      - { tool: Bash }   # …except shell
```

### The `native.opencode` escape hatch

Keys that no modeled feature owns go under `native.opencode`, deep-merged into
`opencode.json` at highest precedence:

```yaml
native:
  opencode:
    theme: opencode
```

The modeled keys `instructions`, `mcp`, `lsp`, `permission`, `permissions`, `provider`,
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
