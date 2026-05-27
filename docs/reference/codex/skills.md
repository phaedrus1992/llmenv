# Skills

Codex skills are folders containing a `SKILL.md`, enabled via config. The
SKILL.md format (name + description frontmatter + body) is the **same convention
Claude Code uses**, so llmenv's existing skill validation largely carries over.

## Config

```toml
[[skills.config]]
path = "/path/to/skill-folder"   # folder containing SKILL.md
enabled = true
```

`skills.config` is an array of per-skill enablement overrides. Each entry has
`path` (the skill folder) and `enabled`. Related feature flag:
`features.skill_mcp_dependency_install` (prompt/install missing MCP deps for
skills; on by default).

## Gaps vs llmenv

Today `ClaudeCodeAdapter` **validates** SKILL.md (name+description frontmatter,
`validate_skills` at `src/adapter/claude_code.rs:115`) but doesn't generate the
skill content — bundle files supply it.

For Codex a `CodexAdapter` would:

- **Reuse the same SKILL.md validation** — the frontmatter contract matches.
- **Additionally register each skill** by emitting a `[[skills.config]]` entry
  with the materialized `path` and `enabled = true`. Claude Code auto-discovers
  skills in `skills/`; Codex requires explicit `path` registration in
  `config.toml`. So Codex needs an *extra generation step* (the registration)
  that Claude Code doesn't.
- Decide skill folder placement under `CODEX_HOME` and point `path` at it.
