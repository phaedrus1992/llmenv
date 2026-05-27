# MCP reference

Source: <https://code.claude.com/docs/en/mcp>, plus managed-mcp notes from the
settings docs (fetched 2026-05-27).

MCP (Model Context Protocol) servers extend Claude Code with external tools. This
is the **one config surface llmenv generates fully** (`mcp.json`).

## Configuration locations

| Scope | File |
| --- | --- |
| User | `~/.claude.json` |
| Project (shared, committed) | `.mcp.json` |
| Local (per-project) | `~/.claude.json` |
| Plugin | `<plugin>/.mcp.json` |
| Managed | `managed-mcp.json` in the managed settings dir |

llmenv emits `mcp.json` inside `CLAUDE_CONFIG_DIR`.

## `.mcp.json` / `mcp.json` schema

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

llmenv's `write_mcp_json` (`src/adapter/claude_code.rs:93`) produces correct
`mcpServers` entries: stdio (`command`/`args`/optional `env`) and remote (`url`).
The YAML schema (`McpServer`, `McpTransport`) models stdio/http/sse with
tag-intersection selection, and the `memory`/ICM backend desugars into a resolved
MCP server. This surface is in good shape. Remaining gaps:

1. **Remote `type` is dropped from output.** `write_mcp_json` emits remote servers
   as `{ "url": url }` with no `"type"` field. Claude Code defaults stdio when
   `command` is present and infers remote from `url`, so this likely works — but
   for `sse` vs `http` disambiguation it may be ambiguous. Verify, and consider
   emitting `"type"` explicitly. (`streamable-http` alias exists if needed.)
2. **No remote auth headers.** The schema has no field for `--header` /
   `Authorization` bearer tokens on http/sse servers. Any authenticated remote
   MCP can't be expressed.
3. **No approval policy passthrough.** llmenv generates the server *definitions*
   but nothing sets `enableAllProjectMcpServers` / `enabledMcpjsonServers`. Since
   llmenv writes to a private `CLAUDE_CONFIG_DIR`, servers there are user-scoped
   and auto-trusted, so this may not matter — confirm against the trust model.
4. **Managed MCP allow/deny lists** are unmodeled; out of scope for a personal
   project but relevant if llmenv ever targets shared/enterprise configs.

These are refinements, not the structural rebuild that `settings.json` and hooks
need.
