# Claude Code configuration reference

Reference notes on **Claude Code's** configuration surfaces, captured from the
official docs at <https://code.claude.com/docs/en/> (fetched 2026-05-27), and
analyzed against what **llmenv** currently materializes. A parallel set for
**Codex** is planned alongside this one under `docs/reference/codex/`.

These are working references for future design docs. They err on the side of too
much detail. Each page ends with a **Gaps vs llmenv** section that is the actual
point of this exercise: what Claude Code supports that llmenv does not yet
generate, validate, or model.

## What llmenv generates today

`ClaudeCodeAdapter::materialize` (`src/adapter/claude_code.rs`) writes, into the
`CLAUDE_CONFIG_DIR` it points Claude Code at:

| Artifact | Source | Status |
| --- | --- | --- |
| `CLAUDE.md` | `manifest.agents_md` | Full |
| `rules/*.md` | `manifest.rules` (verbatim, frontmatter preserved) | Full |
| copied bundle files | `manifest.files` (byte-copy; `hooks/*.json` get `{{ICM_MCP}}` substitution) | Full |
| `skills/<name>/SKILL.md` | bundle files | **Validated only** (name+description frontmatter), not generated |
| `mcp.json` | `manifest.mcps` (stdio `command`/`args`/`env`, remote `url`) | Full |
| `settings.json` | `generate_settings_json` | **Stub** — emits `{"hooks":[],"permissions":[],"mcp":[]}` |

Key structural facts about llmenv:

- Config format is **YAML** (`config.yaml`, `serde_yaml_ng`), not TOML.
- Single crate `llme`. Selection model is **tag intersection** across
  network/host/user/project scopes; bundles, MCP servers, and the memory backend
  are all selected the same way.
- The `memory` backend (ICM) desugars into a resolved MCP server and lands in
  `mcp.json` alongside user MCP entries.

## The headline gap

`generate_settings_json` is a placeholder (issue #34). It emits a JSON object with
three keys — and **`mcp` is not even a real `settings.json` key** (MCP servers
live in `mcp.json` / `.mcp.json` / `~/.claude.json`, never in `settings.json`).
Meanwhile Claude Code's `settings.json` exposes ~120 user-relevant keys. Every
config surface below except CLAUDE.md/rules/mcp.json is currently unreachable
through llmenv.

## Pages

- [settings.json](./settings.md) — the ~120-key settings file; precedence; what llmenv stubs
- [hooks](./hooks.md) — ~25 events, 5 handler types, exact JSON nesting
- [permissions](./permissions.md) — allow/ask/deny rules, modes, sandbox
- [skills-and-commands](./skills-and-commands.md) — SKILL.md, commands/*.md formats
- [subagents](./subagents.md) — `.claude/agents/*.md` frontmatter
- [mcp](./mcp.md) — `.mcp.json` schema, transports, scopes, managed MCP
- [memory](./memory.md) — CLAUDE.md, rules, auto memory
- [statusline-and-output-styles](./statusline-and-output-styles.md)
- [plugins](./plugins.md) — plugin.json, marketplaces
- [claude-directory](./claude-directory.md) — full file/dir layout
- [env-vars](./env-vars.md) — environment variables
- [gap-analysis](./gap-analysis.md) — **consolidated** gap table across all surfaces
