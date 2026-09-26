# Issue #2146 — turn off claude.ai skill and plugin sync by default

- **Issue:** https://github.com/phaedrus1992/llmenv/issues/2146
- **Milestone:** `v3.12.0`
- **Base branch:** `release/3.x` (forward-merges to `release/4.x`)
- **Type:** behavior change (new default)

This is a spec, not a plan.

## Problem

Since Claude Code 2.1.275, a terminal session signed in with a claude.ai account downloads the skills and plugins enabled on that account and loads them.
Skills go to `<config dir>/skills/synced/`; plugins go to `<config dir>/plugins/synced/` and load as `<name>@synced`.
Under llmenv, `<config dir>` is llmenv's rendered `$CLAUDE_CONFIG_DIR`.

llmenv decides which skills and plugins are active per scope.
Account sync goes around that: account skills and plugins load in every scope, whatever the tags say, and their names can tie with llmenv-managed skills.

## Claude Code facts (settings reference, fetched 2026-09-26)

| Key | Accepted values | Scope | Effect of `false` |
| --- | --- | --- | --- |
| `syncClaudeAiSkills` | only `false` has an effect; `true` equals unset | user, local, managed, `--settings` | stops the download, stops loading already-synced skills, and in user settings moves them to `skills/.trash/` |
| `syncClaudeAiPlugins` | same | same | same, for `plugins/synced/` and `plugins/.trash/` |

llmenv's rendered `settings.json` is the user settings file, so both keys work there.

A related key, `disableClaudeAiConnectors` (any scope, `true` turns off claude.ai MCP connectors), does the same job as `ENABLE_CLAUDEAI_MCP_SERVERS=false`.
This issue does not change the connector default, because some users rely on claude.ai connectors.

## Decisions

1. **Default off, rendered by the adapter.** The Claude Code adapter writes `"syncClaudeAiSkills": false` and `"syncClaudeAiPlugins": false` into `settings.json`.
2. **No new config schema.** A user opts back in through the existing native pass-through: `native.claude_code.syncClaudeAiSkills: true` (and the same for plugins).
   This is the pattern `autoMemoryEnabled` uses: the adapter inserts its default before the native overlay, so a native value wins.
3. **Owned keys.** Add `syncClaudeAiSkills` and `syncClaudeAiPlugins` to `LLMENV_OWNED_SETTINGS_KEYS`, so a stale value in the file is replaced on every render.
4. **Connectors unchanged.** Document `native.claude_code.disableClaudeAiConnectors: true` as the settings-file way to turn connectors off. Do not render it by default.

## Verified locations (release/3.x)

| Fact | Location |
| --- | --- |
| `autoMemoryEnabled` default inserted before native overlays | `src/adapter/claude_code.rs` near line 1754, the `#227/#123` block |
| `LLMENV_OWNED_SETTINGS_KEYS` array (size in the type) | `src/adapter/claude_code.rs` line 1931 (`[&str; 10]` today) |
| Native overlay is applied after modeled keys | same render function, the `native` merge step |

## Design

In the settings render function in `src/adapter/claude_code.rs`, next to the `autoMemoryEnabled` block and before the native overlay:

```rust
// Account sync loads skills and plugins outside llmenv's scope rules
// (Claude Code 2.1.275). Native settings can turn it back on.
settings.insert("syncClaudeAiSkills".into(), json!(false));
settings.insert("syncClaudeAiPlugins".into(), json!(false));
```

Insert them unconditionally for the Claude Code adapter.
Update `LLMENV_OWNED_SETTINGS_KEYS`: add both names and bump the array length in its type.
If any test asserts the exact key set of a rendered `settings.json`, update it to include the two keys.

Crush and opencode are not changed.

## Effect on existing users

On the first render after upgrade, Claude Code moves already-synced skills and plugins into `skills/.trash/` and `plugins/.trash/` inside the rendered config dir.
Nothing is deleted by llmenv.
The changelog entry and docs must say this, and say how to opt back in.

## Tests

1. Render with no native override: `settings.json` has both keys set to `false`.
2. Render with `native.claude_code.syncClaudeAiSkills: true`: that key is `true`; the plugin key is still `false`.
3. Reconcile: an existing `settings.json` with `"syncClaudeAiSkills": true` and no native override becomes `false` after render.
4. `LLMENV_OWNED_SETTINGS_KEYS` contains both names (guard test).

## Acceptance criteria

1. A signed-in session under llmenv does not list `@synced` plugins in `/plugin`, and `/skills` shows no synced skills.
2. With `native.claude_code.syncClaudeAiPlugins: true`, synced plugins load again.
3. Changelog `Changed`: llmenv turns off claude.ai skill and plugin sync by default; already-synced items move to `.trash`; how to opt back in.
4. `website/docs/` Claude Code engine page documents the default, the opt-in, and `disableClaudeAiConnectors`, tagged `(added in v3.12.0)`.

## Out of scope

- A neutral `capabilities` field for account sync. Only Claude Code has this concept.
- Changing the claude.ai connector default.
