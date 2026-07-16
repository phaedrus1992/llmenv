# Troubleshooting

Start with the two diagnostic commands, then drill into the specific failure
below.

```bash
llmenv doctor       # validate config + wiring; --gc to also clean the cache
llmenv context      # show resolved scopes/tags/bundles for the current dir
```

## My config isn't being picked up

- **Wrong config path.** llmenv reads `$LLMENV_CONFIG_DIR` if set, otherwise the
  platform config dir (`~/.config/llmenv`). Confirm with `llmenv status`, which
  reports the file it loaded.
- **Parse error.** `llmenv doctor` reports YAML parse failures and missing
  required fields. A common cause is an unquoted value with a colon — see
  [Configuration → YAML gotchas](configuration.md#yaml-gotchas).

## A scope never activates

```bash
llmenv scope-ls     # marks active scopes
llmenv tag-ls       # marks active tags
```

- **Network scopes** match on `gateway_mac` only today (`ssid`/`cidr` are parsed
  but ignored). A VPN or captive network can change or hide the gateway, leaving
  the scope unmatched. Fall back to a **host scope** (matches by hostname, always
  reliable) that emits the same tag.
- **Host scopes** match case-insensitively against the local hostname. Run
  `hostname` and compare.
- **User scopes** match `$USER` exactly.

## A project marker isn't detected

- The `.llmenv.yaml` walk ascends from the current directory **to `$HOME`
  inclusive**, then stops. A marker above `$HOME` is intentionally ignored.
- When `$HOME` is unset, only the current directory itself is checked.
- Confirm detection with `llmenv context` — `LLMENV_ACTIVE_PROJECT` and
  `LLMENV_PROJECT_ROOT` are set only when a marker matched.
- Malformed marker YAML degrades to defaults (id/name from the folder basename)
  and logs a warning; it does not fail the whole resolution.

## A bundle / MCP server / plugin won't fire

These all select by tag intersection. If a contributor's tags aren't in the
active set, it stays dormant.

- Check the active tags with `llmenv tag-ls`.
- `llmenv doctor` flags **orphans**: a contributor whose tags no scope emits, and
  a scope whose tags no contributor consumes.
- To force a bundle on inside a project regardless of tags, add it to the
  marker's `enable_bundles` list.

## The agent is running stale config

After you change config, a running agent keeps the directory it booted with. The
`SessionStart` hook runs `llmenv check-stale`, which compares the booted content
hash against the current one and prints a restart hint on drift. You can run it
manually:

```bash
llmenv check-stale
```

Restart the agent to pick up the new config.

## The cache is growing / I want a clean slate

```bash
llmenv prune --dry-run            # preview
llmenv prune                      # remove old-version folders + orphaned *.tmp
llmenv prune --older-than 14d     # remove current-version folders older than 14d
llmenv prune --all                # nuke everything (re-materializes on next export)
llmenv doctor --gc                # diagnostics + GC in one pass
```

## Memory backend issues

- **Server not activating** — it renders only when one of `memory.tags` is
  active. Check `llmenv tag-ls`.
- **Client can't reach the server** — confirm the `host:` address resolves and
  the port is open: `nc -vz <addr> <port>`.
- **`mcp-proxy` missing** — the server host needs `mcp-proxy` or `uvx` on
  `PATH`. `llmenv export` errors with an install hint if neither is present.

See [MCP & Memory](mcp.md) for the full topology and security model.

## Profiling hook-run latency

Lifecycle hooks run on the agent's hot path, so a slow `hook-run` shows up as
prompt lag. To see where the time goes, set `LLMENV_TRACE_TIMING` (to any value)
before the hook fires. Each `hook-run` that completes the memory/session-log
stage then emits one line to **stderr** (stdout is untouched):

```
llmenv-trace {"config_load_us":123,"scope_eval_us":456,"prep_us":78,"mcp_us":9012}
```

Each value is an integer microsecond count for that phase:

- `config_load_us` — reading and parsing config.
- `scope_eval_us` — evaluating scopes against the environment.
- `prep_us` — everything between scope evaluation and the MCP round-trip:
  building recall queries, generating the context chunk, constructing the MCP
  client (reqwest/TLS on a connection cache miss), building the scope context,
  and the one-time ~3 ms tokio runtime build on the session's first hook-run.
- `mcp_us` — the MCP round-trip plus session logging; usually the dominant term.

Events that early-return before the MCP stage (e.g. a `PreToolUse` with no
active sinks), and runs that error mid-round-trip, emit nothing. The var is off
by default and adds no measurable overhead when unset.

## Sync conflicts

`llmenv sync` runs `git add`/`commit`/`push` on the config repo. If the remote
has diverged, resolve it like any git conflict (pull/rebase, fix, re-run). llmenv
does not force-push.
