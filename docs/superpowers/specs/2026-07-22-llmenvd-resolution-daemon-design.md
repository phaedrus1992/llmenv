<!-- markdownlint-disable MD013 -->
# `llmenvd`: a persistent resolution daemon + stdio MCP — Design

Target milestone: **v4.0.0** (Large Feature → branches from `main`).

## Problem

Every llmenv hook and `export` invocation is a **fresh process**, so all the
in-process caching the codebase already carries is cold on every single call:

- `RUNTIME` (`src/hook_run/mod.rs`) — "~3ms builder overhead is paid once per
  session" is only true within one process; a hook fires as a new process each
  turn, so the tokio runtime is rebuilt every time.
- `MCP_CLIENT_CACHE` — the reqwest client (connection pool, TLS state, DNS
  cache) to the remote ICM MCP is rebuilt per process.
- `MERGE_CACHE` / `PRELOADED_CONFIG` — the config parse plus the bundle merge
  (disk I/O and YAML) runs again every process.

On each prompt the binary re-does the full pipeline: `config.yaml` parse →
env detection (network gateway-MAC probe, hostname, project-marker walk) →
scope eval → bundle merge → MCP resolve → materialize hash check. Worse,
`llmenv export` runs on **every shell prompt** via the shell hook — not just on
Claude events — so this cost is paid continuously, whether or not an agent is
even running.

A stdio MCP alone does **not** fix this: a stdio server is owned by a single
client (Claude Code) over that client's stdin/stdout, and the shell hooks are
separate subprocesses that cannot reach that pipe. Warming state for the agent's
tool calls while leaving the shell-hook hot path cold solves the smaller half of
the problem.

## Design

### Overview

Ship a single long-lived process, `llmenvd`, that holds warm resolved state and
warm connections, and serve it to **two** kinds of client:

```text
Claude Code ──stdio(JSON-RPC/MCP)──▶ llmenv mcp  ─┐
                                                   ├─unix socket─▶ llmenvd (warm state)
shell hooks: export / statusline / ─socket client─┘        │
  hook-run / check-stale                                    ├─ config+bundle merge (mtime-keyed)
                                                            ├─ env+scope+tag+bundle resolution
                                                            ├─ materialize manifest+hash+writes
                                                            └─ warm tokio rt + ICM MCP connection
```

The stdio MCP process Claude Code launches is a **thin proxy**: it forwards
agent-facing tool/resource calls into the same unix socket the shell hooks use.
There is exactly one source of warm truth (the daemon); the MCP process holds no
resolution state of its own.

Shipped in the existing binary as an `llmenv daemon` subcommand and an
`llmenv mcp` subcommand. **No new crate, no new dependency** — reuses the tokio
runtime, `serde_json`, and the existing resolution/merge/materialize/ICM code
paths, which are refactored so they can be called both in-process (fallback) and
from inside the daemon.

### Component boundaries

- **`llmenvd` (the daemon)** — owns warm state, binds the socket, serves verbs,
  self-terminates on idle. Does *not* parse CLI args beyond its own start;
  does *not* print to a shell.
- **Socket client (a small module)** — connect-with-timeout, one round-trip,
  deserialize, or signal "fall back". Used by every thin-client command.
- **`llmenv mcp` (stdio proxy)** — the agent-facing MCP server; translates MCP
  tool/resource calls into socket verbs and back.
- **Resolution core (existing code, refactored)** — the pipeline functions
  (`scope::evaluate`, `merge::merge`, `materialize::*`, `hook_run` recall/store)
  become callable as a library both from the daemon and from the in-process
  fallback. This is the bulk of the "don't duplicate logic" work.

### Socket protocol

Length-prefixed JSON request/response over a unix domain socket.

- **Path:** `$XDG_RUNTIME_DIR/llmenv/d.sock`, falling back to
  `<state_dir>/d.sock`. Parent dir created `0700`; socket is owner-only. No
  network exposure, ever.
