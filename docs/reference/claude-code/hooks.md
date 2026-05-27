# Hooks reference

Source: <https://code.claude.com/docs/en/hooks> (fetched 2026-05-27).

Hooks are user-defined handlers that fire at lifecycle points. They live in the
`hooks` key of any `settings.json`, in plugin `hooks/hooks.json`, in subagent
frontmatter, or registered in-session.

## Configuration schema

Three levels of nesting under `settings.json` → `hooks`:

```json
{
  "hooks": {
    "PreToolUse": [
      {
        "matcher": "Bash",
        "hooks": [
          {
            "type": "command",
            "if": "Bash(rm *)",
            "command": "${CLAUDE_PROJECT_DIR}/.claude/hooks/block-rm.sh",
            "args": []
          }
        ]
      }
    ]
  }
}
```

1. **Hook event** (`PreToolUse`, `Stop`, …) — keys the top-level object.
2. **Matcher group** (`{matcher, hooks}`) — `matcher` filters which tool/event
   instances fire (e.g. tool name `"Bash"`, regex, or omitted for all).
3. **Hook handler** — one of five `type`s.

Source labels in the `/hooks` menu: `User`, `Project`, `Local`, `Plugin`,
`Session`. YAML frontmatter form (subagents) uses the same structure.

## Hook handler types

| `type` | Behavior |
| --- | --- |
| `command` | Run a shell command. Input on **stdin** as JSON; result via exit code + stdout. |
| `http` | POST event JSON to a URL; result from response body (same JSON output schema). |
| `mcp_tool` | Call a tool on a connected MCP server; tool text output = stdout. |
| `prompt` | Single-turn model evaluation returning yes/no JSON. |
| `agent` | Spawn a subagent (Read/Grep/Glob) to verify before deciding. **Experimental.** |

`command` hook fields: `command` (required), `args`, `if` (a permission-rule-style
guard), `timeout`, plus common fields. HTTP hooks: `url`, `allowedEnvVars`
(intersected with `httpHookAllowedEnvVars`).

## Hook events

Events fall into three cadences: per-session, per-turn, per-tool-call.

**Support all five handler types** (`command`, `http`, `mcp_tool`, `prompt`,
`agent`):
`PermissionDenied`, `PermissionRequest`, `PostToolBatch`, `PostToolUse`,
`PostToolUseFailure`, `PreToolUse`, `Stop`, `SubagentStop`, `TaskCompleted`,
`TaskCreated`, `TeammateIdle`, `UserPromptExpansion`, `UserPromptSubmit`.

**Support `command`/`http`/`mcp_tool` only** (no `prompt`/`agent`):
`ConfigChange`, `CwdChanged`.

**`command` + `mcp_tool` only:** `SessionStart`, `Setup`, `SubagentStart`.

**Other events** (varying support): `Elicitation`, `ElicitationResult`,
`FileChanged`, `InstructionsLoaded`, `Notification`, `PostCompact`, `PreCompact`,
`SessionEnd`, `StopFailure`, `WorktreeCreate`, `WorktreeRemove`.

The full lifecycle: optional `Setup` → `SessionStart` → per-turn loop
(`UserPromptSubmit`, `UserPromptExpansion`, then the agentic loop: `PreToolUse`,
`PermissionRequest`, `PostToolUse`, `PostToolUseFailure`, `PostToolBatch`,
`SubagentStart`/`Stop`, `TaskCreated`/`Completed`) → `Stop`/`StopFailure` →
`TeammateIdle`, `PreCompact`, `PostCompact`, `SessionEnd`. `Elicitation`/`Result`
nest inside MCP tool execution; `WorktreeCreate/Remove`, `Notification`,
`ConfigChange`, `InstructionsLoaded`, `CwdChanged`, `FileChanged` are async.

## Input / output

**Input** (stdin / POST body): common fields `session_id`, `transcript_path`,
`cwd`, `hook_event_name`, plus event-specific fields (e.g. `WorktreeCreate` adds
`name`; tool events add tool name + args).

**Output** — two mutually exclusive approaches per hook:

- **Exit codes only:** exit 0 = allow/silent; exit 2 = block. JSON ignored if you
  exit 2.
- **JSON on exit 0:** structured control. Fields:
  - Universal: `continue` (false = Claude stops entirely), `systemMessage`,
    `suppressOutput`.
  - Top-level `decision` (`"block"`) + `reason` — used by `UserPromptSubmit`,
    `UserPromptExpansion`, `PostToolUse`, `PostToolUseFailure`, `PostToolBatch`,
    `Stop`, `SubagentStop`, `ConfigChange`, `PreCompact`.
  - `hookSpecificOutput` (requires `hookEventName`) — richer per-event control.
    `SessionStart`/`Setup`/`SubagentStart` use `additionalContext`; SessionStart
    also accepts `initialUserMessage`, `watchPaths`.

Output strings (incl. `additionalContext`, `systemMessage`, plain stdout) capped
at **10,000 chars**; overflow spilled to a file + preview. stdout must contain
*only* the JSON object (shell-profile noise breaks parsing).

HTTP response handling: 2xx empty = success; 2xx text = added as context; 2xx
JSON = parsed; non-2xx / timeout = non-blocking error. HTTP cannot block via
status alone — must return 2xx + decision JSON.

## Gaps vs llmenv

llmenv handles hook **files** but not hook **wiring**:

- `materialize` copies `hooks/*.json` and substitutes `{{ICM_MCP}}`
  (`src/adapter/claude_code.rs:59`), so bundles can ship hook scripts/templates.
- But `generate_settings_json` emits `"hooks": []` — an **empty array of the wrong
  shape**. Nothing populates the `hooks` object that actually registers those
  files at `PreToolUse`/`Stop`/etc. The copied files are inert.

To close this (issue #34), llmenv needs:

1. A YAML vocabulary for hooks — likely per-bundle, e.g. a `hooks:` list mapping
   event → matcher → handler, tag-selected and merged across active bundles.
2. A generator that produces the nested `{ <Event>: [{matcher, hooks:[...]}] }`
   object, with `${CLAUDE_PROJECT_DIR}`-relative paths to the copied scripts.
3. A decision on which handler types to support. The ICM/memory integration
   today implies `command` hooks that reference the MCP by name; `mcp_tool` hooks
   (call ICM tools directly) may be a cleaner fit and would remove the
   `{{ICM_MCP}}` placeholder dance.
4. Optional validation: warn on unknown event names, missing referenced files,
   handler types unsupported by the chosen event.

Note the `disableAllHooks` and `allowedHttpHookUrls`/`httpHookAllowedEnvVars`
settings — if llmenv ever generates HTTP hooks it must also manage their
allowlists, or they will be silently blocked.
