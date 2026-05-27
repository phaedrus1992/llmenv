# Environment variables reference

Source: <https://code.claude.com/docs/en/env-vars> (fetched 2026-05-27).

Env vars control Claude Code behavior without editing settings. **Any variable can
also be set under the `env` key of `settings.json`** to apply it to every session.
The reference lists ~247 variables. Categories below; see the source for the full
table.

## Categories

**Auth / API routing**
`ANTHROPIC_API_KEY`, `ANTHROPIC_AUTH_TOKEN`, `ANTHROPIC_BASE_URL`,
`ANTHROPIC_CUSTOM_HEADERS`, `ANTHROPIC_BETAS`. Cloud: `CLAUDE_CODE_USE_BEDROCK`,
`CLAUDE_CODE_USE_VERTEX`, `ANTHROPIC_BEDROCK_*`, `ANTHROPIC_VERTEX_*`,
`ANTHROPIC_AWS_*`, `ANTHROPIC_FOUNDRY_*`, `AWS_BEARER_TOKEN_BEDROCK`.

**Model selection**
`ANTHROPIC_MODEL`, `ANTHROPIC_SMALL_FAST_MODEL` (deprecated),
`ANTHROPIC_DEFAULT_{OPUS,SONNET,HAIKU}_MODEL[_NAME|_DESCRIPTION|_SUPPORTED_CAPABILITIES]`,
`ANTHROPIC_CUSTOM_MODEL_OPTION[_NAME|_DESCRIPTION]`.

**Config / directories**
`CLAUDE_CONFIG_DIR` (the one llmenv sets — points Claude Code at its config root),
`CLAUDE_CODE_API_KEY_HELPER_TTL_MS`, `CLAUDE_CODE_OTEL_HEADERS_HELPER_DEBOUNCE_MS`,
`CLAUDE_PROJECT_DIR` / `CLAUDE_PLUGIN_ROOT` (path interpolation in hooks/plugins).

**Limits / timeouts**
`API_TIMEOUT_MS`, `BASH_DEFAULT_TIMEOUT_MS`, `BASH_MAX_TIMEOUT_MS`,
`BASH_MAX_OUTPUT_LENGTH`, `CLAUDE_ASYNC_AGENT_STALL_TIMEOUT_MS`, `MAX_*` token
caps.

**Feature toggles**
`CLAUDE_CODE_DISABLE_*`, `DISABLE_TELEMETRY`, `CLAUDE_AGENT_SDK_DISABLE_BUILTIN_AGENTS`,
`CLAUDE_CODE_ENABLE_*`, color/accessibility (`NO_COLOR`, `FORCE_COLOR`,
`CLAUDE_CODE_ACCESSIBILITY`).

**Telemetry**
`OTEL_*` (OpenTelemetry exporters/headers/resource attributes).

## Gaps vs llmenv

- llmenv sets exactly one env var: `CLAUDE_CONFIG_DIR` (via
  `ClaudeCodeAdapter::env_vars`, `src/adapter/claude_code.rs:28`), pointing Claude
  Code at the materialized config dir. Correct and necessary.
- The **`env` settings key** (apply env vars to every session) is unmodeled — and
  it is the more durable way to set things like `ANTHROPIC_MODEL`, timeouts, or
  `DISABLE_TELEMETRY` than exporting shell vars. Ride this on the `settings.json`
  generator.
- Per-scope env is a natural fit: e.g. a work network scope sets a gateway
  `ANTHROPIC_BASE_URL`; a host scope sets a model default. Worth a design note on
  whether env belongs in bundles (merged `env` maps) or a dedicated block.
- Caution: `env` in settings vs the shell env llmenv exports may interact;
  document precedence (settings `env` applies to spawned subprocesses).
