# Authentication

Codex authenticates via `codex login` (ChatGPT sign-in or API key) and caches
credentials locally. Claude Code's auth is handled separately by the Claude Code
binary, so this is mostly **out of scope for llmenv** — but the adapter needs to
not stomp on it and may need to coexist with `auth.json`.

## Credential cache

- Cached at **`~/.codex/auth.json`** (plaintext) or an OS-specific credential
  store. The CLI and IDE extension share the same cache.
- ChatGPT sessions refresh tokens automatically before expiry.
- Treat `auth.json` like a password — don't commit it, share it, or paste it.

## Headless / CI

- Run `codex login` on a machine with a browser, then copy `~/.codex/auth.json`
  to the headless machine (scp / ssh / `docker cp`).
- `mcp_oauth_credentials_store` (`auto | file | keyring`) selects where MCP OAuth
  tokens are stored.
- API-key auth via env (`OPENAI_API_KEY` / provider `env_key`) is an alternative
  to ChatGPT login.

## Gaps vs llmenv

- **llmenv should not manage Codex auth.** `auth.json` is user-owned secret
  state; the adapter must **never** generate or overwrite it, and must never
  commit it. This mirrors the existing rule that llmenv doesn't manage Claude
  Code login.
- The one intersection: provider `env_key` and MCP `bearer_token_env_var` name
  *environment variables* the user supplies. llmenv can reference those names in
  generated config but must not embed the secret values. Same discipline as the
  MCP `env` story today.
- `CODEX_HOME` placement matters: if llmenv points Codex at a managed config dir
  (the Codex analog of `CLAUDE_CONFIG_DIR`), it must ensure `auth.json` either
  lives there already or is reachable — otherwise login state is lost. This is a
  design decision for the adapter's `env_vars`/home-dir handling.
