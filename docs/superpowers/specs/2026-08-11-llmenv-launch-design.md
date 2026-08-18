<!-- markdownlint-disable MD013 -->
# `llmenv launch <engine>` — Design

Target milestone: **v4.0.0** (Large Feature → branches from `main`). Tracked in #1056.

## Problem

Today, running an agent under llmenv means: a shell precmd/PROMPT_COMMAND hook
(installed into `.zshrc`/`.bashrc` by `llmenv setup`/`init`) re-runs the full
resolution pipeline on **every shell prompt** and `export`s the result into the
interactive shell, ambient direnv-style. The user then types `claude` (or
`crush`/`opencode`) directly, relying on env vars already being in place from the
last prompt.

This has real costs:

- Doesn't work for shells llmenv doesn't hook (fish, nu, non-interactive shells, CI).
- Requires a shell restart / re-source after install or config changes.
- Runs the full pipeline (config parse → env/scope/tag/bundle resolution → bundle
  merge → MCP resolve → materialize hash) on every prompt, whether or not an agent
  is even running.
- Nothing owns the engine's process lifetime, so nothing can guarantee cleanup,
  observe crashes, or hold warm state across the hooks the engine fires *during* a
  session (each `llmenv hook-run` invocation Claude Code spawns is its own fresh
  process, cold caches every time).

#921 proposed fixing the warm-cache half of this with a separate persistent daemon
(`llmenvd`). This design supersedes #921: instead of a system-wide daemon with its
own lifecycle independent of its callers, `llmenv launch <engine>` makes llmenv
itself the long-lived process for an agent session, by supervising the engine as a
child rather than resolving-and-exiting.

## Decided direction

- **Supervise, don't exec-replace.** `launch` forks/spawns the target engine as a
  child process and stays resident for the whole session — piping stdio, forwarding
  signals, propagating the exit code — rather than calling `execve` and disappearing.
  This is the load-bearing decision: it's what lets llmenv hold warm state and
  guarantee teardown without a separate daemon.
- **`export` stays; the ambient hook goes.** The precmd/PROMPT_COMMAND shell hook
  that auto-triggers resolution on every prompt is being replaced by `launch` as the
  primary way to run an engine under llmenv. `llmenv export` remains available as an
  explicit, non-ambient command for scripts/CI that want env vars without launching
  an engine. Removing the shell-hook *installation* from `setup`/`init` is a separate
  follow-up issue, filed once `launch` has shipped and proven out (not part of this
  design) — see "Out of scope" below.
- **v1 scope is the mechanism, not new surface area.** The `llmenv_context`/
  `llmenv_why` MCP tools that #921 planned to host over its stdio proxy are a
  separate follow-up issue once `launch` exists — they're a distinct feature (new
  agent-facing tools, their own schema questions) that happens to become cheap to
  host once `launch` is resident, not a requirement for shipping `launch` itself.

## Architecture

```text
llmenv launch claude -- --resume
        │
        ├─ 1. resolve (in-process, same pipeline `export` already uses):
        │      config parse → env/scope/tag/bundle resolution → bundle merge →
        │      MCP resolve → materialize
        │
        ├─ 2. open a per-session unix socket, bind LLMENV_LAUNCH_SOCKET in the
        │      child's env
        │
        ├─ 3. spawn `claude --resume` as a child, inherited stdio, resolved env
        │      │
        │      └─ engine fires its own lifecycle hooks as fresh subprocesses,
        │         e.g. `llmenv hook-run --engine claude_code session_start` —
        │         these inherit LLMENV_LAUNCH_SOCKET and connect to step 2's
        │         socket for warm state instead of resolving cold
        │
        ├─ 4. install SIGINT/SIGTERM/SIGHUP handlers (ignore-and-keep-waiting;
        │      the terminal-delivered signal already reaches the child directly,
        │      same process group)
        │
        ├─ 5. wait for the child to exit
        │
        └─ 6. teardown: unlink the socket, exit with the child's status (128+signum
               if it died by signal) — the materialized cache is untouched (it's
               shared/content-addressed, not session-scoped; see "Teardown" below)
```

## Components

### CLI shape

`llmenv launch <engine> [-- <args>...]`. `<engine>` accepts either a binary name
(`claude`, `crush`, `opencode`) or an adapter id (`claude_code`), resolved through
the existing `known_engine_ids()`/`engine_id()`/`binary_name()` registry in
`src/adapter/mod.rs` — no new engine-name mapping to maintain. An unrecognized engine
errors and lists the supported names. Everything after `--` passes through to the
engine binary unmodified.

