# `config.toml` — the central Codex config

Codex CLI reads a single TOML file as its primary configuration surface. Unlike
Claude Code (which splits `settings.json`, `.mcp.json`, and `CLAUDE.md`), Codex
folds **almost everything** — model selection, sandbox, approvals, MCP servers,
model providers, hooks, history, profiles — into `config.toml`. Only the
free-text instructions live elsewhere (`AGENTS.md`).

## File locations and precedence

Codex resolves values in this order (**highest precedence first**):

1. **CLI flags** and `-c`/`--config` overrides
2. **Project config**: `.codex/config.toml`, walked from the project root down to
   the cwd — *closest wins*; **trusted projects only**
3. **Profile** files selected with `--profile <name>`
   (`$CODEX_HOME/<name>.config.toml`)
4. **User config**: `$CODEX_HOME/config.toml` (default `~/.codex/config.toml`)
5. **System config** (if present): `/etc/codex/config.toml` on Unix
6. **Built-in defaults**

`CODEX_HOME` defaults to `~/.codex`. If a project is marked **untrusted**, Codex
skips *all* project-scoped `.codex/` layers (project-local config, hooks, rules)
but still loads user/system config.

On managed machines, an org can enforce constraints via `requirements.toml`
(e.g. disallowing `approval_policy = "never"` or
`sandbox_mode = "danger-full-access"`). See [enterprise](./enterprise.md).

### One-off overrides

`-c key=value` / `--config key=value` sets a single value (TOML quoting rules
apply). Highest precedence of all.

## Keys that project-local config **cannot** override

Project-scoped `.codex/config.toml` is restricted — security-sensitive keys can
only be set at user/system level (e.g. `sandbox_mode`, `approval_policy`,
provider auth, login-shell allowances). This matters for an adapter: writing
those keys into a project layer is silently ignored.

## Key reference (selected, grouped)

This is the surface a `CodexAdapter` would need to model. Codex's full
`config.toml` is large (~100+ keys); these are the ones relevant to what llmenv
generates.

### Model & session
| Key | Type / values | Notes |
| --- | --- | --- |
| `model` | string | active model id (e.g. `gpt-5.4`) |
| `model_provider` | string | provider id; defaults to `openai` |
| `model_instructions_file` | path | overrides default system instructions |
| `model_reasoning_effort` | string | reasoning budget hint |
| `service_tier` | string | `flex`, `fast`, or catalog tier id |
| `show_raw_agent_reasoning` | boolean | surface raw reasoning |
| `review_model` | string | model used by `/review` |

### Sandbox & approvals — see [sandbox-and-approvals](./sandbox-and-approvals.md)
| Key | Type / values |
| --- | --- |
| `sandbox_mode` | `read-only \| workspace-write \| danger-full-access` |
| `sandbox_workspace_write.writable_roots` | `array<string>` |
| `sandbox_workspace_write.network_access` | boolean |
| `sandbox_workspace_write.exclude_tmpdir_env_var` | boolean |
| `sandbox_workspace_write.exclude_slash_tmp` | boolean |
| `approval_policy` | `untrusted \| on-request \| on-failure \| never \| { granular = {...} }` |
| `approvals_reviewer` | `user \| auto_review` |
| `allow_login_shell` | boolean |

### MCP — see [mcp](./mcp.md)
`[mcp_servers.<id>]` tables with `command`/`args`/`env`/`env_vars`/`cwd` (stdio)
or `url`/`bearer_token_env_var`/`http_headers` (streamable HTTP), plus per-tool
gating. `mcp_oauth_callback_port`, `mcp_oauth_callback_url`,
`mcp_oauth_credentials_store` configure OAuth login.

### Model providers — see [model-and-providers](./model-and-providers.md)
`[model_providers.<id>]`: `name`, `base_url`, `wire_api` (`responses`/`chat`),
`env_key`, `http_headers`, `env_http_headers`, `query_params`,
`request_max_retries`, `stream_max_retries`, `stream_idle_timeout_ms`, and
`[model_providers.<id>.auth]` command-backed token fetch. `openai_base_url`
overrides the built-in OpenAI provider.

### Shell environment — see [shell-env-policy](./shell-env-policy.md)
`[shell_environment_policy]`: `inherit` (`none`/`core`/`all`), `set`, `exclude`,
`include_only`, `ignore_default_excludes`, `experimental_use_profile`.

### Hooks — see [hooks](./hooks.md)
`[hooks]` table keyed by event; **command handlers only**.

### Instructions & project discovery — see [agents-md](./agents-md.md)
| Key | Type | Notes |
| --- | --- | --- |
| `project_root_markers` | array | default `[".git"]`; `[]` disables walking |
| `project_doc_max_bytes` | integer | per-`AGENTS.md` read cap |
| `project_doc_fallback_filenames` | array | extra filenames when `AGENTS.md` absent |
| `instructions` | string | **reserved**; prefer `model_instructions_file`/AGENTS.md |

### History, notifications, state
| Key | Type | Notes |
| --- | --- | --- |
| `history.persistence` | `save-all \| none` | local transcript persistence |
| `history.max_bytes` | integer | cap; drops oldest + compacts |
| `notify` | array | external program for notifications |
| `tui.notifications` | bool/array | built-in TUI notifications, optional event filter |
| `tui.notification_method` | `auto \| osc9 \| bel` | |
| `tui.notification_condition` | `unfocused \| always` | |
| `log_dir` | path | default `$CODEX_HOME/log` |
| `sqlite_home` | path | SQLite-backed agent-job state |

### Profiles — see [profiles](./profiles.md)
`profile = "<name>"` selects a default profile; `[profiles.<name>]` tables hold
overrides. `--profile` selects at runtime.

### Skills — see [skills](./skills.md)
`[[skills.config]]` entries with `path` (folder containing `SKILL.md`) and
`enabled`.

### Feature flags
`[features]` table of boolean toggles (`web_search`, `shell_tool`,
`unified_exec`, `undo`, `shell_snapshot`, `browser_use`, `computer_use`, …),
some stable, some experimental.

## Gaps vs llmenv

llmenv has **no Codex adapter**, so every key above is a gap in the sense that
nothing generates it yet. The schema-level gaps (things llmenv's `config.yaml`
can't currently express even conceptually) are:

- **No model selection** — llmenv's `Settings` is about cache/sync, not the
  agent's model. A `CodexAdapter` needs `model`/`model_provider` inputs.
- **No sandbox/approval vocabulary** — would need new schema fields or a
  Codex-specific block.
- **No model-provider concept** — custom base URLs / wire API / auth are
  entirely unmodeled.
- **No profiles** — llmenv resolves to a *single* merged manifest per
  invocation; Codex profiles are a named-variant mechanism with no llmenv analog
  (though scopes could plausibly *generate* profiles).

What maps cleanly: **MCP servers** (`mcp:` → `[mcp_servers.*]`), the **memory
backend** (desugars to an MCP server, same as Claude Code), and **AGENTS.md**
(`manifest.agents_md` → `AGENTS.md`).
