# Issue #2149 — document and report Claude Code's starting permission mode

- **Issue:** https://github.com/phaedrus1992/llmenv/issues/2149
- **Milestone:** `v3.11.2`
- **Base branch:** `release/3.x` (forward-merges to `release/4.x`)
- **Type:** documentation, plus a small doctor report

This is a spec, not a plan.

## Problem

The issue title assumed that only sessions with telemetry off start in auto mode.
The Claude Code docs (permission modes page, fetched 2026-09-26) give a broader rule: from Claude Code 2.1.283, every interactive terminal or VS Code session starts in `auto` when no permission mode is configured.

llmenv's docs do not say this, so a user who leaves `capabilities.permissions.default_mode` unset does not know which mode their sessions start in.
The docs also list only 4 of the 7 values llmenv accepts for `default_mode`.

## Claude Code facts (permission modes page)

A new terminal session takes its permission mode from the first of these that applies:

1. `--permission-mode` or `--dangerously-skip-permissions`.
2. `permissions.defaultMode` in a settings file. `auto` and `bypassPermissions` have no effect from project files (`.claude/settings.json`, `.claude/settings.local.json`); they do work from the user file (`~/.claude/settings.json`), which is the file llmenv renders.
3. The built-in default:

| How Claude Code runs | Built-in starting mode |
| --- | --- |
| Any settings file sets `disableAutoMode` to `"disable"` | `default` (Manual) |
| `claude -p` or the Agent SDK | `default` (Manual) |
| Interactive terminal or VS Code | `auto` with 2.1.283 or later; on earlier versions, `auto` on Pro, Max or Team plans in sessions that fetch feature flags, else `default` |

If `auto` is selected but not available (model, provider, organization, or server-side switch), the session starts in Manual.
`manual` is accepted as an alias of `default` from Claude Code 2.1.200.
With telemetry off, auto mode's safety review runs on the server by default from 2.1.282 (`CLAUDE_CODE_AUTO_MODE_SERVER=0` opts out).

## Verified locations (release/3.x)

| Fact | Location |
| --- | --- |
| llmenv modes and their Claude values: `acceptEdits`, `plan`, `default`, `bypassPermissions`, `auto`, `dontAsk`, `manual` | `permission_mode_str`, `src/adapter/claude_code.rs` |
| Rendered as `permissions.defaultMode` only when `default_mode` is set | settings render function, `src/adapter/claude_code.rs` |
| Docs example comment lists only `acceptEdits \| plan \| default \| bypassPermissions` | `website/docs/configuration.md` line 184 |

## Changes

### Docs (`website/docs/configuration.md`)

1. Line 184: the comment lists all seven values: `default | manual | acceptEdits | plan | auto | dontAsk | bypassPermissions`.
2. Add a subsection under the permissions section, heading `Starting permission mode (Claude Code)`, tagged `(added in v3.11.2)`:
   - With `default_mode` unset, llmenv writes no `defaultMode`, and Claude Code's built-in default applies: `auto` for interactive sessions from Claude Code 2.1.283, Manual for `claude -p`.
   - Set `default_mode: default` (or `manual`) to keep Manual.
   - llmenv writes `defaultMode` into the user settings file, so `auto` and `bypassPermissions` take effect; the same values in a project's `.claude/settings.json` would not.
   - If auto mode is not available to the session, Claude Code starts in Manual.
   - With telemetry off, auto mode's review runs on Anthropic's server by default; `native.claude_code.env.CLAUDE_CODE_AUTO_MODE_SERVER: "0"` keeps it local.
   - Link to the Claude Code permission-modes page.

### Doctor

Add a pure function in `src/adapter/claude_code.rs`:

```rust
/// The starting permission mode Claude Code will use for an interactive
/// session with this rendered settings.json, and why.
pub(crate) fn starting_permission_mode(settings: &serde_json::Value) -> (String, &'static str)
```

Rules, in order:

1. `permissions.defaultMode` is a string: return it, reason `set by capabilities.permissions.default_mode or native settings`.
2. Top-level `disableAutoMode == "disable"` or `permissions.disableAutoMode == "disable"`: return `default`, reason `auto mode is disabled in settings`.
3. Otherwise: return `auto`, reason `Claude Code 2.1.283+ built-in default for interactive sessions (claude -p starts in default)`.

In `llmenv doctor`, in the Claude Code section, read the rendered `settings.json` from `adapter_root` (as #2145 does) and print one line:
`{info} starting permission mode: <mode> (<reason>)`.
A missing or unreadable file prints `{info} starting permission mode: unknown (settings.json not rendered yet)`.

This line is information only. It never warns.

## Tests

1. `starting_permission_mode` for: `defaultMode: "plan"`; `defaultMode: "manual"`; no `defaultMode` and top-level `disableAutoMode: "disable"`; `permissions.disableAutoMode: "disable"`; empty object.
2. Doctor prints the line for a rendered file (if doctor output has tests; otherwise cover the function only).

## Acceptance criteria

1. The docs subsection exists, with the version tag and the Claude Code link.
2. `llmenv doctor` shows the starting mode line.
3. Changelog `Added`: doctor shows Claude Code's starting permission mode. Docs changes need no changelog entry of their own.

## Out of scope

- Changing llmenv's default for `default_mode`. Leaving it unset keeps Claude Code's own default, which is the user's choice.
- The my-llmenv config comment; that is my-llmenv issue #69.