### Resolution core reuse

"Shared library both daemon and fallback call" from #921 collapses to something
smaller here: `launch` lives in the same binary as `export`/`hook-run`, so it calls
`scope::evaluate`, `merge::merge`, and `materialize::*` directly, in-process. No new
crate, no daemon RPC, no version-skew handling (there's only one binary version
involved — the one that's running). The only refactor needed is confirming these
functions are callable as plain library calls rather than being entangled with
`export`'s own CLI/stdout-printing code path.

### Per-session socket

- **Path:** `$XDG_RUNTIME_DIR/llmenv/launch-<pid>.sock`, falling back to
  `<state_dir>/launch-<pid>.sock`. `<pid>` is `launch`'s own pid, so the path is
  unique per session by construction — no spawn-lock or stale-socket cleanup needed
  (unlike a global daemon's socket, this one's owner and lifetime are unambiguous).
- **Discovery:** `launch` sets `LLMENV_LAUNCH_SOCKET=<path>` in the child's
  environment. Env vars propagate to every descendant process, so any
  `hook-run`/`export`/`statusline`/`check-stale` invocation the engine spawns for its
  own lifecycle hooks picks it up automatically — no change needed to how Claude
  Code (or crush/opencode) invoke hook commands.
- **Protocol:** reuses #921's wire format — length-prefixed JSON request/response,
  a fixed verb allow-list (`resolve`, `status`, `materialize`, `recall`, `store`,
  `log`), connect-with-a-50ms-budget-then-fall-back. No version handshake: client and
  socket owner are always the same `llmenv` invocation, so version skew can't happen.
- **Degradation contract (unchanged from #921):** the socket is a pure optimization.
  Every client must produce identical output via the exact in-process path that
  exists today if the socket is missing, times out, or returns malformed data —
  covering both "engine launched without `launch`" and "socket torn down mid-race."
  `LLMENV_NO_LAUNCH_SOCKET=1` forces the fallback for debugging.

### Signal handling

`launch` installs handlers for SIGINT/SIGTERM/SIGHUP using tokio's `signal` feature
(already a dependency — enabling one more Cargo feature, no new crate). On receipt,
`launch` never exits on its own account; it keeps waiting on the child so the
reported status is always the engine's. Once the child exits: normal exit →
propagate its exit code; killed by signal → exit with `128 + signum` (POSIX
convention), so `$?` in a calling script/CI job sees the expected value.

**Amended by #1383.** This section originally specified ignoring *all three*
signals, on the reasoning that the terminal already delivers the same signal
directly to the child (same process group by default, standard `Command::spawn`
behavior). That reasoning holds only when the signal is group-delivered. It
does not hold for the non-interactive contexts this design explicitly targets —
`docker stop` signals PID 1, systemd `KillMode=mixed` signals the main pid, a CI
runner or IDE task does `kill <pid>` — where the engine never receives a copy
and ignoring the signal meant nothing shut down until the caller's SIGKILL
deadline. The shipped behavior is therefore asymmetric:

- **SIGINT is not forwarded.** The terminal generates it for the whole
  foreground process group, so the engine already has it, and an agent TUI
  commonly reads a second interrupt as "force quit" — forwarding would turn one
  Ctrl-C into two.
- **SIGTERM and SIGHUP are forwarded** to the child, after which `launch` keeps
  waiting as before. A terminal never generates SIGTERM, so one arriving here
  came from a supervisor targeting this pid. Both mean "terminate", so the
  duplicate a rare group-directed kill produces is harmless.

Forwarding goes through `rustix::process::kill_process`, not the `kill` binary:
the workspace forbids `unsafe` (ruling out `libc::kill`), and shelling out would
fail on exactly the minimal container images this fix exists to serve (#1382).

### Teardown

**Correction from an earlier draft of this design:** `materialize::*`'s output is a
fixed, content-addressed directory keyed on the manifest hash
(`cache_root.join(folder_name(mode, shape, hash))`) that every `export`/`launch`
invocation for the same config reuses and shares — it is **not** session-scoped, and
nothing else in the codebase writes a genuinely per-session secret/credential file
today. Deleting it on exit would force every other concurrent session (or the next
plain `export`) to re-materialize from scratch, and risks deleting a directory a
sibling process is actively reading from. `launch`'s teardown therefore does **not**
touch the materialized cache — that stays owned by the existing content-addressed
cache lifecycle (`doctor --gc` etc.), unchanged by this design.

On every exit path — normal exit, crashed child, or `launch` itself killed — unlink
only the per-session socket (`$XDG_RUNTIME_DIR/llmenv/launch-<pid>.sock`), the one
artifact this design actually introduces that is genuinely scoped to a single
`launch` invocation.

### Error handling

- Resolution failure (bad config, invalid scope) → print the error, exit nonzero,
  never spawn the child. Same failure behavior `export` has today — `launch` doesn't
  introduce a new error path, it reuses the existing one.
- Unrecognized `<engine>` → error listing `known_engine_ids()` and binary names.
- Engine binary not found on `PATH` → clear error before attempting to spawn,
  generalizing the existing `find_claude_binary`-style check to every adapter.

## Non-goals / out of scope

- **Removing the ambient shell-hook installation from `setup`/`init`.** Separate
  follow-up issue, filed once `launch` has shipped.
- **`llmenv_context`/`llmenv_why` MCP tools.** Separate follow-up issue — a new
  agent-facing surface, not required for the supervision mechanism itself.
- **Crash/restart supervision** (#1284), **mid-session auth/token refresh** (#1285),
  **mid-session config-drift watch** (#1286) — filed as independent follow-ups, none
  required for `launch` v1.
- **Windows support.** Unix process-group signal semantics and unix-domain sockets
  only, matching #921's own non-goals; revisit if there's demand.
- **A pty layer.** Inherited stdio (`Stdio::inherit()`) is sufficient — the engine's
  own terminal I/O passes through transparently, the same way `env`/`time`/`sudo`
  wrap interactive commands today without allocating a pty.

## Testing strategy

- **Engine-name resolution (unit):** binary name and adapter id both resolve;
  unknown engine names produce a clear error listing supported engines.
- **Env parity (integration):** `launch <engine>` resolves the same env vars, for
  the same scope, that plain `export` would — asserted byte-identical, since `launch`
  must not introduce a second resolution behavior.
- **Socket round-trip parity (integration):** a client connecting to
  `LLMENV_LAUNCH_SOCKET` gets state identical to what the in-process fallback
  produces — same parity-test spirit as #921's acceptance gate.
- **Degradation (integration):** `LLMENV_NO_LAUNCH_SOCKET=1`, a missing socket, and a
  socket torn down mid-session all fall back cleanly with no user-visible error.
- **Signal propagation (integration):** sending SIGINT/SIGTERM to `launch` results in
  the child receiving it and `launch`'s exit code matching the child's.
- **Teardown (integration):** the per-session socket file is gone after `launch`
  exits, including when the child exits nonzero or is killed by a signal; the shared
  materialized cache directory is untouched.

## Decomposition (implementation sub-issues, filed when work starts)

1. **`launch` skeleton** — CLI subcommand, engine-name resolution, spawn with
   inherited stdio, exit-code/signal propagation. No socket yet (falls back to
   today's cold-start behavior for any hooks fired during the session).
2. **Resolution-core call path** — confirm/refactor `scope::evaluate`/`merge::merge`/
   `materialize::*` are callable as plain in-process functions from `launch`, sharing
   code with `export` rather than duplicating it.
3. **Per-session socket** — bind/framing, `LLMENV_LAUNCH_SOCKET` env propagation,
   the verb set, the degradation/parity test suite (the acceptance gate).
4. **`hook-run` warm path** — `recall`/`store`/`log` verbs over the socket;
   `export`/`statusline`/`check-stale` thin-client behavior when the socket is
   present.
5. **Teardown + signal handling** — SIGINT/SIGTERM/SIGHUP handlers, guaranteed
   per-session socket cleanup on every exit path.
6. **Docs + changelog** — `website/docs/` page for `launch`, CHANGELOG entry under
   `[Unreleased]`, migration note pointing existing shell-hook users at `launch`.

## Open questions (defaults chosen; revisit during implementation)

- The 50ms socket-connect budget is carried over from #921's guess — tune against
  real measurements once `launch` exists.
- Whether `doctor` should gain a `launch` check (any orphaned per-session sockets
  left behind by a crash that skipped teardown) — likely yes, folded into
  sub-issue 5.
