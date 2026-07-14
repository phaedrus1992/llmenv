# Usage-throttling support — design

Issue: #487 — feat: umans usage-throttling support
Milestone: Small Enhancements
Base branch: `release/2.x`

## Problem

Claude Code backed by the Umans API hits Umans' rate limits fast and lands in a
hard-locked state. Umans enforces a **moving 5-hour window** with a soft request
limit, a higher hard cap, and short burst tolerance. Exceeding the cap triggers
a **deprioritization penalty** (`priority.low` / `boxed_until`) that *persists
across window resets* — the user observed being throttled in a fresh window
because of hitting the cap in the previous one.

The goal: spread requests out *before* the cap is hit so work keeps flowing
slowly rather than slamming into a 429, while never stalling the session for the
(potentially multi-hour) full penalty period.

## Scope decision: generic built-in feature, not a Umans-specific bundle

This is a **built-in llmenv core feature** (Rust, ships with the binary), not
example bundle content. Per project rules, new features live in core.

The config is **backend-agnostic**, modeled on `features.memory`: a tag-scoped
list where each entry names a `backend`. Throttling is useful across multiple
clients/backends; Umans is the first backend implementation. Backend-specific
options (custom endpoints, auth file locations) belong in the `native:`
passthrough, not the generic schema.

## Investigation findings (live Umans API, 2026-06-29)

`~/.umans/config.json` keys: `api_token`, `api_endpoint`
(`https://api.code.umans.ai`), `model`, `plan_slug`, `max_concurrency`.

**`GET /v1/usage` carries NO rate-limit headers** — state must be obtained by
**explicit polling**. The JSON body is rich:

| Field | Meaning |
| --- | --- |
| `limits.requests.limit` | soft limit per window (200) |
| `limits.requests.hard_cap` | hard 429 ceiling (400) |
| `limits.requests.window_seconds` | window length (18000 = 5h) |
| `window.resets_at` | when window resets (ISO 8601) |
| `usage.requests_in_window` | requests used |
| `usage.remaining_requests` | soft headroom |
| `usage.priority.low` | deprioritized right now (bool) |
| `usage.priority.boxed_until` | throttled until this ISO time |
| `usage.priority.reason` | why (e.g. `rate_limited`) |

`GET /v1/usage/history` needs a `from` query param — not used; the live
`priority` block already signals throttle state.

## Architecture

Follows the built-in-feature pattern (ICM as reference):

1. **Config** (`crates/llmenv-config/src/schema.rs`): add `throttle: Vec<Throttle>`
   to the `Features` struct. `Throttle` is generic:

   | Field | Type | Default | Purpose |
   | --- | --- | --- | --- |
   | `backend` | String | — (required) | selects backend implementation (`"umans"`) |
   | `when` | `Vec<String>` | `[]` | tag-scoped activation (like `memory`) |
   | `cache_ttl` | u64 | 30 | shared usage-poll cache, seconds |
   | `max_wait` | u64 | 300 | hard cap on any single delay, seconds |
   | `soft_threshold` | u64 | 20 | `remaining` level where delays begin |

   Resolver selects the active entry by tag intersection (same model as memory);
   more than one active entry is an error (single throttle per scope).

2. **Hook injection** (`src/adapter/claude_code.rs`, `generate_settings_json()`):
   when a throttle entry is active, inject two hooks that call back into the
   binary, mirroring the existing `config-guard` injection:
   - `PreToolUse` → `llmenv throttle pre-tool`
   - `UserPromptSubmit` → `llmenv throttle prompt`

3. **Runtime logic** (new module `src/throttle/`, CLI subcommand in
   `src/cli/mod.rs`): `llmenv throttle <event>` reads the hook JSON from stdin,
   resolves the active throttle config, asks the backend for a `UsageSnapshot`
   (TTL-cached), computes a capped delay, sleeps, and on `prompt` prints a
   one-line budget note as `additionalContext`. Always exits 0.

4. **Backend abstraction**: a `ThrottleBackend` trait returning a normalized
   snapshot. One impl (`umans`) selected by a `match` on `backend` — no plugin
   registry (YAGNI).

   ```rust
   struct UsageSnapshot {
       remaining: Option<u64>,   // remaining requests in window
       limit: Option<u64>,       // soft limit (for the budget line)
       resets_at: Option<String>,// ISO window reset (for the budget line)
       penalized: bool,          // priority.low OR boxed_until in future
   }
   trait ThrottleBackend {
       fn fetch_usage(&self) -> anyhow::Result<UsageSnapshot>;
   }
   ```

   `UmansBackend` reads `~/.umans/config.json`, polls `GET /v1/usage`, maps the
   body to `UsageSnapshot` (`penalized = priority.low || boxed_until > now`).

## Throttle logic — `compute_delay(snapshot, cfg) -> Duration`

Pure, backend-agnostic, unit-tested. Always capped at `max_wait`; we never wait
until `boxed_until` (it can span hours / future windows, #487).

```text
if snapshot.penalized:            return max_wait        # server deprioritizing us
if remaining is None:             return 0               # unknown -> don't block
if remaining == 0:                return max_wait
if remaining < soft_threshold:    return max_wait * (soft_threshold - remaining) / soft_threshold
else:                             return 0
```

## Caching

Shared TTL cache so the two hooks never double-poll within `cache_ttl`. JSON
snapshot written to `${state_dir}/throttle/<backend>-usage.json`; mtime-based
TTL check, no locking (a race just causes a harmless extra poll).

## Error handling

Every failure (missing/invalid backend config, network error, parse error,
no active throttle) → exit 0, zero delay, one-line stderr diagnostic. A throttle
that breaks the session is worse than no throttle.

## Testing

- `compute_delay` (pure): penalized→capped; `boxed_until` far future→still
  capped; remaining 0→max; just under threshold→scaled `< max`; healthy→0;
  grows as remaining shrinks; unknown remaining→0.
- Umans body→`UsageSnapshot` mapping: a recorded sample body maps to expected
  fields; `penalized` true when `priority.low`; `penalized` true when
  `boxed_until` in future; false when in past.
- Config: `Throttle` deserializes with defaults; `backend` required; multiple
  active entries for one scope is a validation/resolve error.
- Cache TTL: write snapshot, reuse within TTL, refresh past TTL.

## Out of scope (YAGNI)

- `/v1/usage/history` trend analysis — live `priority` block is sufficient.
- Concurrency throttling — the request-window limit is what users hit.
- Additional backends beyond `umans` — the trait leaves room; we ship one impl.
- A plugin registry for backends — a `match` on the `backend` string is enough.
