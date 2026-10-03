# Issue #2417 — who sent mcp-proxy a shutdown signal with requests in flight

- **Issue:** https://github.com/phaedrus1992/llmenv/issues/2417
- **Milestone:** `v3.12.0`
- **Base branch:** `release/3.x` (forward-merges to `release/4.x`)
- **Type:** investigation, with instrumentation and a conditional fix
- **Related:** #2358 (session-start recovery for a dead proxy, shipped in PR #2415)

This is a spec for an investigation.
It names what to instrument, what to try, and what to do for each outcome.

## Problem

On 2026-10-02 `mcp-proxy.log` shows a graceful shutdown (`Shutting down`, two `ASGI callable returned without completing response`, `Finished server process`) while requests were in flight.
That is a SIGINT, SIGTERM, or SIGHUP, not a crash.
Sessions that started in the next six minutes got `ECONNREFUSED`.
#2358 now restarts a dead proxy at session start, but the cause is unknown.

## Verified facts (release/3.x)

| Fact | Location |
| --- | --- |
| `spawn_mcp_proxy(bind, pid_path)` runs `mcp-proxy --host <ip> --port <port> -- icm serve` (or `uvx mcp-proxy …`), cwd `/`, stderr to `mcp-proxy.log` next to the pidfile (1 MiB rotation), then `configure_detached` | `src/mcp/proxy.rs` |
| `configure_detached` nulls stdin and stdout and calls `detach_process_group` (`process_group(0)`). It does **not** call `setsid`; the doc comment says this avoids libc. The child stays in the spawner's session and inherits its environment and controlling terminal | `src/mcp/proxy.rs` |
| Pidfile `mcp-proxy.pid` and lockfile `mcp-proxy.pid.lock` under the state dir; `ensure_running` probes TCP, then lock → `spawn_and_publish` → `wait_for_bind` (5 s) → `write_pidfile_atomic`; `reconcile_pidfile` removes a pidfile whose pid is dead (`is_alive` uses `kill -0`) | `src/mcp/proxy.rs` |
| Two spawners: `run_export` on every `llmenv export` (so every new shell), and the SessionStart health check `mcp_health::session_start_notice` → `ensure_local_memory_proxy`. A proxy from the first is a child of the user's shell session; a proxy from the second is a child of a `hook-run` under Claude Code's session | `src/cli/mod.rs`, `src/hook_run/mcp_health.rs` |
| llmenv code that signals a process: `proxy::reap` (SIGKILL, only on a proxy llmenv just spawned and that failed to bind), `consolidation::kill_process_group` (the `claude -p` child), `mcp_health::probe_stdio` kill. None target a running proxy | `src/mcp/proxy.rs`, `src/consolidation/mod.rs`, `src/hook_run/mcp_health.rs` |
| The SessionStart notice text from `effect_and_fix(MEMORY_MCP_NAME)` tells the agent: stop a proxy that "runs but does not answer" with `pkill -f 'mcp-proxy .*-- icm serve'`. It appears whenever the probe fails, including when `restart_note` reports `AlreadyRunning`. An agent that follows it sends SIGTERM to every matching proxy on the machine | `src/hook_run/mcp_health.rs` |
| No `llmenv stop` command, no SessionEnd code touches the proxy, no signal handlers, no launchd or systemd unit | `src/`, repo root |
| `rustix` is a dependency with the `process` feature; `rustix::process::setsid` is available without libc | `Cargo.toml` |
| `tests/mcp_proxy.rs` and `tests/hook_run_failsoft.rs` use `kill -9` on stand-ins in temp dirs; they never reach a real proxy | tests |

## Hypotheses, most likely first

1. **An agent followed the `pkill` advice.**
   The notice fires on any failed probe, including a slow proxy that is alive.
   `pkill -f` is a graceful SIGTERM, which matches the log.
   It also kills a proxy that serves other sessions, which matches "requests in flight".
2. **SIGHUP from the spawning session.**
   With no `setsid`, a proxy spawned by `llmenv export` in a terminal shares that terminal's session.
   Closing the terminal sends SIGHUP to the session.
   A new process group does not shield it from that.
   A proxy spawned from a Claude Code hook shares Claude Code's session and may get the same on exit.
3. **An external actor** (an upgrade of `mcp-proxy` or `icm` by `uv tool`, a login-shell cleanup, a system sleep hook).
   Not llmenv's bug, but worth naming in the docs.

## Decisions

1. **Instrument first, then reproduce, then fix only what the evidence names.**
   No `setsid` change lands on a guess.
