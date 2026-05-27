# MCP servers in Codex

Codex registers MCP servers as **`[mcp_servers.<id>]` tables inside
`config.toml`** — not in a sidecar JSON file. This is the single most important
structural difference for a `CodexAdapter`: where `ClaudeCodeAdapter` writes a
separate `mcp.json`, a Codex adapter renders TOML tables into the central file.

## STDIO servers

```toml
[mcp_servers.context7]
command = "npx"                      # required
args = ["-y", "@upstash/context7-mcp"]
env_vars = ["LOCAL_TOKEN"]           # forward/allow these env vars
cwd = "/path/to/dir"                 # working directory (optional)
experimental_environment = "remote"  # run via remote executor (optional)

[mcp_servers.context7.env]           # explicit env values (optional)
MY_ENV_VAR = "MY_ENV_VALUE"
```

- `command` (required), `args`, `env` (explicit map), `env_vars` (allowlist to
  forward from Codex's environment), `cwd`, `experimental_environment`.
- `env_vars` entries may be plain names or `{ name = "X", source = "remote" }`.
  `source = "local"` (default) reads Codex's local env; `source = "remote"`
  reads the remote executor env and requires remote MCP stdio.

## Streamable HTTP servers

```toml
[mcp_servers.figma]
url = "https://mcp.figma.com/mcp"        # required
bearer_token_env_var = "FIGMA_OAUTH_TOKEN"
http_headers = { "X-Figma-Region" = "us-east-1" }      # static header values
env_http_headers = { "X-Feat" = "FEAT_ENV" }           # header values from env
```

- `url` (required), `bearer_token_env_var`, `http_headers`, `env_http_headers`.

## Tool gating & approval (per server)

```toml
[mcp_servers.chrome_devtools]
url = "http://localhost:3000/mcp"
enabled_tools = ["open", "screenshot"]
disabled_tools = ["screenshot"]   # applied AFTER enabled_tools
default_tools_approval_mode = "prompt"   # auto | prompt | approve
startup_timeout_sec = 20
tool_timeout_sec = 45
enabled = true

[mcp_servers.chrome_devtools.tools.open]
approval_mode = "approve"          # per-tool override
```

## OAuth

`codex mcp login` runs an OAuth flow with a local callback server:

```toml
mcp_oauth_callback_port = 5555
mcp_oauth_callback_url = "https://devbox.example.internal/callback"
```

`mcp_oauth_credentials_store` = `auto | file | keyring` selects where OAuth
tokens are stored.

## Comparison to llmenv's `McpServer`

| llmenv field | Codex equivalent | Notes |
| --- | --- | --- |
| `name` | table key `[mcp_servers.<name>]` | |
| `command` | `command` | stdio |
| `args` | `args` | |
| `env` (map) | `[mcp_servers.<name>.env]` | |
| `url` | `url` | HTTP transport |
| `transport` (`stdio`/`http`/`sse`) | implicit: presence of `command` vs `url` | Codex has **no `sse`** — only stdio + streamable HTTP |

Codex distinguishes stdio vs HTTP by **which keys are present**, not an explicit
`type`. There is **no SSE transport** — llmenv's `McpTransport::Sse` has no Codex
target and a `CodexAdapter` would need to reject or down-map it.

## Gaps vs llmenv

A `CodexAdapter` would:

- **Render `[mcp_servers.*]` into `config.toml`**, not a separate file. The
  existing `write_mcp_json` logic (`src/adapter/claude_code.rs:93`) is
  Claude-shaped and not reusable; Codex needs a TOML serializer that nests MCP
  tables alongside everything else.
- **Map the memory backend** the same way as Claude Code — ICM desugars to an
  MCP server, which becomes an `[mcp_servers.icm]` (or proxy URL) table. The
  `{{ICM_MCP}}` substitution model carries over conceptually.
- **Handle the transport mismatch**: llmenv allows `sse`; Codex doesn't. Either
  reject at config-validation time or map `sse`→streamable-HTTP with a warning.
- **Surface Codex-only features llmenv can't express**: per-tool approval modes,
  `enabled_tools`/`disabled_tools` gating, `env_vars` allowlists with
  local/remote sourcing, startup/tool timeouts. These would need schema
  additions if we want them generated.
