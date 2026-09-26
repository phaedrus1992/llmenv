# Issue #2145 — warn on retired Claude Code settings, env vars and tools

- **Issue:** https://github.com/phaedrus1992/llmenv/issues/2145
- **Milestone:** `v3.12.0`
- **Base branch:** `release/3.x` (forward-merges to `release/4.x`)
- **Type:** feature (doctor check), plus a correction to two existing doctor recommendations

This is a spec, not a plan.

## Problem

Claude Code retires settings keys, environment variables and tools.
A retired entry in the config that llmenv renders does nothing, and nothing tells the user.
Sources of such entries: `native.claude_code` (settings keys and `env`), `capabilities.env`, permission rules, MCP entries, and foreign keys that llmenv keeps in the rendered `settings.json` (seeded from `~/.claude/settings.json`, or written by Claude Code itself).

Two existing `llmenv doctor` recommendations are also out of date:

1. It recommends `BASH_MAX_OUTPUT_LENGTH`, but Claude Code ignores that variable when the `bashOutputMaxChars` setting is set.
2. It recommends `ENABLE_PROMPT_CACHING_1H`, but subscription users already get the 1-hour TTL on the main conversation, and `CLAUDE_CODE_PROMPT_CACHE_TTL` now takes precedence over it.

## Decisions

1. **Check the rendered files, not the config.** One check over the rendered `settings.json` and `.claude.json` covers every source, including foreign keys, with one code path.
2. **Warn, never fail.** Output uses doctor's `{warn}` prefix. Exit status does not change.
3. **Doctor only.** `llmenv validate` is not changed; a retired key is not a config error.
4. **One table, in the adapter.** Claude Code knowledge belongs to the Claude Code adapter.
5. **Monitor's `persistent` option is not in the table.** It is a tool-call argument that the model chooses, not a config value, so no config can hold it.

## Retired entries (verified 2026-09-26)

Sources: Claude Code settings reference and environment-variable reference (`code.claude.com/docs/en/settings-reference.md`, `env-vars.md`), and the Claude Code changelog.

### Settings keys (top level of `settings.json`)

| Key | Status | Since | Replacement text for the warning |
| --- | --- | --- | --- |
| `taskOutputMaxChars` | removed, no effect | 2.1.277 | none; Claude reads a background task's output file with Read |
| `permissionExplainerEnabled` | removed, no effect | 2.1.257 | none |
| `teammateDefaultModel` | removed, no effect | 2.1.234 | see Claude Code's agent-teams docs |
| `keybindingFlavor` | deprecated, no effect | 2.1.261 | none; word-editing keys always follow readline |
| `includeCoAuthoredBy` | deprecated, still read | 2.0.62 | `attribution` |
| `disableArtifact` | deprecated, still read | not stated | `enableArtifact: false` |
| `voiceEnabled` | deprecated, still read | 2.1.92 | `voice.enabled` |

### Environment variables (the `env` object in `settings.json`)

| Variable | Status | Since | Replacement |
| --- | --- | --- | --- |
| `TASK_MAX_OUTPUT_LENGTH` | removed, no-op | 2.1.277 | none |
| `CLAUDE_SUBAGENT_BG_SHELL_MAX_MS` | removed, no-op | 2.1.260 | none |
| `CLAUDE_CODE_MAX_SUBAGENTS_PER_SESSION` | removed, no-op | 2.1.224 | none |
| `CLAUDE_CODE_CONNECT_TIMEOUT_MS` | removed, no-op | 2.1.186 | `API_TIMEOUT_MS` |
| `CLAUDE_CODE_OPUS_4_6_FAST_MODE_OVERRIDE` | removed, no-op | 2.1.160 | none |
| `CLAUDE_CODE_ENABLE_OPUS_4_7_FAST_MODE` | removed, no-op | 2.1.142 | none |
| `ANTHROPIC_SMALL_FAST_MODEL` | deprecated | not stated | `ANTHROPIC_DEFAULT_HAIKU_MODEL` |
| `ENABLE_PROMPT_CACHING_1H_BEDROCK` | deprecated | not stated | `ENABLE_PROMPT_CACHING_1H` |

### Permission rules (`permissions.allow`, `ask`, `deny` in `settings.json`)

| Tool name | Status | Since | Replacement |
| --- | --- | --- | --- |
| `TaskOutput` | tool removed; the rule matches nothing | 2.1.277 | none |

A rule matches this row when its tool name, the part before any `(`, equals `TaskOutput` exactly.

### MCP servers (`mcpServers` in `.claude.json`)

| Entry shape | Status | Since | Replacement |
| --- | --- | --- | --- |
| `"type": "sdk"` | skipped at load with a warning | 2.1.274 | use `stdio` or `http`; only an SDK host application can register in-process servers |

