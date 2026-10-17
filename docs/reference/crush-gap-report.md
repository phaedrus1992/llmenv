<!-- markdownlint-disable MD013 -->

# Crush v0.98.1 vs the Claude Code engine: gaps and improvements

Sources:

- Crush: `charmbracelet/crush`.
  The baseline is tag `v0.81.0` (released 2026-06-29), the newest release when the llmenv Crush adapter landed (`aeba0148`, 2026-07-01).
  The current release is tag `v0.98.1` (released 2026-10-09, commit `8b182428`).
  Every `crush/<path>:<line>` citation names its tag.
- llmenv: `release/3.x` after the v3.13.0 bugfix sweep (#2870).
  llmenv citations name a file and an identifier, not a line.
- Method: source read at both tags, through the code-explorer cache. No run checks.

## 1. Summary

The Crush feature set that matters to llmenv grew less between v0.81.0 and v0.98.1 than expected.
The hook system, the permission model, skills, context files, custom commands, and the task subagent are the same at both tags.
The biggest improvements come from features that Crush already had at v0.81.0, which the adapter never used:

1. Crush loads `CRUSH.md` from the config dir that llmenv sets, but the adapter never writes llmenv's merged instructions there.
   This is the largest gap: on Crush, llmenv's global instructions do not reach the agent.
2. Crush's PreToolUse hook accepts Claude Code's output format, with deny, `updatedInput`, and `additionalContext`.
   llmenv's PreToolUse guards could run on Crush.
3. `options.disabled_tools` can carry an unscoped `deny` rule.
4. Crush's `agent` tool is the counterpart of `Task`.

New since v0.81.0 and useful to llmenv: a top-level `env` key (v0.88.0) and a `question` tool (v0.85.0).
New but not useful to llmenv today: `crushrc` shell config, `/etc/crush/crush.json`, MCP OAuth 2.1, MCP channels, a plan agent, and five LSP tools.

## 2. Improvements llmenv can make now

| # | Improvement | Crush support | Min. Crush | Issue |
| --- | --- | --- | --- | --- |
| 1 | Render the merged `agents_md` to `CRUSH.md` in the Crush config dir | `GlobalContextPaths` defaults to `<dir of $CRUSH_GLOBAL_CONFIG/crush.json>/CRUSH.md`: crush/internal/config/load.go:576-582 and 1212-1217 (v0.98.1), load.go:510-516 (v0.81.0) | v0.81.0 | #2872 |
| 2 | Render unscoped `deny` rules to `options.disabled_tools` | crush/internal/config/config.go:471, applied at config.go:1059-1065 (v0.98.1); config.go:289 (v0.81.0) | v0.81.0 | #2873 |
| 3 | Map `Task` to `agent`; add `AskUserQuestion` to the vocabulary, mapped to `question` | `agent`: crush/internal/agent/agent_tool.go:23 (v0.98.1); `question`: internal/agent/tools/question.go:15 (v0.98.1) | v0.81.0 (`agent`), v0.85.0 (`question`) | #2874 |
| 4 | Render `capabilities.env` to the top-level `env` key | crush/internal/config/config.go:858-859, applied at load.go:534-551 (v0.98.1) | v0.88.0 | #2875 |
| 5 | Run llmenv's PreToolUse guards (commit gate, cd guard, read-once, slippage, repeat detection) through `hook-run` | Claude Code `hookSpecificOutput` parsed at crush/internal/hooks/input.go:91-94 and 157-182; deny and context applied at internal/agent/hooked_tool.go:62-96 (v0.98.1); the same files at v0.81.0 | v0.81.0 | #2876 |
| 6 | Deliver SessionStart memories and notices through the first PreToolUse context | Same hook output path as row 5 | v0.81.0 | #2869 |

### 2.1 Notes on the hook rows

- Crush has one hook event, `PreToolUse`, at both tags: crush/internal/hooks/hooks.go:15.
  No other Claude Code event exists as a string anywhere in `internal/` at v0.98.1.
- The hook config is `hooks.<event>[]` with `name`, `matcher` (a regex on the Crush tool name), `command`, and `timeout` (default 30 s): crush/internal/config/config.go:804-813 (v0.98.1).
- Stdin carries `event`, `session_id`, `cwd`, `tool_name`, and `tool_input`; the same values are in `CRUSH_*` env vars: crush/internal/hooks/input.go:23-29 and 53-76.
- Exit code 2 blocks the call with stderr as the reason, and exit code 49 halts the turn: crush/internal/hooks/runner.go:221-233 (v0.98.1).
- `additionalContext` is appended to the tool result after the tool runs. It is not injected before the call, so advisory text reads as a postscript.
- Hooks wrap only the top-level agent's tools, not the task subagent's: crush/internal/agent/coordinator.go:966 (v0.98.1).
- Tool names and inputs differ from Claude Code (`bash`, `view`, `edit`, `file_path`), so `hook-run` must normalize them first.

## 3. Gaps that remain

| Claude Code feature (llmenv) | Crush v0.98.1 | Why it stays a gap |
| --- | --- | --- |
| SessionStart, UserPromptSubmit, PostToolUse, Stop, SubagentStop, PreCompact, Notification hooks | Only `PreToolUse` | No event to attach to. Row 6 above covers the SessionStart text through PreToolUse; the rest have no channel |
| `mcp_tool` hook handlers | Command hooks only | No handler kind |
| Path- or pattern-scoped permission rules | `allowed_tools` matches `tool` or `tool:action` only: crush/internal/permission/permission.go:187-188 | A scoped rule would widen to the whole tool, so llmenv drops it |
| `ask` rules | Every tool not allowed already prompts | No separate key needed; the default is `ask` |
| Custom agents (`agents/*.md`) | `coder`, `task`, and (v0.95.0) `plan` agents; agents are `json:"-"`: crush/internal/config/config.go:861 (v0.98.1) | Users cannot configure agents |
| Plugins and marketplaces | No plugin system | Skills are projected; other plugin content still errors |
| Custom commands from plugins | Crush loads commands from `~/.config/crush/commands`, `~/.crush/commands`, and the project data dir: crush/internal/commands/commands.go:121-136 | None of those is under the config dir llmenv owns. Writing into the user's home is out of scope |
| Status line | No status line config | #855 stays blocked upstream |
| Output styles | No output-style concept | llmenv renders them as skills (v3.10.0) |
| `NotebookEdit`, `Skill` tools | No equivalent; skills are read through `view` | Rules naming them stay dropped |
| `WebSearch` | `web_search` exists only inside the `agentic_fetch` subagent: crush/internal/agent/agentic_fetch_tool.go:165-166 | No top-level tool to allow |
| Compaction control | `options.disable_auto_summarize` only; no compaction hook | Nothing to attach PreCompact behavior to |

## 4. New Crush features with no llmenv counterpart today

- `crushrc` (v0.88.0): a config file written as a Bash script, with builtins for providers, models, MCP, LSP, permissions, hooks, and options (crush/internal/shellconfig/register.go:21-28).
  A project `.crushrc` or `crush.json` sits above llmenv's global `crush.json` in Crush's merge order (crush/internal/config/load.go:940-979), so a project file can override what llmenv renders.
  The docs should say so.
- `/etc/crush/crush.json` (v0.87.0): a system-wide config at the lowest priority.
- MCP OAuth 2.1 for `http` servers (v0.87.0): `oauth`, `oauth_client_id`, `oauth_client_secret`, `oauth_callback_port`, `oauth_token` (crush/internal/config/config.go:253-275).
  llmenv does not model MCP OAuth for any engine.
- MCP channels (v0.97.0): a server can push events into the session (crush/internal/config/config.go:281-321).
- The plan agent and plan mode (v0.95.0), and five LSP tools (v0.86.0).
  Their tool names can be named in `native_permissions.crush` if a user wants them pre-approved.

## 5. Corrections to llmenv's current Crush docs

These were already true at v0.81.0:

- "Hooks — `PreToolUse`: `command`-kind handlers only" is right, but the hook can also return `additionalContext` and `updatedInput`, not only a decision.
- "Custom agents: Crush hardcodes exactly two agent roles (coder/task)" is three roles since v0.95.0 (`plan`), and none is configurable.
- `Task` is not absent from Crush: the `agent` tool is its counterpart (#2874).