2. **Attribution data goes in the existing log.**
   When `spawn_and_publish` succeeds, write one line to `mcp-proxy.log`: time, proxy pid, spawner (`export` or `session-start`), parent pid, session id and process group of the spawner, and the controlling terminal if any.
   This is the only durable record besides the pidfile, and it costs nothing.
3. **The `pkill` advice changes regardless of outcome.**
   Killing by command line is wrong on a machine with more than one proxy or more than one session.
   The notice should tell the agent to run `llmenv doctor` and, if the proxy is wedged, to restart it through llmenv.
   Add a hidden-safe path for that: `ensure_local_memory_proxy` already restarts a dead one; add a `--restart-memory-proxy` flag to `doctor` (or a small `llmenv mcp restart-proxy` subcommand) that signals the pid from the pidfile with SIGTERM, waits up to 5 s, then respawns.
   The notice names that command instead of `pkill`.
   Same change for the codebase-memory notice that suggests `pkill -f cbm-daemon-internal`, pointing to the cbm daemon's own stop command instead.
4. **If SIGHUP from the session is confirmed, add `setsid`.**
   Use `rustix::process::setsid` in a `pre_exec` closure on Unix inside `configure_detached`.
   `pre_exec` is `unsafe`; keep it to one line with a `SAFETY` comment (the closure only calls `setsid`, which is async-signal-safe).
   The project denies `unsafe_code`; use `#[expect(unsafe_code, reason = …)]` on that one function.
5. **If nothing in llmenv is the cause**, write it down: a troubleshooting entry that lists the three hypotheses, what was tested, and that session-start recovery (#2358) is the answer.

## Design

### Instrumentation (`src/mcp/proxy.rs`)

- In `spawn_and_publish`, after `write_pidfile_atomic`, append the attribution line to the proxy log through `open_proxy_log` (the same bounded writer).
- The spawner label comes from a new parameter `SpawnSource { Export, SessionStart }` threaded from the two callers.
  `ensure_running` has 3 parameters today; adding one stays under the limit.
- Parent pid, session id, and process group: `std::process::id()`, `rustix::process::getsid`, `rustix::process::getpgid`.
  Controlling terminal: `rustix::termios::ttyname` on fd 0 when it is a tty, else `none`.

### Reproduction protocol (run by hand, record results in the issue)

For each case, start a long `icm_memory_recall` through the proxy from one session so a request is in flight, then:

1. End the Claude Code session that spawned the proxy (quit normally).
2. Close the terminal window that ran `llmenv export` and spawned the proxy.
3. Send SIGHUP to the shell that spawned it (`kill -HUP <shell pid>`).
4. From a second session, run the exact `pkill` line from the notice.
5. Put the machine to sleep for two minutes and wake it.

Record for each: did the proxy die, what did `mcp-proxy.log` show, and what did the attribution line say about the spawner.
Compare against the 2026-10-02 log entry.

### Conditional fixes

- Hypothesis 1 confirmed or plausible: decision 3 (notice text and the restart path). This change lands in any case.
- Hypothesis 2 confirmed: decision 4 (`setsid`).
- Neither: decision 5 (docs only).

### Docs

- `website/docs/troubleshooting.md`, memory backend section: how to restart the proxy through llmenv, what the attribution line means, tagged `(added in v3.12.0)`.
- Changelog `Changed`: the session-start notice names the llmenv restart path instead of `pkill`; `Added`: the proxy log records who started it. `Fixed` only if `setsid` lands.

## Tests

1. The attribution line: a unit test on the formatter (pure) with each `SpawnSource`.
2. `spawn_and_publish` writes the line: extend the existing stand-in proxy test in `tests/mcp_proxy.rs` to read the log after a spawn.
3. Notice text: `effect_and_fix(MEMORY_MCP_NAME)` no longer contains `pkill`; it names the restart command.
4. Restart path: with a stand-in proxy running, the restart command sends SIGTERM to the pidfile pid, waits for exit, respawns, and the pidfile holds the new pid.
5. If `setsid` lands: a spawned stand-in reports a session id different from the test process (`ps -o sid=`), on Unix only.

## Acceptance criteria

1. The issue has a comment naming the cause, or listing the five reproduction cases with their results and stating that llmenv is ruled out.
2. The notice no longer suggests `pkill`, and the restart path works end to end.
3. Any code change has a test.
4. Changelog entry and troubleshooting docs.

## Out of scope

- A supervisor (launchd, systemd) for the proxy.
- Changing `mcp-proxy` or `icm serve` themselves.
- Session-start recovery (#2358), which stays as is.
