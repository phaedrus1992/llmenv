<!-- markdownlint-disable MD013 -->
# `launch` mid-session supervision — Design

Target milestone: **v4.0.0**. Tracked in composite #1480 (sub-issues #1284, #1285,
#1286). Branches from `release/4.x`. Follow-on to `launch` v1 (#1056,
`docs/superpowers/specs/2026-08-11-llmenv-launch-design.md`), which explicitly
listed these three as non-goals (design doc lines 190-192).

## Problem

`launch` (added in v4.0.0) supervises the target engine as a resident child
process with inherited stdio. Three deferred follow-ups need `launch` to act on
things that happen *during* that session:

- **#1284** — relaunch the engine after a crash, without redoing full resolution.
- **#1285** — notice when a managed credential is close to expiry.
- **#1286** — notice when the user edits `config.yaml`/bundles mid-session.

All three share one open question the v1 design didn't answer: `launch` doesn't
own stdio (the child does, no pty), so it has no way to print into the running
session or push new state into the child on its own.

## Decided direction

**Two shared primitives**, covering all three issues without inventing a new
channel per issue:

### 1. Graceful relaunch

`launch` already observes the child's exit status (v1 design, "Signal
handling"). This adds a `relaunch()` path triggered by either a crash (nonzero
exit or signal) or an internal decision (credential expiry — see below). In
both cases the child has already exited, so `launch` owns stdio again for the
brief window before it spawns the replacement: it prints the reason and, unless
`--auto-restart` is set, reads a `y/N` prompt directly from the inherited
terminal. No new IPC is needed for this path.

Relaunch reuses the already-resolved env and materialized config from the
original launch — it does not re-run `scope::evaluate`/`merge::merge`/
`materialize::*`. A restart-attempt counter, reset on a rolling 5-minute
window, caps repeats at a default of 3 to prevent a crash loop; once the cap is
hit, `launch` prints the final error and exits nonzero instead of retrying.

### 2. Mid-session notice channel

For the case where the child is still alive and `launch` needs to tell the
agent something, this reuses the existing per-session socket
(`LLMENV_LAUNCH_SOCKET`, v1 design "Per-session socket") rather than inventing
a push mechanism:

- `launch` sets an internal pending-notice flag (drift detected, credential
  nearing expiry) when a background check fires.
- The engine's own `hook-run` invocations already connect to this socket on
  effectively every turn (tool-use hook events fire for every supported
  engine). A new verb, `pending_events`, is added to the existing verb
  allow-list (`resolve`, `status`, `materialize`, `recall`, `store`, `log`).
  `hook-run` calls it on every connection it already makes; if a notice is
  pending, the flag clears (exactly-once delivery) and `hook-run` returns it.
- `hook-run` renders the notice via the adapter's existing
  `emit_hook_context()` (`src/adapter/mod.rs`, already implemented for Claude
  Code, Crush, OpenCode, and Codex), which places it in
  `hookSpecificOutput.additionalContext` for the current hook event.

This is best-effort, next-turn delivery, not instant — acceptable for a
warning, not appropriate for anything time-critical. It follows the same
degradation contract as the rest of the socket: if the socket is unavailable
when `hook-run` checks, it skips silently and tries again on the next
connection, exactly like the existing verbs already do when the socket is
missing or times out.

## Per-issue application

### #1284 — crash/restart supervision

Uses graceful relaunch directly:

1. Child exits nonzero or by signal.
2. `launch` prints the exit reason (code or signal name).
3. Check the restart-attempt cap. If exceeded, print a final error and exit
   nonzero.
4. Otherwise: `--auto-restart` set → relaunch immediately. Not set → prompt
   `Restart? [y/N]` on the inherited terminal; `n` or EOF exits with the
   child's original status.
5. Relaunch reuses the resolved env/materialized config from the original
   launch.

No new channel needed — the child being dead is what frees stdio for the
prompt.

### #1286 — config-drift watch

Uses the notice channel:

1. `launch` already tracks config mtime for its warm-resolution cache
   (v1 design). A background task compares this on an interval, reusing that
   comparison — no new watcher mechanism.
2. On a change, set the pending-notice flag with the message: "llmenv config
   changed since this session started; restart to pick up changes."
3. Delivered to the agent's context on the next `hook-run` connection via
   `emit_hook_context()`.
