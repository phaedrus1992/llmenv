# MCP reference

Source: <https://code.claude.com/docs/en/mcp>, plus managed-mcp notes from the
settings docs (fetched 2026-05-27).

MCP (Model Context Protocol) servers extend Claude Code with external tools.
llmenv resolves MCP servers and merges them into the **top-level `mcpServers`
object of `.claude.json`** — the surface Claude Code actually reads for
user-scoped servers.

## Configuration locations

| Scope | File |
| --- | --- |
| User | `~/.claude.json` (top-level `mcpServers`) |
| Project (shared, committed) | `.mcp.json` |
| Local (per-project) | `~/.claude.json` (per-project `mcpServers`) |
| Plugin | `<plugin>/.mcp.json` |
| Managed | `managed-mcp.json` in the managed settings dir |

llmenv merges resolved servers into the top-level `mcpServers` of
`.claude.json` inside `CLAUDE_CONFIG_DIR`. `.claude.json` is overwhelmingly
foreign Claude state (`oauthAccount`, `projects`, `numStartups`, caches), so the
adapter does a **read-merge-write**: it upserts llmenv servers by name into
`mcpServers` and preserves every other key. A corrupt or non-object
`.claude.json` is a hard error — the adapter refuses to overwrite rather than
destroy Claude's session state. (#244 — the previously-emitted `mcp.json` in
`CLAUDE_CONFIG_DIR` was never ingested by Claude and is no longer written.)

## `mcpServers` schema

```json
{
  "mcpServers": {
    "shared-server": {
      "command": "/path/to/server",
      "args": [],
      "env": {}
    },
    "remote-api": {
      "type": "http",
      "url": "https://api.example.com/mcp"
    }
  }
}
```

Keyed by server name. Plugin servers may use `${CLAUDE_PLUGIN_ROOT}` and `${VAR}`
interpolation.

## Transports

| Transport | How to declare | Notes |
| --- | --- | --- |
| stdio | `command` + `args` + `env` | Local subprocess. Default. |
| http | `type: "http"` + `url` (+ headers) | Recommended for remote. `streamable-http` is an accepted alias. |
| sse | `type: "sse"` + `url` | **Deprecated** — use http. |

CLI: `claude mcp add --transport http <name> <url> --header "Authorization: Bearer …"`.

## Approval & policy (settings.json)

| Setting | Effect |
| --- | --- |
| `enableAllProjectMcpServers` | Auto-approve all `.mcp.json` servers. |
| `enabledMcpjsonServers` / `disabledMcpjsonServers` | Per-server approve/reject. |
| `allowedMcpServers` / `deniedMcpServers` (M) | Org allow/deny (deny wins). |
| `allowManagedMcpServersOnly` (M) | Lock to managed allowlist. |
| `allowAllClaudeAiMcps` (M) | Load claude.ai connectors alongside managed MCP. |

## Gaps vs llmenv (mostly parity — narrow gaps)

llmenv's `merge_mcp_into_claude_json` (`src/adapter/claude_code.rs`) produces
correct `mcpServers` entries: stdio (`command`/`args`/optional `env`) and remote
(`type` + `url`). The YAML schema (`McpServer`, `McpTransport`) models
stdio/http/sse with tag-intersection selection, and the `memory`/ICM backend
desugars into a resolved MCP server. This surface is in good shape. Remaining
gaps:

1. **No stale-server pruning.** The adapter upserts llmenv servers by name but
   tracks no owned-name set, so a server llmenv stops resolving lingers in
   `.claude.json`'s `mcpServers` until removed by hand. (Deferred follow-up.)
2. **Managed MCP allow/deny lists** are unmodeled; out of scope for a personal
   project but relevant if llmenv ever targets shared/enterprise configs.

The approval-policy settings below (`enableAllProjectMcpServers`,
`enabledMcpjsonServers`) gate project `.mcp.json` servers; llmenv's servers are
user-scoped in a private `CLAUDE_CONFIG_DIR` and auto-trusted, so they need no
approval passthrough.

These are refinements, not the structural rebuild that `settings.json` and hooks
need.
