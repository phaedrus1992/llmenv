# Issue #2155 — opt-in codebase-memory search-augment hooks for Claude Code

- **Issue:** https://github.com/phaedrus1992/llmenv/issues/2155
- **Milestone:** `v3.12.0`
- **Base branch:** `release/3.x` (forward-merges to `release/4.x`)
- **Type:** feature

This is a spec, not a plan.

## Problem

`codebase-memory-mcp install` registers Claude Code hooks that add graph context to Grep, Glob and Bash searches.
Under llmenv those hooks are either missing, or the user runs `cbm install` by hand and it writes into llmenv's rendered config directory: a script at `$CLAUDE_CONFIG_DIR/hooks/cbm-code-discovery-gate` and entries in `$CLAUDE_CONFIG_DIR/settings.json`.
llmenv did not render either, and a later `llmenv regenerate` can drop or duplicate them.

## cbm facts (source at tag `v0.11.0`)

| Fact | Location |
| --- | --- |
| Claude hooks cbm installs: `PreToolUse` matcher `Grep\|Glob\|Bash` and `PostToolUse` matcher `Read`, timeout 5 seconds | `src/cli/cli.c` lines 4431 to 4447 and `cbm_upsert_claude_hooks_with_binary` near line 5200 |
| The hook command runs a shim at `$CLAUDE_CONFIG_DIR/hooks/cbm-code-discovery-gate` (or `~/.claude/hooks/…`) | `cbm_resolve_hook_command`, near line 2982 |
| The shim only calls `<binary> hook-augment`; it never blocks a tool call, and a missing, old or hung binary exits 0 silently | comment above `cbm_install_hook_gate_script`, near line 5669 |
| For Codex, cbm registers `codebase-memory-mcp hook-augment` directly, with no shim | near line 3949 |
| `hook-augment` reads the event from `hook_event_name` in the stdin payload; `--dialect` names only non-Claude formats (copilot, gemini, …), so no flag means Claude Code's format | `src/cli/hook_augment.c` lines 1007 and 1075 |
| 0.11.0 changes: graph-augmented Bash searches; no-op Bash calls are skipped before identity hashing | v0.11.0 release notes |

## Verified locations (release/3.x)

| Fact | Location |
| --- | --- |
| `CodebaseMemory { when, index_path, mcp_permissions }` (plus `mem_budget_mb` from #2154) | `crates/llmenv-config/src/schema.rs` line 1407 |
| Claude hook rendering builds `hooks_by_event` and appends llmenv's own hooks | `src/adapter/claude_code.rs` near lines 1221 to 1310 |
| The cbm entry is found in the manifest with `m.name == CODEBASE_MEMORY_MCP_NAME` | `src/adapter/claude_code.rs` near line 1695 |
| `hooks` is reconciled so plugin-registered hooks survive a render | `reconcile_settings`, `src/adapter/claude_code.rs` near line 2168 |

## Decisions

1. **Opt-in.** New field `codebase_memory.augment_hooks: bool`, default `false`. The hooks run on every Grep, Glob, Bash and Read call; users choose that cost.
2. **No shim.** llmenv renders the command `codebase-memory-mcp hook-augment` directly, as cbm does for Codex. Nothing is written into the config directory's `hooks/` folder.
3. **Same environment as the server.** When `index_path` is set, prefix the command with `CBM_CACHE_DIR=<shell-quoted path> ` so the hook sees the same index as the MCP server.
4. **Claude Code only.** cbm ships its own opencode plugin; llmenv does not render hooks for opencode or Crush here.
5. **Detect, do not delete, a manual install.** doctor warns when the rendered `settings.json` holds cbm installer hooks.

## Design

### Config

```rust
/// Render codebase-memory-mcp's search-augment hooks for Claude Code
/// (PreToolUse Grep|Glob|Bash, PostToolUse Read). Off by default: they run
/// on every search and read.
#[serde(default, skip_serializing_if = "std::ops::Not::not")]
pub augment_hooks: bool,
```

### Rendering (Claude Code adapter)

When the manifest has the cbm MCP entry and the matching `CodebaseMemory` entry has `augment_hooks: true`, add to `hooks_by_event`:

| Event | Matcher | Hook |
| --- | --- | --- |
| `PreToolUse` | `Grep\|Glob\|Bash` | `{ "type": "command", "command": "<cmd>", "timeout": 5 }` |
| `PostToolUse` | `Read` | same |

`<cmd>` is `codebase-memory-mcp hook-augment`, or `CBM_CACHE_DIR=<quoted> codebase-memory-mcp hook-augment` when `index_path` is set.
Quote the path with `shell_escape` (`src/cli/mod.rs` line 817: single quotes, embedded single quote as `'\''`). It is private to `cli`; make it `pub(crate)` and call it from the adapter rather than writing a second copy.

The resolved MCP entry does not carry the `CodebaseMemory` config, so pass `augment_hooks` (and `index_path`) through the manifest the same way `mcp_permissions` reaches the adapter.

### doctor

In the codebase-memory section, read the rendered `settings.json` (from `adapter_root`, as #2145 does).
If any hook command contains `cbm-code-discovery-gate`:

- `augment_hooks: true`: `{warn} codebase-memory: settings.json has hooks from a manual codebase-memory-mcp install as well as llmenv's; searches run the augmenter twice. Remove the entries whose command contains cbm-code-discovery-gate.`
- `augment_hooks: false` or no cbm entry: `{info} codebase-memory: settings.json has hooks from a manual codebase-memory-mcp install. Set codebase_memory.augment_hooks: true to let llmenv manage them, then remove the manual entries.`

## Tests

1. Render with `augment_hooks: false`: no cbm hooks.
2. Render with `augment_hooks: true`: exactly the two entries, with matchers, command and timeout as specified.
3. Render with `augment_hooks: true` and `index_path: "/tmp/cbm idx"`: the command carries the quoted `CBM_CACHE_DIR` prefix.
4. Render with `augment_hooks: true` but the cbm entry inactive (tags do not match): no cbm hooks.
5. doctor classification from a synthetic `settings.json`, both branches.

## Acceptance criteria

1. Measure first: in this repo, `echo '<a Claude PreToolUse Grep payload>' | codebase-memory-mcp hook-augment` completes; record the median of 10 runs in the PR description. If the median is above 300 ms, keep the feature but say so in the docs.
2. With `augment_hooks: true`, a Grep call in Claude Code shows cbm's added context.
3. Changelog `Added`: `codebase_memory.augment_hooks`.
4. `website/docs/mcp.md` codebase-memory section documents the field, its cost and the manual-install warning, tagged `(added in v3.12.0)`.

## Out of scope

- cbm's `SessionStart` and `SubagentStart` reminder hooks. llmenv's own skill reference (#2152) covers that guidance.
- Hooks for engines other than Claude Code.