- **Framing:** 4-byte big-endian length prefix + a JSON body. One request, one
  response, per connection (connection-per-call keeps the daemon stateless
  between calls and sidesteps multiplexing).
- **Verbs:**
  - `resolve { cwd, env_hints }` → `{ active_scopes, tags, bundles, project,
    project_root, export_lines, icm_context }` — everything `export` needs.
  - `status { kind }` → the data `llmenv status`/`statusline` render.
  - `materialize { }` → performs (or confirms) materialization, returns the
    content-hashed dir + drift verdict (for `check-stale`).
  - `recall { query }` / `store { chunk }` → proxied through the warm ICM
    connection.
  - `log { event }` → session-log write (JSONL + ICM transcript).
  - `invalidate { reason }` → drop caches (fired by mutating CLI commands).
- **Versioning:** every response carries the daemon's `llmenv` version. A client
  whose own version differs treats the daemon as unavailable (falls back) — a
  version skew means the daemon may be running stale resolution logic.

This is deliberately **not** MCP JSON-RPC on the wire. MCP framing is reserved
for the agent-facing stdio side; internally a flat verb protocol is smaller,
easier to version-gate, and has no handshake cost.

### Degradation contract (hard invariant)

The daemon is a **pure optimization**. Correctness must never depend on it.

- Every client connects with a **50 ms** budget. Any of {socket missing, connect
  timeout, version mismatch, malformed response, error response} → the client
  silently falls back to the **exact in-process path that exists today**.
- The current resolution/hook code becomes the shared fallback library — the
  daemon calls the same functions, so daemon-path and fallback-path results are
  the same code, not two implementations that can drift.
