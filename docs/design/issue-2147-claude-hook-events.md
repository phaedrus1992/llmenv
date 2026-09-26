# Issue #2147 — full Claude Code hook event list, matcher and handler checks

- **Issue:** https://github.com/phaedrus1992/llmenv/issues/2147
- **Milestone:** `v3.12.0`
- **Base branch:** `release/3.x` (forward-merges to `release/4.x`)
- **Type:** feature, plus a fix to a wrong doctor message

This is a spec, not a plan.

## Problem

1. `CLAUDE_CODE_HOOK_EVENTS` (`src/adapter/claude_code.rs` line 251) lists 9 events. Claude Code has 33.
2. `llmenv doctor` prints `hook event '<e>' is not supported by the claude-code adapter — it will be skipped, not materialized` for any event outside the 9.
   That is false for Claude Code: the render loop (`src/adapter/claude_code.rs` near line 1221) writes every hook under its `event` name without a check.
   So the warning fires for valid events such as `SubagentStart`, and a misspelled event such as `PreToolUSe` is written with no real warning.
3. llmenv accepts a `matcher` on events that ignore matchers, and `mcp_tool` handlers on `SessionStart` and `Setup`, where Claude Code skips them at launch.

## Claude Code facts (hooks reference, `code.claude.com/docs/en/hooks.md`, fetched 2026-09-26)

### Events, in the reference's order

`SessionStart`, `Setup`, `InstructionsLoaded`, `UserPromptSubmit`, `UserPromptExpansion`, `MessageDisplay`, `PreToolUse`, `PermissionRequest`, `PostToolUse`, `PostToolUseFailure`, `PostToolBatch`, `PermissionDenied`, `Notification`, `SubagentStart`, `SubagentStop`, `TaskCreated`, `TaskCompleted`, `Stop`, `StopFailure`, `TeammateIdle`, `ConfigChange`, `CwdChanged`, `DirectoryAdded`, `FileChanged`, `WorktreeCreate`, `WorktreeRemove`, `PreCompact`, `PostCompact`, `PreModelSwitch`, `PostModelSwitch`, `SessionEnd`, `Elicitation`, `ElicitationResult`.

That is 33 names, one per event heading in the reference.
Use exactly this list.

### Events with no matcher support

`UserPromptSubmit`, `PostToolBatch`, `Stop`, `TeammateIdle`, `TaskCreated`, `TaskCompleted`, `WorktreeCreate`, `WorktreeRemove`, `MessageDisplay`, `CwdChanged`.
On these, Claude Code fires the hook on every occurrence and ignores any matcher.

### Handler types

Every event supports `command` and `mcp_tool`, the two handler kinds llmenv models (`HookHandlerKind::{Command, McpTool}`).
llmenv has no `prompt`, `agent` or `http` handler kinds, so the per-event limits on those types do not apply.

`mcp_tool` timing:

- `SessionStart` at launch (including `--continue` and `--resume`) fires before MCP servers are available; Claude Code skips its `mcp_tool` hooks. After `/clear` or a compaction, they run.
- `Setup` always fires before MCP servers are available; its `mcp_tool` hooks never run.

## Decisions

1. **The list is for checking, not for filtering.** The Claude Code adapter keeps rendering every hook. Claude Code adds events often, and a stale llmenv list must not drop a valid hook.
2. **Unknown event means "check the spelling".** doctor warns when an event is not in the list and suggests the closest known name.
3. **Other adapters are unchanged.** Crush really does skip unsupported events; its message stays. The doctor text becomes adapter-specific.
4. **No neutral event names in this issue.** Hook `event` values are Claude Code's names today; keep that.

## Design

### Constant

Replace the 9-entry `CLAUDE_CODE_HOOK_EVENTS` with the 33 names above, in the same order, with a doc comment that names the source page and date.
Add:

```rust
/// Claude Code events that ignore a hook's matcher (hooks reference).
const CLAUDE_CODE_MATCHERLESS_EVENTS: &[&str] = &[
    "UserPromptSubmit", "PostToolBatch", "Stop", "TeammateIdle", "TaskCreated",
    "TaskCompleted", "WorktreeCreate", "WorktreeRemove", "MessageDisplay", "CwdChanged",
];
```

### Adapter trait

Add one method to `AgentAdapter` with a default:

```rust
/// Whether this adapter drops hooks whose event is not in `supported_hook_events()`.
/// `false` means the adapter renders them anyway and the list is only advisory.
fn drops_unsupported_hook_events(&self) -> bool { true }
```

`ClaudeCodeAdapter` returns `false`.
Crush and opencode keep the default, `true`.

### Checks

Put the Claude-specific checks in one pure function in `src/adapter/claude_code.rs`:

```rust
pub(crate) fn hook_warnings(hooks: &[crate::config::Hook]) -> Vec<String>
```

For each hook, in order, add a warning when:

1. `event` is not in `CLAUDE_CODE_HOOK_EVENTS`:
   `hook event '<event>' is not a Claude Code hook event, so Claude Code never fires it. Check the spelling<; did you mean '<closest>'?>`
   `closest` is the known event with the smallest case-insensitive edit distance, shown only when that distance is 3 or less.
   Use `strsim::levenshtein` on the lowercased names.
   `strsim` 0.11.1 is already in `Cargo.lock` on `release/3.x` (a transitive dependency), so add `strsim = "=0.11.1"` to the root `[dependencies]`; this adds no new crate.
   Run `scripts/gen-attribution.sh` and commit its output with the change.
2. `matcher` is set and `event` is in `CLAUDE_CODE_MATCHERLESS_EVENTS`:
   `hook on '<event>' has matcher '<matcher>', but Claude Code ignores matchers on this event and runs the hook every time`
3. `handler.kind` is `McpTool` and `event` is `Setup`:
   `mcp_tool hook on 'Setup' never runs: Setup fires before MCP servers connect. Use a command handler`
4. `handler.kind` is `McpTool` and `event` is `SessionStart`:
   `mcp_tool hook on 'SessionStart' is skipped at launch and on --resume; it runs only after /clear or a compaction. Use a command handler to run at every start`

### Doctor

In `src/cli/doctor.rs` near line 972, replace the loop body:

- For an adapter whose `drops_unsupported_hook_events()` is `true`: keep today's message.
- For the Claude Code adapter: print each string from `hook_warnings(&manifest.capabilities.hooks)` with the `{warn}` prefix, and do not print today's message.

Do not change the existing `native_hooks.crush` hard error in `crush.rs`.

## Tests

1. The constant has 33 unique entries, and every entry of `CLAUDE_CODE_MATCHERLESS_EVENTS` is in it.
2. `hook_warnings` table test: each of the four rules fires on its case and is silent on a clean hook (`PreToolUse` with matcher `Bash`, command handler).
3. Suggestion: `PreToolUSe` suggests `PreToolUse`; `Foo` gives no suggestion.
4. Adapter test: `ClaudeCodeAdapter.drops_unsupported_hook_events()` is `false`; Crush and opencode are `true`.
5. The existing test at `src/adapter/mod.rs` near line 780, which checks that the list contains the 9 old events, still passes; extend it with `SubagentStart` and `PermissionRequest`.

## Acceptance criteria

1. A config with a `SubagentStart` hook gives no doctor warning, and the hook is in the rendered `settings.json`.
2. A config with `event: PreToolUSe` gives a doctor warning that suggests `PreToolUse`.
3. Changelog `Fixed`: doctor no longer says Claude Code hooks on newer events are skipped. `Added`: doctor checks hook event names, matchers and `mcp_tool` timing for Claude Code.
4. `website/docs/` hooks page lists the Claude Code events llmenv knows, the matcher note and the `mcp_tool` timing note, tagged `(added in v3.12.0)`.

## Out of scope

- `prompt`, `agent` and `http` handler kinds.
- Neutral, engine-independent event names.
