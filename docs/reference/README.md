# Reference

Per-tool reference notes on the configuration surfaces llmenv targets, each
analyzed against what llmenv currently materializes. These are the basis for
future design docs and err on the side of too much detail.

- [claude-code/](./claude-code/) — Claude Code config surfaces (settings, hooks,
  permissions, skills, subagents, MCP, memory, statusline, plugins, env vars) and
  a consolidated gap analysis vs llmenv.
- [codex/](./codex/) — OpenAI Codex CLI config surfaces (`config.toml`, model
  providers, sandbox/approvals, MCP, AGENTS.md, hooks, profiles, skills, auth,
  env vars, enterprise) and a consolidated gap analysis framed as what a
  greenfield `CodexAdapter` would need to build.
- [pi-durable-evaluation.md](./pi-durable-evaluation.md) — evaluation of the pi
  coding agent and pi-durable against llmenv + Claude Code: hooks, determinism,
  token efficiency, multi-project scopes, multi-channel access, model routing,
  and a recommendation (#2372).
- [context-management-evaluation.md](./context-management-evaluation.md) — evaluation of
  context-management research (Context Language Models, pi-clm, connectome, open-strix,
  compaction and forgetting posts, salience and subliminal-learning papers) against
  llmenv: technique inventory, gaps found, fit per goal, and an ordered recommendation
  (#2377).