- `LLMENV_NO_DAEMON=1` forces the fallback (kill-switch for debugging and for
  users who don't want a daemon).
- **Parity test:** an integration test asserts that `export` (and `status`,
  `check-stale`) produce **byte-identical** output with the daemon running and
  with `LLMENV_NO_DAEMON=1`. This is the acceptance gate for "degrades
  gracefully", not a hope.

### Spawn + lifecycle

- **Auto-spawn:** the first client to find the socket missing double-forks a
  detached `llmenv daemon` and retries connect for ~200 ms, then proceeds
  (warm if it connected, fallback if not — either way the call succeeds).
- **Spawn lock:** binding the socket *is* the lock. A losing race just fails to
  bind and exits; no separate lockfile.
- **Stale socket:** connect-refused on an existing socket path → the previous
  owner died; unlink and respawn.
- **Idle shutdown:** self-terminate after **N minutes** with no requests
  (default 10, configurable via `config.yaml`). Keeps a laptop from carrying an
  idle daemon forever.
- **User-scoped:** one daemon per user (socket path is per-user). Multiple
  terminals and multiple concurrent agent sessions share it.

### Invalidation

- **Config:** resolution keyed on `config.yaml` mtime (already the pattern in
  `merge_cache_key`), so an edit is picked up without an explicit signal.
- **cwd:** sent by the client per request; the daemon caches resolution
  per-cwd.
- **Network identity:** re-probed on a short TTL (~5 s) so gateway-MAC / network
  changes are noticed without probing on every prompt.
- **Explicit:** mutating CLI commands (`edit`, `regenerate`, `login`,
  `plugin-sync`) fire an `invalidate` after they run. `upgrade` additionally
  shuts the daemon down (the new binary respawns a fresh one on next call), so a
  daemon never runs resolution logic from an old version.

### Security

- Socket dir `0700`, socket owner-only; unix domain only, no TCP.
- Follows the existing owner-only atomic-write pattern
  (`write_owner_only_atomic`).
- The daemon accepts only the fixed verb set; no arbitrary command execution
  over the socket. cwd/env hints from clients are treated as untrusted input and
  validated the same way the in-process path validates them (tag/bundle
  character rules, path canonicalization).

### Feature split

**Bucket 1 — warm inside the daemon (the recompute killers):**

- config parse + bundle merge (mtime-keyed)
- env detection + scope/tag/bundle evaluation
- materialization manifest + content hash + **writes**
- warm tokio runtime + warm ICM MCP connection

**Bucket 2 — thin clients that fetch resolved state, still run per-invocation:**

- `export` — still prints env lines to the shell's stdout, but fetches the
  resolved result instead of computing it
- `statusline` — fetches warm status data
- `check-stale` / `config-context` — fetch drift/context
- `hook-run` recall/store — routed through the daemon's warm ICM connection
- session-log writes — routed through the daemon

**Bucket 3 — CLI-only one-shots, never in the daemon, but signal invalidation:**

- `init`, `setup`, `edit`, `login`, `upgrade`, `completions`, `plugin-sync`,
  `prune`, `doctor --gc`, `regenerate`

### Agent-facing stdio MCP surface

- **Resources:** resolved context (active scopes/tags/bundles/project),
  materialized config-dir path, drift status.
- **Tools:** `llmenv_context` (the resolved environment) and `llmenv_why`
  (explain why a given tag/bundle is active). Small, read-only surface.
- **Not** re-exposing ICM memory — ICM already ships its own MCP; duplicating it
  would fork the memory surface.

## Non-goals

- **Windows named-pipe transport** — unix socket only for v4.0.0; Windows
  support is a later addition if demand appears.
- **Cross-user / shared-host daemon** — strictly per-user.
- **Persisting warm state across daemon restarts** — the daemon is a cache; a
  cold start just recomputes.
- **Moving mutating commands into the daemon** — one-shots stay CLI-only.

## Testing strategy

- **Parity (acceptance gate):** daemon-on vs `LLMENV_NO_DAEMON=1` produce
  byte-identical `export`/`status`/`check-stale` output.
- **Degradation:** kill the daemon mid-session, assert the next hook succeeds
  via fallback with no user-visible error; corrupt/downgrade the version, assert
  fallback.
- **Lifecycle:** spawn race (N clients, one daemon binds), stale-socket cleanup,
  idle shutdown fires.
- **Invalidation:** config edit picked up via mtime; `invalidate` drops caches;
  network TTL re-probe.
- **Security:** socket perms, verb allow-list rejects unknown verbs, malformed
  frames rejected.
- Property/behavior tests on the resolution core stay valid — it's the same
  functions, exercised through both entry points.

## Decomposition (implementation sub-issues, filed when work starts)

1. **Daemon skeleton + socket + lifecycle** — `llmenv daemon`, socket bind/framing,
   auto-spawn, spawn-lock, stale-socket cleanup, idle shutdown, version handshake,
   `LLMENV_NO_DAEMON` kill-switch.
2. **Resolution caching in the daemon** — `resolve`/`status`/`materialize` verbs
   backed by mtime + per-cwd + network-TTL caches; refactor the resolution core
   into a shared library callable from both daemon and fallback.
3. **`export` + `statusline` thin clients + degradation** — plus the parity test
   suite (the acceptance gate).
4. **`hook-run` warm path** — `recall`/`store`/`log` verbs over the warm ICM
   connection; `check-stale`/`config-context` thin clients.
5. **stdio MCP proxy + agent tools** — `llmenv mcp`, context resources,
   `llmenv_context` / `llmenv_why` tools.
6. **Invalidation wiring + docs** — mutating commands fire `invalidate`/shutdown;
   `website/docs/` daemon page; CHANGELOG entry under `[Unreleased]`.

## Open questions (defaults chosen; revisit during implementation)

- Idle-shutdown default (10 min) and network-probe TTL (5 s) are guesses — tune
  against real measurements once the daemon exists.
- Whether `doctor` should gain a `daemon` check (is it running, version, socket
  health). Likely yes, folded into sub-issue 6.
