# Umans usage-throttling support — design

Issue: #487 — feat: umans usage-throttling support
Milestone: Small Enhancements
Base branch: `release/2.x`

## Problem

Claude Code backed by the Umans API hits Umans' rate limits fast and lands in a
hard-locked state. Umans enforces a **moving 5-hour window** with a soft request
limit, a higher hard cap, and short burst tolerance. Crucially, exceeding the
cap triggers a **deprioritization penalty** (`priority.low` / `boxed_until`) that
*persists across window resets* — the user observed being throttled in a fresh
window because of hitting the cap in the previous one.

The goal is to spread requests out *before* the cap is reached so work keeps
flowing slowly rather than slamming into a 429, while never stalling the session
for the (potentially multi-hour) full penalty period.

## Investigation findings (live API, 2026-06-29)

`~/.umans/config.json` keys: `api_token`, `api_endpoint`
(`https://api.code.umans.ai`), `model` (`umans-coder`), `plan_slug`
(`code_pro`), `max_concurrency` (5), `per_user_max_concurrency`.

**`GET /v1/usage` carries NO rate-limit headers** (no `X-RateLimit-*`,
`Retry-After`, quota headers). State must be obtained by **explicit polling** —
this is the decisive architectural fact.

The `GET /v1/usage` JSON body is rich:

| Field | Meaning |
|---|---|
| `limits.requests.limit` | soft limit per window (200) |
| `limits.requests.hard_cap` | hard 429 ceiling (400) |
| `limits.requests.window_seconds` | window length (18000 = 5h) |
| `window.started_at` / `window.resets_at` | window bounds (ISO 8601) |
| `window.remaining_minutes` | minutes left in window |
| `usage.requests_in_window` | requests used so far |
| `usage.remaining_requests` | soft headroom |
| `usage.concurrent_sessions` | active concurrent sessions |
| `usage.priority.low` | **deprioritized right now (bool)** |
| `usage.priority.boxed_until` | **throttled until this ISO time** |
| `usage.priority.reason` | why (e.g. `rate_limited`) |

`GET /v1/usage/history` requires a `from` query param (ISO timestamp). Not used
by the throttle — the live `priority` block already signals throttle state.

## Approach

A **PreToolUse + UserPromptSubmit hook pair** that polls `/v1/usage` (shared,
TTL-cached) and introduces a **capped, box-aware adaptive delay** before letting
the session proceed. Ships as a new `bundles/umans/` bundle, consistent with how
`rtk`, `slop-scan`, and `session-log` ship in this repo.

Both gates share one poll cache so they never double-poll within the TTL:
- **PreToolUse** — gates per tool call (proxy for "Claude is actively working").
- **UserPromptSubmit** — gates per turn, and prints a one-line budget note as
  context so the user/agent sees remaining headroom.

The hook **never hard-blocks**: any error (missing config, network failure,
parse error, throttling disabled) logs to stderr and exits 0 with zero delay.
Throttling must never break a session.

## Components (`bundles/umans/`)

1. **`hooks/umans_usage.py`** — shared helper. Reads `~/.umans/config.json`,
   polls `GET /v1/usage`, caches the JSON to
   `${XDG_STATE_HOME:-~/.local/state}/llmenv/umans-throttle/usage.json` with a
   TTL. First caller past the TTL refreshes; others reuse the file (mtime check,
   no locking — worst case a couple extra polls, harmless). Exposes
   `get_usage(config) -> dict | None` and `compute_delay(usage, config) -> float`.

2. **`hooks/umans-throttle.sh`** — single hook entrypoint wired to both events.
   Reads the hook event from stdin, loads usage via the helper, computes the
   delay, sleeps (capped), and for `UserPromptSubmit` prints a budget line. Exits
   0 always.

3. **`bundle.yaml`** — wires two `hooks:` entries (PreToolUse, UserPromptSubmit)
   both invoking `umans-throttle.sh`, declares the `env:` defaults, and allows
   the `~/.umans/config.json` read + the script invocation in `permissions:`.

4. **config.yaml registration** — `bundle: - name: umans, when: [backend-umans]`,
   and the `backend-umans` tag added to hosts that use the Umans backend (the
   personal-laptop host, per its `umans-coder` model).

## Throttle logic — `compute_delay(usage, config)`

Pure function, unit-tested. Returns seconds to sleep (0 = proceed immediately).

```
max_wait   = LLMENV_UMANS_THROTTLE_MAX_WAIT        # default 300
threshold  = LLMENV_UMANS_THROTTLE_SOFT_THRESHOLD  # default 20
remaining  = usage.usage.remaining_requests
boxed      = usage.usage.priority.low is true OR priority.boxed_until in future

if boxed:
    # Server is already deprioritizing us. Spread requests out, but NEVER wait
    # until boxed_until (it can be hours away / span windows). Cap hard.
    return max_wait
elif remaining <= 0:
    # At/over soft limit, not yet boxed. Cap the wait.
    return max_wait
elif remaining < threshold:
    # Scale linearly: closer to 0 remaining => closer to max_wait.
    return max_wait * (threshold - remaining) / threshold
else:
    return 0
```

The cap (`max_wait`, default 5 min) is a hard requirement: the wait is always
bounded regardless of how far in the future `boxed_until` is.

## Configuration (env vars — `LLMENV_` prefix, llmenv-internal)

| Var | Default | Purpose |
|---|---|---|
| `LLMENV_UMANS_THROTTLE_CACHE_TTL` | `30` | shared `/v1/usage` poll cache, seconds |
| `LLMENV_UMANS_THROTTLE_MAX_WAIT` | `300` | hard cap on any single delay, seconds |
| `LLMENV_UMANS_THROTTLE_SOFT_THRESHOLD` | `20` | `remaining_requests` level where delays begin |
| `LLMENV_UMANS_THROTTLE_DISABLE` | unset | any non-empty value disables throttling (exit 0, no delay) |

Defaults are baked into the helper so the bundle works with no env config; the
`env:` block in `bundle.yaml` documents and can override them.

## Error handling

Every failure path exits 0 with zero delay and a one-line stderr diagnostic
(`umans-throttle: <reason>`). Covered cases: config file missing/unparseable,
network/HTTP error polling `/v1/usage`, unexpected JSON shape, throttle disabled.
A throttle that breaks the session is worse than no throttle.

## Testing

`compute_delay` is pure and gets a `pytest` (colocated `hooks/test_umans_usage.py`)
covering:
- boxed (`priority.low: true`) → returns `max_wait` (capped)
- `boxed_until` far in the future → still returns `max_wait`, never the gap to
  `boxed_until`
- `remaining_requests <= 0`, not boxed → `max_wait`
- `remaining_requests` just under threshold → scaled, `< max_wait`
- healthy headroom → `0`
- malformed usage dict → no crash (helper returns None → hook no-ops)

Cache TTL behavior verified with a small filesystem test (write usage file,
assert reuse within TTL, refresh past TTL) if cheap; otherwise covered by the
mtime check being a one-liner.

## Out of scope (YAGNI)

- `/v1/usage/history` trend analysis — the live `priority` block is sufficient
  to decide throttling. Add only if simple polling proves inadequate.
- Concurrency throttling (`concurrent_sessions` vs `limit`) — the request-window
  limit is what the user actually hits. Revisit if concurrency 429s appear.
- Cross-process locking on the cache file — mtime check is enough.
