# Subagents reference

Source: <https://code.claude.com/docs/en/sub-agents> (fetched 2026-05-27).

Subagents are specialized assistants with their own system prompt, tools, and
model. File-based subagents live in `~/.claude/agents/` (user) or
`.claude/agents/` (project). No local scope.

## File format

```markdown
---
name: code-reviewer
description: Reviews code for quality and best practices
tools: Read, Glob, Grep
model: sonnet
---

You are a code reviewer. When invoked, analyze the code and provide
specific, actionable feedback on quality, security, and best practices.
```

Frontmatter, then the system prompt as the markdown body. Loaded at session
start — **editing a file on disk requires a restart**; subagents created via
`/agents` take effect immediately.

## Frontmatter fields

Full set (also accepted by `--agents` JSON, where `prompt` replaces the body):

`description`, `prompt`, `tools`, `disallowedTools`, `model`, `permissionMode`,
`mcpServers`, `hooks`, `maxTurns`, `skills`, `initialPrompt`, `memory`, `effort`,
`background`, `isolation`, `color`.

| Field | Notes |
| --- | --- |
| `name` | Identifier (file-based). |
| `description` | When Claude should invoke it (drives auto-dispatch). |
| `tools` / `disallowedTools` | Allow/deny tool lists. |
| `model` | Model alias (`sonnet`, …) or inherit. |
| `effort` | `low`/`medium`/`high`/`xhigh`. |
| `maxTurns` | Turn cap. |
| `permissionMode` | Per-agent permission mode. |
| `mcpServers` / `hooks` | Per-agent MCP + hooks. |
| `skills` | Skills available to the agent. |
| `memory`, `initialPrompt`, `background`, `isolation`, `color` | Misc behavior/display. |

## Scope & precedence

| Scope | Path |
| --- | --- |
| User | `~/.claude/agents/` |
| Project | `.claude/agents/` |
| Managed | `.claude/agents/` under managed settings dir (highest precedence) |
| Plugin | `<plugin>/agents/` (namespaced) |

**Plugin subagents cannot use `hooks`, `mcpServers`, or `permissionMode`** (those
fields are ignored for security). Subagent definitions are also usable as
agent-team teammate types.

## Gaps vs llmenv

- **Entirely unmodeled.** llmenv has no agents path. Bundle files placed under
  `agents/` would byte-copy through `manifest.files`, but there is no schema,
  validation, selection, or generation for subagents.
- This is arguably the largest *capability* gap (vs the largest *correctness* gap,
  which is `settings.json`). Subagents are a first-class extension surface and a
  natural thing to distribute per-scope (e.g. a `rust` bundle ships a
  rust-focused reviewer agent).
- If added, mirror the skills approach: copy `agents/*.md`, optionally validate
  required frontmatter (`name`, `description`), and document the restart caveat.
- The per-agent `mcpServers`/`hooks` fields interact with llmenv's MCP/memory
  resolution — a generated agent referencing the memory MCP would need the same
  name resolution that `{{ICM_MCP}}` provides for hooks.