## Design

### Table

New file `src/adapter/claude_code/retired.rs`, declared from `src/adapter/claude_code.rs` with `mod retired;` (a file module next to `claude_code.rs`).

```rust
pub(crate) enum RetiredKind {
    SettingsKey,
    EnvVar,
    PermissionTool,
    McpType,
}

pub(crate) struct Retired {
    pub kind: RetiredKind,
    /// Exact name as Claude Code spells it.
    pub name: &'static str,
    /// `true` when Claude Code ignores it; `false` when it still reads it but it is deprecated.
    pub no_effect: bool,
    /// Claude Code version, or `None` when the docs give none.
    pub since: Option<&'static str>,
    /// Replacement, or `None`.
    pub replacement: Option<&'static str>,
}

pub(crate) const RETIRED: &[Retired] = &[ /* rows from the tables above */ ];
```

Add a doc comment above `RETIRED` that says where each row came from and that new rows go at the top.

### Scan

```rust
pub(crate) struct RetiredHit {
    pub entry: &'static Retired,
    /// Where it was found, such as `settings.json env` or `settings.json permissions.deny[3]`.
    pub location: String,
}

pub(crate) fn scan(settings: &serde_json::Value, claude_json: &serde_json::Value) -> Vec<RetiredHit>
```

- Settings keys: top-level keys of `settings`.
- Env vars: keys of `settings["env"]` when it is an object.
- Permission tools: each string in `settings["permissions"]["allow"|"ask"|"deny"]`.
- MCP types: each value in `claude_json["mcpServers"]` whose `type` is the string `sdk`.
- Non-object or missing parts are skipped, never an error.
- Output order: settings keys, env, permissions, MCP; within each, the file order.

`scan` is pure and has no file I/O.

### Doctor

In `src/cli/doctor.rs`, after the lifecycle-hook section, when the Claude Code adapter is installed:

1. Read `settings.json` and `.claude.json` from the same `adapter_root` doctor already computes for the credentials check (near line 753).
2. A missing file counts as `{}`. A file that is not valid JSON prints one `{warn}` line naming the file and the parse error, and the scan continues with `{}` for it.
3. Print a heading `Retired Claude Code settings:` only when there is at least one hit.
4. One line per hit, format:
   - no effect: `{warn} <location>: <name> has no effect since Claude Code <since>. <replacement text or "Remove it.">`
   - deprecated: `{warn} <location>: <name> is deprecated. Use <replacement>.`
   - when `since` is `None`, omit `since Claude Code <since>`.
5. When there are no hits, print nothing.

### Corrected recommendations (`run_doctor_token_efficiency`)

1. `BASH_MAX_OUTPUT_LENGTH`: if `native.claude_code.bashOutputMaxChars` is set, print `{pass} bashOutputMaxChars=<n> (BASH_MAX_OUTPUT_LENGTH is ignored while it is set)` and skip the variable check.
   Otherwise keep today's check.
2. `ENABLE_PROMPT_CACHING_1H`: pass if it is `1` or `true`, or if `CLAUDE_CODE_PROMPT_CACHE_TTL` is `1h` (process env or `native.claude_code.env`).
   When none is set, change the text from a warning to `{info} prompt cache TTL not set: subscription plans get 1h on the main conversation automatically; API-key and cloud-provider users can set CLAUDE_CODE_PROMPT_CACHE_TTL=1h`.

Use the existing `effective_token_efficiency_var` helper for every lookup.

## Tests

1. `scan` finds each table row in a synthetic `settings.json` or `.claude.json`, one test per kind.
2. `scan` does not flag `TaskOutputFoo` or `Bash(TaskOutput)`; it flags `TaskOutput` and `TaskOutput(*)`.
3. `scan` on `{}`, on `settings.env` as an array, and on `permissions` as a string returns no hits and does not panic.
4. A table-shape test: every `name` is unique within its kind, and every `no_effect == false` row has a `replacement`.
5. Token-efficiency tests for the two corrected checks (bashOutputMaxChars present; TTL variable present; neither present).

## Acceptance criteria

1. A config with `native.claude_code.env.TASK_MAX_OUTPUT_LENGTH: "1000"` makes `llmenv doctor` print the removal warning with `2.1.277`.
2. `llmenv doctor` on a config with no retired entries prints no `Retired Claude Code settings` heading.
3. Changelog `Added`: doctor warns about retired Claude Code settings. `Changed`: doctor's prompt-cache and bash-output advice.
4. `website/docs/` doctor or troubleshooting page lists the new check, tagged `(added in v3.12.0)`, and says the table follows Claude Code's own docs.

## Out of scope

- Rewriting or removing retired keys automatically.
- Checking Crush or opencode configs.
