<!-- markdownlint-disable MD013 -->
# Consolidated gap analysis — what a CodexAdapter must build

llmenv has **no Codex adapter**. This page consolidates the per-surface findings
into a single picture of what a greenfield `CodexAdapter` would need, and how
hard each piece is given llmenv's current model.

## Coverage matrix

| Surface | Codex target | llmenv source today | Effort | Notes |
| --- | --- | --- | --- | --- |
| Instructions | `AGENTS.md` | `manifest.agents_md` | **Trivial** | direct map |
| Rules | (folded into `AGENTS.md`) | `manifest.rules` | **Low (lossy)** | Codex has no rules dir; concat as sections, drop glob frontmatter |
| MCP servers | `[mcp_servers.*]` in `config.toml` | `manifest.mcps` | **Medium** | TOML-in-main-file, not sidecar; no `sse` transport |
| Memory backend | `[mcp_servers.icm]` (proxy URL) | `manifest` memory desugar | **Low** | same desugar as Claude Code |
| Model | `model` / `model_provider` | — | **Medium (schema)** | new input vocabulary |
| Model providers | `[model_providers.*]` | — | **Medium (schema)** | base URL, wire API, auth command |
| Sandbox | `sandbox_mode`, `[sandbox_workspace_write]` | — | **High (schema)** | net-new domain; user-level only |
| Approvals | `approval_policy` (+ granular) | — | **High (schema)** | net-new domain; user-level only |
| Hooks | `[hooks]` (command-only) | bundle `hooks/*.json` (Claude-shaped) | **Medium** | not portable; regenerate or skip |
| Skills | `SKILL.md` + `[[skills.config]]` | bundle files (validated) | **Low** | reuse validation; add registration |
| Shell env policy | `[shell_environment_policy]` | — | **Low (optional)** | hardening; can defer |
| Profiles | `[profiles.*]` / `--profile` | (single merged manifest) | **Design** | impedance mismatch; likely ignore |
| Auth | `~/.codex/auth.json` | — | **None (out of scope)** | never generate/commit |
| Env / home | `CODEX_HOME` | `CLAUDE_CONFIG_DIR` pattern | **Low** | mirror existing `env_vars` approach |
| Enterprise | `requirements.toml` (consumed, not written) | — | **None** | output is a request, may be clamped |

## The big structural facts

1. **One TOML file, not three.** Codex folds MCP, model, sandbox, approvals,
   hooks, profiles into `config.toml`. The `CodexAdapter` needs a TOML serializer
   that composes all of these — there is no separate `mcp.json` to reuse the
   existing `write_mcp_json` logic for.

2. **Rules are lossy.** Claude Code's path-glob conditional rules have no Codex
   equivalent; they collapse into unconditional `AGENTS.md` prose.

3. **Two new config domains.** Sandbox and approval policy are vocabulary llmenv
   doesn't have at all. These are the highest-effort schema additions and must be
   written to user-level config (project-local is forbidden for these keys).

4. **No SSE.** llmenv's `McpTransport::Sse` has no Codex target — reject or
   down-map to streamable HTTP.

5. **Profiles vs scopes.** Codex selects config at runtime via `--profile`;
   llmenv selects via environment (tag intersection). Recommend generating a
   single resolved `config.toml` and ignoring profiles initially.

## Suggested sequencing for a CodexAdapter

1. **Phase 1 — parity with what llmenv already produces:** `AGENTS.md`
   (+ folded rules), `[mcp_servers.*]` (incl. memory desugar), `CODEX_HOME` env.
   This reuses existing manifest data; no schema changes. Gets a working Codex
   config out the door.
2. **Phase 2 — Codex-specific generation:** `[[skills.config]]` registration,
   skill validation reuse.
3. **Phase 3 — new schema vocabulary:** `model`/`model_provider`,
   `sandbox_mode`/`approval_policy`. These need `config.yaml` schema additions and
   are where the design work concentrates.
4. **Phase 4 — optional hardening / advanced:** `[model_providers.*]`,
   `[shell_environment_policy]`, hooks (or keep deferred, mirroring the inert-hooks
   state of the Claude Code adapter, issue #34).

Profiles and enterprise `requirements.toml` are explicitly **out of initial
scope**.

## What carries over from the Claude Code adapter

- The `env_vars` / managed-home pattern (`CLAUDE_CONFIG_DIR` → `CODEX_HOME`).
- The memory-backend desugar to an MCP server.
- SKILL.md frontmatter validation.
- The discipline of referencing secret env-var **names**, never values.

Everything else is new code: a TOML renderer composing one file, a rules→AGENTS.md
fold, and (eventually) schema for model/sandbox/approval.
