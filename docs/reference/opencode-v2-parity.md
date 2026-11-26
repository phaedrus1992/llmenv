<!-- markdownlint-disable MD013 -->

# opencode v2 vs the Claude Code engine: parity gaps

Sources:

- opencode: `anomalyco/opencode`, tag `v2.0.31`, commit `22728d674` (released 2026-10-16).
  Every `opencode/<path>:<line>` citation below is at that tag.
- Run checks: the opencode `v2.0.30` binary installed by Homebrew, in `--standalone` mode with private `XDG_*` directories.
- llmenv: `main` at `e5723de5`.
  llmenv citations name a file and an identifier, not a line, because line numbers differ between release lines.

Issue: [#2839](https://github.com/phaedrus1992/llmenv/issues/2839) (epic), children #2840 to #2851.

Each finding carries one of two marks:

- **run-verified**: reproduced with the `v2.0.30` binary.
- **source-verified**: read in the `v2.0.31` source, not reproduced.

## 1. Summary

opencode v2 is a rewrite, not an update.
The plugin API, the config schema, the tool names, and the process model all changed.
The llmenv opencode adapter targets the v1 shapes.

Four findings break the adapter on v2 today:

1. **The hook bridge does not load (run-verified).**
   v2 accepts only a default export with an `id` and a `setup` or `effect` function (`opencode/packages/core/src/plugin/module.ts:78-133`).
   The generated `plugin/llmenv.js` (`SHIM_TEMPLATE` in `src/adapter/opencode.rs`) exports a v1 hook object.
   v2 logs `failed to load plugin ... PluginModule.LoadError: Plugin must export a default definition with an id and an effect or setup function.`
   So no llmenv hook runs on v2: no ICM wake-up or recall, no guards, no task redirect, no session log, and no bundle hooks.
2. **Rules do not load from the cache dir (source-verified).**
   llmenv writes `instructions: ["rules/<x>.md"]` as relative paths.
   v2 resolves a relative entry by an upward glob from the session directory to the project root (`opencode/packages/core/src/config/plugin/instruction.ts:88-97`), not from the config dir.
   The rule either does not load, or a project's own `rules/<x>.md` loads in its place.
3. **Unscoped skills leak in (source-verified).**
   v2 reads `~/.claude/skills` and `~/.agents/skills` for every session (`opencode/packages/core/src/config/discovery.ts:69-143`, `opencode/packages/core/src/config/plugin/compatibility.ts`).
   Only `OPENCODE_DISABLE_CLAUDE_CODE=1` turns off the `.claude` roots (`opencode/packages/cli/src/server-process.ts:122-126`).
   llmenv sets only `OPENCODE_CONFIG_DIR`, so skills outside llmenv scope rules load.
4. **The background service can ignore the llmenv config (source-verified).**
   v2 runs one shared background server by default.
   Its registration file lives in the global state dir and is not keyed by `OPENCODE_CONFIG_DIR` (`opencode/packages/cli/src/services/service-config.ts:117-139`, `opencode/packages/cli/src/services/server-connection.ts:42-50`).
   A client that llmenv starts can attach to a server that started with another config dir, or with none.

The v1 `opencode.json` itself still loads.
v2 migrates v1 keys (`permission`, `plugin`, `provider`, `small_model`, `agent`, `command`, `mcp`) through `ConfigMigrateV1` (`opencode/packages/core/src/config/normalize.ts:57-231`).
Unknown keys are ignored (`normalize.ts:44`).

## 2. What changed in v2

| Area | v1 (what llmenv targets) | v2.0.31 |
| --- | --- | --- |
| Plugin package | `@opencode-ai/plugin` | `@opencode/plugin` (`opencode/packages/plugin/package.json`) |
| Plugin shape | hook object (`event`, `chat.message`, `tool.execute.before`, ...) | `Plugin.define({ id, setup(ctx) })`; hooks register through domains: `ctx.tool.hook`, `ctx.session.hook`, `ctx.permission.hook`, `ctx.shell.hook`, `ctx.event.subscribe` (`opencode/packages/plugin/src/README.md`) |
| Config keys | `permission`, `plugin`, `provider`, `agent`, `command` | `permissions` (ordered rule list), `plugins`, `providers`, `agents`, `commands`, `policies` (`opencode/packages/schema/src/config.ts:26-114`) |
| Permission match | per-tool pattern map | ordered `{action, resource, effect}` list, last match wins (`opencode/packages/core/src/permission.ts:91`) |
| Tool ids | `bash`, `task`, `todowrite`, `list`, ... | `shell`, `subagent`, `read`, `edit`, `write`, `patch`, `glob`, `grep`, `skill`, `question`, `webfetch`, `websearch`; no `todowrite`, no `list` (`opencode/packages/core/src/tool/plugin/`, `opencode/packages/core/src/v1/config/migrate.ts:28-33`) |
| MCP tool names | `<server>_<tool>` | same (`opencode/packages/core/src/tool/mcp.ts:16-17`) |
| Model selection | `provider/model` | `provider/model#variant`, or `{ providerID, model, variant }` (`opencode/packages/schema/src/config/model.ts:12-35`) |
| `small_model` | top-level key | migrated to `agents.title.model` (`normalize.ts:137-151`) |
| Model `reasoning` field | accepted | unsupported, dropped with a diagnostic (`normalize.ts:54`) |
| Agent files | `agent/*.md` | `{agent,agents}/**/*.md`, `{mode,modes}/*.md` (`opencode/packages/core/src/config/plugin/agent.ts:22-26`) |
| Claude compatibility | none | reads `.claude/skills` and `.agents/skills` roots |
| Process model | one process per TUI | shared background service; `--standalone` gives a private server |
| UI extension | none | TUI plugins with slots such as `prompt.footer.status` and `sidebar.content` (`opencode/packages/plugin/src/tui/context.ts:191-201`) |

## 3. Gap matrix

Status key: **parity** (works the same), **partial** (works with a loss), **broken** (wired, but does not work on v2), **missing** (not wired for opencode).

### 3.1 Hooks and lifecycle

| Claude Code feature (llmenv) | opencode adapter today | v2 status | v2 mechanism for parity |
| --- | --- | --- | --- |
| `SessionStart` hook-run (ICM wake-up, check-stale, config-context) | shim on `session.created` | broken (shim does not load) | `ctx.event.subscribe` on `session.created`; inject with `ctx.session.synthetic` or a `session.hook("context")` system part |
| `SessionEnd` | shim on `session.deleted` and `dispose` | broken | `session.deleted` event; plugin cleanup function |
| `UserPromptSubmit` (`turn_start` recall, rules digest, throttle) | shim on `chat.message`; throttle not wired | broken; throttle missing | `ctx.session.hook("prompt")` (mutable prompt) |
| `PreToolUse` deny | exit code 2, or a deny envelope on stdout | broken | `ctx.tool.hook("execute.before")` fails with `Tool.Error` |
| `PreToolUse` `ask` decision | not supported | missing (#2998) | `ctx.permission.hook("evaluate")` sets `effect` to `ask` (`opencode/packages/plugin/src/effect/permission.ts:10-22`) |
| `PreToolUse` `updatedInput` | not supported | missing | `execute.before` has a mutable `input` (`opencode/packages/plugin/src/effect/tool.ts:20-28`) |
| `PostToolUse` | shim on `tool.execute.after` | broken | `execute.after` with mutable `result` |
| `PostToolUseFailure` (adaptive recall) | not registered | missing | `execute.after` with `status: "error"` |
| `PostToolBatch` (adaptive recall) | not registered | bridged (v3.14.0); transcript layer inert (#2997) | `session.step.ended` event (closest; not the same boundary) |
| `SubagentStart`, `SubagentStop`, `subagent_task` | not registered | bridged (v3.14.0); background stop missing (#3001) | `execute.before` on tool `subagent` (start); `execute.after` with `result.output.status` `completed` (stop, foreground only; the subagent's own Output is `result.output`). The `subagent` input names the agent `agent`, not `subagent_type` |
| `Stop` | shim on `session.idle`, cannot block | broken | `session.execution.succeeded` event; a block becomes `ctx.session.prompt` with the reason |
| `Notification` | not registered | bridged (v3.14.0) | `permission.asked` event |
| `PreCompact` | not registered | bridged (v3.14.0); `reason` is `auto` or `manual`, no `overflow` | `session.compaction.started` event |
| `PostModelSwitch` (agent-config doc, #2398) | not registered | bridged (v3.14.0) | `session.model.selected` event |
| Tool matcher on bundle hooks | matcher dropped; every hook runs on every tool (`generate_shim_js`) | broken (also wrong on v1) | filter in the plugin before the spawn |
| `mcp_tool` hook handler | dropped with a warning | missing (#3000) | plugin calls the MCP tool through the `mcp` domain (not verified) |
| `session_id` on the hook payload | read from `OPENCODE_SESSION_ID`, which v2 sets only in shell-tool children (`opencode/packages/core/src/tool/plugin/shell.ts:216`) | broken (empty) | every v2 hook input carries `sessionID` |
| `transcript_path` | absent | missing (#2997) | plugin writes a JSONL transcript under the llmenv state dir, or hook-run reads messages through the session API |

### 3.2 hook-run handlers that key on Claude names

These handlers compare against Claude tool names and argument keys.
The opencode payload carries opencode names (`read`, `shell`, `filePath`), so the handlers return early even when the hook fires.

| Handler | Claude key it checks | opencode v2 value | Effect on opencode |
| --- | --- | --- | --- |
| read-once (`src/hook_run/read_once.rs`) | `tool_name == "Read"` | `read` | inert |
| slippage read-before-edit, metrics (`src/hook_run/slippage.rs`) | `Read`, `Write`, `Edit`, `MultiEdit`, `Bash` | `read`, `write`, `edit`, `patch`, `shell` | inert |
| adaptive recall subagent (`src/hook_run/adaptive.rs`) | `Agent` | `subagent` | inert |
| cbm index guard (`src/hook_run/cbm_index_guard.rs`) | `mcp__codebase-memory-mcp__index_repository` | `codebase-memory-mcp_index_repository` | inert; the cross-project overwrite is not blocked |
| task redirect (`src/hook_run/task_tools.rs`) | `TaskCreate`, `TaskList`, `TaskUpdate`, `todowrite` | v2 has no todo tool | moot on v2; the tracker has no engine tool to replace |
| config guard (`llmenv config-guard`) | reads `CLAUDE_CONFIG_DIR` | unset under opencode | falls back to the default cache path |

Parity needs one normalization step in `hook-run`: map the engine tool name and argument keys to the neutral (Claude) vocabulary at payload intake.
The table in `src/adapter/tools.rs` already holds the forward map, so the reverse map belongs beside it.

### 3.3 Config surfaces

| Claude Code feature (llmenv) | opencode adapter today | v2 status | Parity path |
| --- | --- | --- | --- |
| `CLAUDE.md` / `AGENTS.md` | `AGENTS.md` in the config dir | parity (global `AGENTS.md` loads from the config dir, `instruction.ts:41`) | none |
| `rules/*.md` | written; listed as relative `instructions` | broken | emit absolute paths |
| Conditional rules (`paths:` frontmatter) | every rule is unconditional | partial (context cost) | plugin injects a rule through `session.hook("context")` after a matching `read`; or keep unconditional |
| Skills | native `skills/*/SKILL.md` | parity | none |
| Leak of `~/.claude/skills` and `~/.agents/skills` | not handled | broken | set `OPENCODE_DISABLE_CLAUDE_CODE=1`; the `.agents` roots have no switch, so `doctor` should warn when `~/.agents/skills` exists |
| Subagents (`agents/*.md`) | written to `command/__*_agent_*.md` with `mode: subagent` | broken (also wrong on v1: commands, not agents) | write to `agents/<name>.md` |
| Slash commands | `command/*.md` | parity | v2 also reads `commands/` |
| Output styles | rendered as a skill | partial (the model must invoke it) | plugin adds the style as a system part through `session.hook("context")` |
| Permissions | v1 per-tool map, migrated by v2 | partial | emit v2 `permissions` as an ordered list; update the tool map (`bash`→`shell`, `task`→`subagent`, drop `todowrite` and `list`) |
| MCP tool tiers (ICM, cbm, context-mode) | not rendered | missing | render `permissions` rules on `<server>_<tool>` |
| MCP servers | v1 `mcp` map, migrated | parity | optional: emit v2 `mcp.servers` |
| LSP | v1 `lsp`, same shape in v2 | parity | none |
| Model providers | v1 `provider`, migrated | partial (`reasoning` dropped) | emit v2 `providers`; route reasoning through the model `variant` |
| Default models | `model`, `small_model` | parity through migration | emit `agents.title.model` directly |
| `effortLevel`, per-model effort | not rendered | missing | `model: "<provider>/<model>#<variant>"` |
| `advisorModel` | not rendered | missing | no v2 equivalent found |
| `autoMemoryEnabled: false` when ICM is on | not applicable | parity | opencode has no automatic memory |
| `syncClaudeAiSkills/Plugins: false` | not applicable | see the skills leak | `OPENCODE_DISABLE_CLAUDE_CODE=1` |
| Status line | not rendered | missing | TUI plugin claim on `prompt.footer.status` |
| Temp dir isolation (`TMPDIR` in the state dir, #1379) | not set | missing | add the same four temp vars to `env_vars` |
| Claude plugin hooks (`hooks/hooks.json`) | translated into the shim table | broken (shim) | carried by the new plugin |
| Native opencode plugins (context-mode) | `plugin: ["context-mode"]` | broken (source-verified): the pinned `v1.0.169`, also its latest release, exports `{ id, server }` (`mksglu/context-mode` `src/adapters/opencode/plugin.ts:261`), which the v2 loader rejects | do not register it on v2 until upstream ships a v2 build |
| Config isolation from the background service | not handled | broken | launch with `--standalone`; or run a service per scope |

## 4. What parity takes

### 4.1 Replace the shim with a v2 plugin

Write one static plugin module, `plugin/llmenv.js`, in the v2 shape.
Keep the hook table as generated JSON beside it, so the module itself is a fixed asset with tests.

```js
export default {
  id: "llmenv",
  setup: async (ctx) => {
    await ctx.tool.hook("execute.before", async (e) => { /* PreToolUse */ })
    await ctx.tool.hook("execute.after", async (e) => { /* PostToolUse, failure, subagent stop */ })
    await ctx.session.hook("prompt", async (e) => { /* UserPromptSubmit */ })
    await ctx.permission.hook("evaluate", async (e) => { /* ask decisions */ })
    // Not awaited: setup must return, and the stream never ends.
    void (async () => {
      for await (const ev of ctx.event.subscribe()) { /* created, deleted, idle, compaction, model.selected, permission.asked */ }
    })()
  },
}
```

The call shapes come from the Promise API at `v2.0.31`:

- Each domain `hook` takes `(name, callback)` and returns a `Registration` (`opencode/packages/plugin/src/promise/registration.ts`).
- A throw or a rejected Promise in `execute.before` rejects that tool call only (`opencode/packages/plugin/src/promise/adapter.ts:637-641`).
- `ctx.event.subscribe()` returns an `AsyncIterable` over the server event stream (`opencode/packages/client/src/promise/generated/client.ts:1713`).

Rules for the new plugin:

1. Filter each table entry by its matcher before the spawn.
2. Send the neutral tool name and Claude argument keys, or let `hook-run` normalize them (section 3.2).
3. Put `sessionID` into `session_id`.
4. Parse the JSON decision from stdout (`permissionDecision`, `updatedInput`, `additionalContext`), not only the exit code.
5. Treat a spawn failure as a visible error, not a silent `console.error`.

### 4.2 Renderer changes

1. Absolute `instructions` paths.
2. Agents into `agents/`, not `command/`.
3. `OPENCODE_DISABLE_CLAUDE_CODE=1` and temp vars in `env_vars`.
4. v2 tool map in `src/adapter/tools.rs`, plus the reverse map for `hook-run`.
5. MCP tier permissions.
6. Effort as a model variant.
7. Optional: emit native v2 keys (`permissions`, `plugins`, `providers`, `agents`, `mcp.servers`) instead of relying on `ConfigMigrateV1`.
   v2 still accepts the v1 keys through `ConfigMigrateV1`.
   Because llmenv drops opencode 1.x, nothing needs the v1 keys, and upstream can remove that migration layer.

### 4.3 Launch and version handling

1. `llmenv launch opencode` passes `--standalone`, or `doctor` warns when a background service is running.
2. llmenv supports opencode 2.x only, and drops 1.x support (decision of 2026-10-16).
   The adapter writes only the v2 plugin.
   `doctor` reports an installed opencode older than 2.0 as unsupported.

### 4.4 Size estimates

Estimates assume one engineer who knows the llmenv codebase, and include tests.

| Work | Estimate |
| --- | --- |
| v2 plugin module, hook table, JSON decision parsing, tests against `v2.0.30` | 2 days |
| `hook-run` tool-name and argument normalization (section 3.2) | 1 day |
| Renderer fixes 1 to 4 in section 4.2 | 1 day |
| MCP tier permissions and effort variant | 1 day |
| Launch `--standalone` and `doctor` version check | 0.5 day |
| Conditional rules and output styles through `session.hook("context")` | 2 days |
| Status line TUI plugin | 1 day (needs a Solid JSX build step, which llmenv does not have) |
| Native v2 key output | 1 day |

The first four rows restore what the adapter claimed on v1: about 5 days.
All rows together: about 9.5 days.

## 5. Open questions

1. Can a plugin call an MCP tool through `ctx.mcp`? That decides whether `mcp_tool` hook handlers can work on opencode.
2. Run-verify finding 2 (relative `instructions`) with a reachable model.

## 6. Method notes

- The codebase-memory-mcp `v0.11.0` index of the opencode clone crashed with signal 10 (SIGBUS) in the resolve pass, in both `moderate` and `fast` modes.
  This report reads source with `rg` and `Read` at the pinned tag instead of graph queries.
- The run checks used `opencode run --standalone --model bogus/none`, so no model request left the machine.
