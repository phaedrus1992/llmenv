<!-- markdownlint-disable MD013 -->
# Environment variables

Codex reads a handful of environment variables. The one that matters most for an
adapter is `CODEX_HOME` — the analog of Claude Code's `CLAUDE_CONFIG_DIR`.

| Variable | Purpose |
| --- | --- |
| `CODEX_HOME` | Config home; default `~/.codex`. Holds `config.toml`, `auth.json`, `AGENTS.md`, `history.jsonl`, `log/`, profile files |
| `OPENAI_API_KEY` | API-key auth (and API-priced image generation) |
| provider `env_key` (e.g. `AZURE_OPENAI_API_KEY`) | per-provider API key, named by `model_providers.<id>.env_key` |
| MCP `bearer_token_env_var` (e.g. `FIGMA_OAUTH_TOKEN`) | bearer token for a streamable-HTTP MCP server |
| MCP `env_vars` entries | named vars forwarded into stdio MCP subprocesses |

## CLI overrides (not env, but adjacent)

Codex also takes `-c`/`--config key=value` for one-off TOML overrides
(dot-notation for nested keys; values parsed as TOML), and dedicated flags like
`--model`, `--profile`. These are runtime, not config-file, surfaces.

## Gaps vs llmenv

The key adapter decision is the **`CODEX_HOME` analog of
`CLAUDE_CONFIG_DIR`**. `ClaudeCodeAdapter::env_vars` (`src/adapter/claude_code.rs:28`)
sets `CLAUDE_CONFIG_DIR` to the managed dir it materializes into. A `CodexAdapter`
would do the equivalent: set `CODEX_HOME` to its managed directory and write
`config.toml` + `AGENTS.md` there.

The consequence (see [auth](./auth.md)): pointing `CODEX_HOME` at a fresh managed
dir means `auth.json` and `history.jsonl` won't be where the user expects. The
adapter must either preserve/relocate those, or accept that login state is
per-managed-home. This is the central env-var design question for Codex support.

Secret env vars (`OPENAI_API_KEY`, provider keys, MCP bearer tokens) follow the
same rule as today: llmenv references the **names** in generated config but never
embeds **values**.
