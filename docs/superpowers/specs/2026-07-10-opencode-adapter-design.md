# opencode engine support (full Claude-parity adapter)

Design for a third `AgentAdapter` targeting [opencode](https://opencode.ai)
(`sst/opencode`), aiming for feature parity with the `claude-code` adapter —
including hook bridging and Claude-plugin content translation, the two things
the `crush` adapter deliberately skipped.

Reference source explored: `~/git/reference/opencode` at `4a1982f5c`
(2026-07-10, current main).

## Goals

- `opencode` becomes a registered engine: PATH-gated, env-signal detected,
  selectable via `--engine opencode`, excludable via `disabled_engines`.
- Every llmenv capability the claude adapter expresses has an opencode
  rendering: rules, skills, MCP, LSP, permissions, hooks (all events opencode
  can observe), and Claude-style plugin content.
- Bundle-authored hook *scripts* work unchanged: the same shell command that
  receives a Claude-shaped JSON payload on stdin under Claude Code receives an
  equivalent payload under opencode.
- The three llmenv auto-hooks (stale-check, config-context injection,
  cache-write guard) work under opencode.

## Non-goals

- No new engine-neutral bundle-schema fields. Native opencode plugins (auth
  providers, notification plugins, etc.) are declared through the existing
  `native.opencode` overlay — decided during brainstorming; a first-class
  `capabilities.opencode_plugins` field is premature until a second engine
  has the same concept.
- No rendering for opencode concepts llmenv doesn't model: `keybinds`,
  `theme`, `provider`/`model`, `default_agent`, auth. All reachable via
  `native.opencode`.
- No bridging for hook events opencode cannot observe: `Notification`,
  `SubagentStop`, `PreCompact`. These warn-and-skip (crush precedent).
- opencode's data/state/session storage stays at its XDG defaults. opencode
  has **no** app-specific data-dir env override (only `OPENCODE_CONFIG_DIR`
  for config, verified in `packages/core/src/flag/flag.ts` +
  `packages/core/src/global.ts`), and leaving sessions/auth in place gives
  state persistence across config-hash changes for free.

## Background: what opencode provides

Verified against the reference checkout:

- **Config**: `opencode.json`/`opencode.jsonc` in the global config dir.
  `OPENCODE_CONFIG_DIR` env overrides that dir (`Flag.OPENCODE_CONFIG_DIR ??
  Path.config` in `global.ts`) — the same "point the whole config root at the
  llmenv cache dir" model as `CLAUDE_CONFIG_DIR`.
- **Config-dir conventions**: `AGENTS.md` (global rules), `agent/*.md`
  (agents), `command/*.md` (slash commands), `plugin/*.{js,ts}` (JS plugins),
  `skills/<name>/SKILL.md` (skills — deliberately Claude-compatible format).
- **Config keys** (v1 schema, `packages/core/src/v1/config/config.ts`):
  `instructions[]` (paths/globs), `mcp{}` (`local`/`remote`), `lsp{}`,
  `permission{}`, `agent{}`, `command{}`, `skills` (extra folder paths),
  `plugin[]` (npm specs or file paths), plus TUI/model keys we don't touch.
- **No shell-hook system.** The extension mechanism is JS plugins
  (`@opencode-ai/plugin` `Hooks` interface): `event` (bus events like
  `session.created`, `session.idle`, `session.deleted`), `chat.message`
  (can append parts to the outgoing message), `tool.execute.before` (can
  mutate args or throw to block), `tool.execute.after`, `permission.ask`,
  `shell.env`, `config`, `auth`, custom `tool`s.

## Feature mapping: claude adapter → opencode adapter

| llmenv capability | claude adapter renders | opencode adapter renders |
|---|---|---|
| `agents_md` | `CLAUDE.md` | `AGENTS.md` in config dir |
| `rules/*.md` | `rules/` dir (native convention) | copied verbatim + listed in `instructions[]` |
| skills (first-class + plugin) | `skills/<name>/SKILL.md` | same — opencode reads Claude's SKILL.md format natively |
| MCP | merged into `.claude.json` | `mcp{}` in `opencode.json` (stdio→`local`, http/sse→`remote`) |
| LSP | synthetic skills-plugin (#556) | native `lsp{}` — no hack needed |
| permissions | `settings.json` permissions | `permission{}` (mapped; unmappable → warn-and-skip) |
| hooks | `settings.json` hooks | llmenv shim plugin (§3) |
| auto-hooks (stale-check, config-context, cache guard) | auto-emitted `settings.json` hooks | shim plugin built-ins |
| plugins/marketplaces | `installed_plugins.json` + plugin cache | content translation (§4) |
| commands / agents files | copied verbatim (`commands/`, `agents/`) | translated to `command/`, `agent/` with frontmatter mapping (§5) |
| native overlay | `native.claude_code` | `native.opencode` with modeled-key rejection |

## Design

### 1. `OpencodeAdapter` registration

New `src/adapter/opencode.rs`, registered in `registered_adapters()` after
crush. Trait probes:

- `name()` = `"opencode"` (cache subdir), engine id `opencode` (already
  hyphen-free).
- `binary_name()` = `"opencode"` — PATH-gates the adapter.
- `supports_plugins()` = `true` (content translation, §4).
- `supports_lsp()` = `true`.
- `supported_hook_events()` = `["SessionStart", "SessionEnd",
  "UserPromptSubmit", "PreToolUse", "PostToolUse", "Stop"]`.
- `active_adapter()` detection signal: `OPENCODE_CONFIG_DIR` set (add arm to
  the match in `src/adapter/mod.rs`).

`env_vars()` returns `OPENCODE_CONFIG_DIR=<cache_dir>`. No data-dir var (see
Non-goals). No tmp-dir vars — opencode has no `CLAUDE_CODE_TMPDIR`
equivalent.

### 2. `materialize()` output

Written into `<cache_dir>/`, all paths reported as owned:

1. `AGENTS.md` — from `manifest.agents_md`, with the same
   `reject_hardcoded_config_path` guard the claude adapter applies.
2. `rules/*.md` — copied verbatim (frontmatter preserved); each file also
   listed (relative path) in `opencode.json` `instructions[]`.
3. `skills/<name>/SKILL.md` — via the shared
   `adapter::skills::write_first_class_skills` path, plus plugin-sourced
   skills (§4). Validated by the existing `validate_skills` machinery.
4. `opencode.json` — generated: `$schema`, `instructions`, `mcp`, `lsp`,
   `permission`, then `native.opencode` deep-merged on top (modeled-key
   rejection first: a `native.opencode` fragment may not set keys llmenv owns
   — `instructions`, `mcp`, `lsp`, `permission`; everything else, including
   `plugin`, is the user's escape hatch).
5. `plugin/llmenv.js` — the hook shim (§3), emitted only when at least one
   supported-event hook or auto-hook applies.
6. `command/*.md`, `agent/*.md` — translated bundle and plugin content (§5).

Idempotency: full rewrite of every owned file on each render, same as crush.
`opencode.json` is generated fresh (no reconcile-merge needed — unlike
Claude's `settings.json`, opencode plugins don't self-register into the
config file).

### 3. Hook bridge: the llmenv shim plugin

One self-contained ES module materialized at `<cache_dir>/plugin/llmenv.js`.
No npm dependencies, no network, no build step — generated from a template
embedded in the adapter (à la the LSP plugin template), with the hook table
baked in as JSON at render time.

Event mapping:

| llmenv event | opencode plugin surface | response handling |
|---|---|---|
| `SessionStart` | `event` → `session.created` | run hooks; collected stdout injected as context on the session's first `chat.message` |
| `UserPromptSubmit` | `chat.message` | run hooks; stdout appended as a text part to `output.parts` |
| `PreToolUse` | `tool.execute.before` | run hooks; non-zero exit with block semantics → `throw` (blocks the call); stdout hint → logged |
| `PostToolUse` | `tool.execute.after` | run hooks; fire-and-forget |
| `Stop` | `event` → `session.idle` | fire-and-forget |
| `SessionEnd` | `event` → `session.deleted` + plugin `dispose` | fire-and-forget |

Execution contract (shim → hook command):

- The shim spawns each registered hook command via the shell, writing a
  **Claude-compatible JSON payload** to stdin: `hook_event_name`,
  `session_id`, `cwd`, and for tool events `tool_name`/`tool_input`
  (mapped from opencode's tool + args). This is what makes bundle-authored
  hook scripts (including `llmenv hook-run …`) work unchanged.
- Exit 0 + stdout → context to inject (event-dependent, table above).
  Stdout that parses as Claude's `hookSpecificOutput.additionalContext` JSON
  is unwrapped; otherwise raw stdout is used (matches how `emit_hook_context`
  output round-trips).
- Exit 2 on `PreToolUse` → block (throw), stderr as the reason — mirroring
  Claude Code's blocking convention. Other non-zero exits → warn, continue.
- Per-hook timeout honoured from the hook declaration (llmenv `Hook` model),
  default matching the claude adapter's default.
- `mcp_tool` hook handlers: warn-and-skip (crush precedent) — the shim only
  executes command handlers.

Auto-hooks baked into the shim's table at render time (parity with the
claude adapter's auto-emitted hooks):

- `SessionStart`: `llmenv check-stale --engine opencode` and
  `llmenv config-context --engine opencode`.
- `PreToolUse` (write/edit tools): the cache-write guard,
  filtered to opencode's `write`/`edit` tool names.

`emit_hook_context()` for opencode returns the shim's expected shape; since
the shim accepts Claude's `hookSpecificOutput` JSON (above), the claude
implementation's output format can be shared — the opencode impl delegates
to the same JSON emitter.

### 4. Claude-plugin content translation

Plugin resolution (marketplaces, pinning, `installed_plugins.json` inputs)
stays engine-neutral and untouched. The opencode adapter consumes the same
resolved-plugin file trees the claude adapter renders, and translates:

- **Skills** → `skills/<plugin>__<skill>/` (name-prefixed to avoid
  collisions; opencode reads them natively).
- **Commands** (`commands/*.md`) → `command/<plugin>__<name>.md` (§5
  translation).
- **Agents** (`agents/*.md`) → `agent/<plugin>__<name>.md` (§5 translation).
- **MCP servers** declared by the plugin → entries in `mcp{}`.
- **Hooks, statuslines, everything Claude-protocol-specific** →
  warn-and-skip with the crush-style actionable message.

### 5. Command/agent frontmatter translation

Claude and opencode both use markdown-with-YAML-frontmatter but differ in
directory name (plural vs singular) and fields. Bundle-level `commands/` and
`agents/` files get the same treatment as plugin-sourced ones.

Commands (`commands/foo.md` → `command/foo.md`):

| Claude field | opencode field |
|---|---|
| `description` | `description` |
| `model` | `model` (only when it names a provider/model opencode can resolve — otherwise dropped with a warning) |
| `argument-hint` | dropped (no equivalent; `$ARGUMENTS` substitution works in both) |
| `allowed-tools` | dropped with warning |
| body | `template` (body passes through; `$ARGUMENTS` preserved) |

Agents (`agents/foo.md` → `agent/foo.md`):

| Claude field | opencode field |
|---|---|
| `name` | filename (opencode keys agents by filename) |
| `description` | `description` |
| `model` | `model` (same caveat as commands) |
| `tools` (comma list) | `tools` (record of `name: true`) — best-effort tool-name mapping; unknown names dropped with warning |
| — | `mode: subagent` (Claude agents are subagents by definition) |
| `color` | dropped |

Exact field inventories must be re-verified against the pinned opencode
version during implementation — this table is the design intent, not a
frozen contract.

### 6. Unsupported-capability handling

Same policy and message style as crush (`#543` follow-up): cross-engine gaps
**warn-and-skip** so one incompatible hook/permission/plugin artifact never
drops the capabilities opencode *can* express. Hard errors only for
config-authoring mistakes: modeled keys inside `native.opencode`, hardcoded
cache paths in `agents_md`, non-UTF-8 paths.

## Testing

Mirror `crush.rs`'s embedded test suite:

- trait probes (registry order, engine ids, `active_adapter` signal)
- `env_vars` (single var, UTF-8 failure path)
- `materialize` per capability: empty manifest, rules→instructions, MCP
  local/remote, LSP, permissions, native overlay merge + modeled-key
  rejection
- hook filtering: unsupported events warn-and-skip, `mcp_tool` skip
- shim generation: hook table JSON embeds resolved commands/timeouts;
  auto-hooks present; no shim emitted when no hooks apply
- frontmatter translation: command and agent cases incl. dropped-field
  warnings
- plugin translation: skills/commands/agents land in prefixed paths

The shim's JS behaviour is covered by contract tests in Rust (generated
source contains the expected table and calls); live behaviour is exercised
via the `verify` skill against a real opencode binary, not in CI.

## Rollout

Single large feature, branch from `main`, milestone **Large Projects**.
CHANGELOG entry under `[Unreleased]` (Added). Docs: engines page gains an
opencode section (env vars, capability matrix, shim explanation,
`native.opencode` examples).
