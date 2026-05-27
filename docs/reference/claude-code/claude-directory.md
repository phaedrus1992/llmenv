# The .claude directory reference

Source: <https://code.claude.com/docs/en/claude-directory> (fetched 2026-05-27).

Where Claude Code reads configuration. Project-scope files live under `.claude/`
(or repo root for `CLAUDE.md`, `.mcp.json`, `.worktreeinclude`). Global-scope
files live under `~/.claude/`. llmenv materializes its own dir and points
`CLAUDE_CONFIG_DIR` at it.

## File reference

| File | Scope | Commit | What it does |
| --- | --- | --- | --- |
| `CLAUDE.md` | Project + global | ✓ | Instructions loaded every session |
| `rules/*.md` | Project + global | ✓ | Topic-scoped instructions, optionally path-gated |
| `settings.json` | Project + global | ✓ | Permissions, hooks, env vars, model defaults |
| `settings.local.json` | Project only | | Personal overrides, auto-gitignored |
| `.mcp.json` | Project only | ✓ | Team-shared MCP servers |
| `.worktreeinclude` | Project only | ✓ | Gitignored files to copy into new worktrees |
| `skills/<name>/SKILL.md` | Project + global | ✓ | Reusable prompts, `/name` or auto-invoked |
| `commands/*.md` | Project + global | ✓ | Single-file prompts (same mechanism as skills) |
| `output-styles/*.md` | Project + global | ✓ | Custom system-prompt sections |
| `agents/*.md` | Project + global | ✓ | Subagents |
| `hooks/` | Project + global | ✓ | Hook scripts referenced from `settings.json` |

Override order: managed settings > CLI flags (`--permission-mode`, `--settings`) >
some env vars > the files above (see [settings.md](./settings.md) precedence).

## Application data (not config)

`~/.claude.json` (OAuth session, user/local MCP, per-project trust, caches —
five timestamped backups), session transcripts (cleaned per `cleanupPeriodDays`),
auto memory dir, plans dir. Plaintext storage warning applies to `~/.claude.json`.

## What llmenv materializes (mapping)

| Claude Code file | llmenv generates? |
| --- | --- |
| `CLAUDE.md` | ✓ from `agents_md` |
| `rules/*.md` | ✓ verbatim |
| `settings.json` | ✗ stub only (wrong shape) |
| `settings.local.json` | ✗ (n/a — single merged config) |
| `mcp.json` / `.mcp.json` | ✓ full |
| `skills/<name>/SKILL.md` | ~ validated only, not generated |
| `commands/*.md` | ✗ (would byte-copy, unmodeled) |
| `output-styles/*.md` | ✗ |
| `agents/*.md` | ✗ |
| `hooks/` scripts | ✓ copied + `{{ICM_MCP}}` substituted, but **not wired into settings.json** |
| `.worktreeinclude` | ✗ |

## Gaps vs llmenv

The directory model shows the shape of the work: llmenv covers the
instruction-layer files (`CLAUDE.md`, `rules/`) and MCP fully, partially covers
skills (validate) and hooks (copy without wiring), and does not touch
`settings.json` (beyond a broken stub), `agents/`, `commands/`,
`output-styles/`, or `.worktreeinclude`. See [gap-analysis.md](./gap-analysis.md)
for the prioritized consolidation.
