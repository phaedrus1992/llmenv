<!-- markdownlint-disable MD013 -->
# OpenAI Codex CLI configuration reference

Reference notes on the **OpenAI Codex CLI's** configuration surfaces, captured
from the official docs at <https://developers.openai.com/codex/> (fetched
2026-05-27), and analyzed against what **llmenv** would need to support Codex.
This mirrors the `docs/reference/claude-code/` set.

These are working references for future design docs. They err on the side of too
much detail. Each page ends with a **Gaps vs llmenv** section.

## The greenfield framing

**llmenv has no Codex adapter today.** The only adapter is
`ClaudeCodeAdapter` (`src/adapter/claude_code/mod.rs`). So unlike the Claude Code
reference — where "gaps" means "what the existing adapter stubs or skips" — the
Codex gaps describe **what a future `CodexAdapter` would have to generate from
scratch**.

The good news: llmenv's selection model (tag intersection across
network/host/user/project scopes, applied uniformly to bundles, MCP servers, and
the memory backend) maps cleanly onto Codex. The work is in the **adapter**, not
the config model — though Codex's TOML surface needs a few schema additions
(approval/sandbox policy, model providers, profiles).

## Codex vs Claude Code: the structural differences that matter for an adapter

| Dimension | Claude Code | OpenAI Codex CLI |
| --- | --- | --- |
| Config format | JSON (`settings.json`) | **TOML** (`config.toml`) |
| Config home | `CLAUDE_CONFIG_DIR` (default `~/.claude`) | `CODEX_HOME` (default `~/.codex`) |
| Instructions file | `CLAUDE.md` | `AGENTS.md` |
| Rules | `rules/*.md` with path-glob frontmatter | folded into `AGENTS.md` layering (no separate rules dir) |
| MCP config | `.mcp.json` / `~/.claude.json` (`mcpServers`) | `[mcp_servers.<id>]` tables **inside `config.toml`** |
| Layered config | settings precedence (enterprise→cli→local→project→user) | TOML precedence (cli→project `.codex/`→profile→user→system→defaults) |
| Profiles | (none — single settings merge) | **named `[profiles.<name>]` tables**, selected with `--profile` |
| Model providers | (fixed) | **`[model_providers.<id>]`** — custom base URLs, wire API, auth |
| Sandbox | OS sandbox via permissions | **first-class `sandbox_mode` + `[sandbox_workspace_write]`** |
| Hooks | ~25 events, 5 handler types | subset of events, **command hooks only** |

The single biggest adapter consequence: **MCP servers live inside the main TOML
file**, not a sidecar JSON. A `CodexAdapter` would render `config.toml` with
embedded `[mcp_servers.*]` tables — there is no Codex equivalent of writing
`mcp.json` separately.

## What a CodexAdapter would materialize (proposed)

| Artifact | Source | Notes |
| --- | --- | --- |
| `$CODEX_HOME/config.toml` | `settings` + `mcp` + `memory` | the central file; embeds MCP, model, sandbox, approval, hooks |
| `$CODEX_HOME/AGENTS.md` | `manifest.agents_md` (+ rules folded in) | Codex has no `rules/` dir; rules become AGENTS.md sections |
| `$CODEX_HOME/<profile>.config.toml` | per-scope profiles (optional) | if scopes map to Codex profiles |
| skills folders (`SKILL.md`) | bundle files | referenced via `[[skills.config]]` `path` |
| hooks (command scripts) | bundle files | registered in `[hooks]` table |

## Pages

- [config-toml](./config-toml.md) — the central `config.toml`: locations, precedence, full key reference
- [model-and-providers](./model-and-providers.md) — `model`, `model_provider`, `[model_providers.*]`, auth
- [sandbox-and-approvals](./sandbox-and-approvals.md) — `sandbox_mode`, `[sandbox_workspace_write]`, `approval_policy`
- [mcp](./mcp.md) — `[mcp_servers.*]` stdio + streamable HTTP, OAuth, tool gating
- [agents-md](./agents-md.md) — `AGENTS.md` discovery, layering, project-root detection
- [hooks](./hooks.md) — `[hooks]` table, events, command-only handlers
- [profiles](./profiles.md) — `[profiles.*]` and `--profile`
- [skills](./skills.md) — `[skills.config]`, SKILL.md
- [shell-env-policy](./shell-env-policy.md) — `[shell_environment_policy]`
- [auth](./auth.md) — `codex login`, `~/.codex/auth.json`, credential stores
- [env-vars](./env-vars.md) — `CODEX_HOME` and friends
- [enterprise](./enterprise.md) — `requirements.toml`, managed constraints
- [gap-analysis](./gap-analysis.md) — **consolidated** what a CodexAdapter must build