4. No auto-apply. The change is surfaced only; `launch` never re-materializes
   or restarts on its own account for a drift event.

### #1285 — auth/token refresh (scope narrowed)

**Finding during design:** the issue body assumes llmenv can silently refresh
a credential. Today llmenv only caches Claude Code's OAuth blob
(`.claude.json`) after Claude Code itself performs the refresh
(`src/auth/detect.rs`, `src/auth/credentials.rs`) — llmenv has no OAuth
refresh call of its own, for this or any other flow.

**v1 scope for this issue is detect-and-notify, not silent refresh:**

1. A background task in `launch` checks the cached credential's `expiresAt`
   against a threshold window.
2. When a credential is close to expiry (or already expired with no live
   refresh token, per `src/auth/credentials.rs`'s existing
   stale/expired distinction), set the pending-notice flag: "credentials
   expire soon; run `llmenv login` if the engine reports an auth failure."
3. Delivered via the same channel as #1286.
4. Implementing an actual non-interactive refresh call is separate,
   per-provider work (an OAuth token-endpoint call, requires confirming which
   flows even support silent refresh) and is out of scope here — file as a
   follow-up issue once a flow is confirmed to support it.

Failure mode (refresh not possible / credential already expired): the same
notice fires. `launch` never force-restarts on this account — an engine using
a static, literal env-var credential (Crush, OpenCode) would only fail again
immediately with a fresh restart, so the notice is the only action taken.

## Non-goals / out of scope

- **Silent (non-interactive) credential refresh.** Detection and notice only,
  per the #1285 scope finding above. A follow-up issue covers the actual
  refresh call once a flow is confirmed to support it.
- **A pty layer or owning stdio directly.** The mid-session notice channel
  exists specifically to avoid this; `launch` still never reads or writes the
  child's stdio streams itself.
- **Auto-applying config drift.** Surfaced only, never silently
  re-materialized (matches #1286's own "Out of scope").
- **Windows support.** Matches the base `launch` design's own non-goal.

## Testing strategy

- **Unit — restart-attempt cap:** counter resets correctly across the rolling
  window; caps at the default of 3 within the window.
- **Unit — drift detection:** reuses the existing mtime-comparison function;
  a config edit inside the window is detected, no edit produces no notice.
- **Unit — expiry-proximity:** a credential inside the threshold window flags,
  one outside does not; a credential with no readable expiry is treated as
  unknown and produces no notice (not an error).
- **Integration — relaunch parity:** a relaunch resolves the same env as the
  original launch, asserted byte-identical (same parity-test pattern as the
  base `launch` design).
- **Integration — `pending_events` round-trip:** a flag set by `launch` is
  returned to the next `hook-run` connection exactly once; a second connection
  with nothing new pending gets nothing.
- **Integration — crash-loop protection:** simulate repeated crashes past the
  cap; confirm `launch` stops auto-restarting and exits nonzero with a final
  error instead of looping.
- **Degradation:** `pending_events` behaves like the existing verbs when the
  socket is missing, times out, or returns malformed data — `hook-run` skips
  silently and retries on the next connection, no user-visible error.

## Decomposition (implementation sub-issues)

1. **Graceful relaunch core** (#1284) — exit-reason detection, restart-attempt
   cap, `--auto-restart` flag, prompt path, relaunch reusing resolved
   env/materialized config.
2. **`pending_events` socket verb** — add to the existing verb allow-list;
   `hook-run` client-side check on every connection; exactly-once delivery
   semantics.
3. **Config-drift detection** (#1286) — background mtime-comparison task;
   wires into the notice channel from (2).
4. **Credential-expiry detection and notice** (#1285, narrowed scope) —
   background expiry check against the existing credential cache; wires into
   the notice channel from (2). Explicitly excludes implementing a refresh
   call.
5. **Docs + changelog** — `website/docs/` updates for `launch`'s new restart
   flag and behavior, CHANGELOG entries under `[Unreleased]` for all three.

## Open questions (defaults chosen; revisit during implementation)

- The credential-expiry threshold window (how far ahead of `expiresAt` to
  notify) has no measured default yet — start conservative (e.g. 5 minutes)
  and tune during implementation.
- Whether the drift-check interval should be fixed or itself configurable —
  default to a fixed interval matching the existing warm-cache mtime check's
  granularity, revisit if it proves too chatty or too slow in practice.
