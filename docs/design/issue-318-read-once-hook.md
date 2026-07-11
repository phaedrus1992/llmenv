# Issue #318 — read-once file deduplication hook for context efficiency

- **Issue:** https://github.com/phaedrus1992/llmenv/issues/318
- **Milestone:** Small Projects
- **Type:** Feature (built-in, meta-feature — session/project agnostic)
- **Difficulty:** Moderate. The hook plumbing already exists; the work is
  the cache, the modes, and the stats.

## Problem

Claude Code re-reads files already in context; each redundant full-file
read costs ~2,000+ tokens. Upstream `read-once` (Boucle framework) reports
~40% read-token savings. llmenv should ship a native equivalent: a
`PreToolUse` hook that tracks reads by `{path, mtime}` and warns on (or
denies) redundant re-reads.

## Where this plugs in (verified in code)

llmenv already registers a baseline hook dispatcher **unconditionally** for
claude-code: every hook event runs
`llmenv hook-run --engine claude_code <event>` (see `HOOK_RUN_COMMAND` and
the event mapping table at `src/adapter/claude_code.rs:45–90`, which
already includes `("pre_tool_use", "PreToolUse")`). Dispatch lands in
`hook_run::run()` / the `HookEvent` enum in `src/hook_run/mod.rs`. The ICM
memory feature is the reference pattern for a feature-gated built-in.

Config-side, built-ins live on the `Features` struct in
`crates/llmenv-config/src/schema.rs` — `context_mode: Option<ContextMode>`
(a token-efficiency feature with an `enabled` flag) is the closest sibling;
copy its shape. **New code goes in core (`src/`), never `examples/`**
(per `AGENTS.md`).

## Design

### Config (`crates/llmenv-config/src/schema.rs`)

```yaml
features:
  read_once:
    enabled: true        # default false (opt-in at first ship)
    mode: warn           # warn | deny (default warn)
    ttl_seconds: 1200    # session-cache TTL fallback, default 20 min
    diff: false          # changed-file delta mode (phase 2, default false)
```

Add `ReadOnce` struct + `Features.read_once: Option<ReadOnce>`, serde
defaults matching the above, validation (mode enum, ttl > 0), YAML
round-trip test — mirror how `ContextMode` does each of these.

### Cache

- One JSON file per Claude Code session under llmenv's state dir (follow
  wherever `icm.json` lives — see `src/icm.rs` state-file handling):
  `read_once/<session_id>.json`. Session id comes from the hook input JSON
  Claude Code passes on stdin (`session_id` field — check how existing
  `hook_run` handlers parse stdin input and reuse that).
- Entries: `{ path, mtime_unix, first_read_at, hits }`.
- Prune entries older than `ttl_seconds` on every load (TTL doubles as the
  compaction-safety fallback). Also delete session files older than 7 days
  on load — ponytail: opportunistic cleanup, no background job.

### PreToolUse behavior (`src/hook_run/`)

New handler wired into the `pre_tool_use` dispatch path, gated on
`features.read_once.enabled`. On a `Read` tool call:

1. Parse `tool_input` from stdin. If `offset` or `limit` present →
   **always pass through, never cache** (partial read).
2. `stat` the path. Missing file → pass through.
3. Cache miss, or cached `mtime` differs from current → record/update
   entry, pass through (changed files always pass regardless of mode).
4. Cache hit (same path + mtime, within TTL):
   - **warn mode:** allow the read but emit an advisory via the hook's
     additional-context output ("`<path>` already read this session
     (~N tokens); prefer the copy in context"). Estimate N as
     `file_bytes / 4`.
   - **deny mode:** block the call using Claude Code's PreToolUse deny
     response (`permissionDecision: "deny"` JSON on stdout — match the
     response envelope the existing write-guard PreToolUse hook at
     `src/adapter/claude_code.rs:29` emits) with the advisory as the
     reason. First read of a path in a session always passed through in
     step 3, so deny can never starve the agent of a file it lacks.
5. Increment `hits` and a running `tokens_saved` figure on every hit.

Fail-soft everywhere: any cache/IO error → pass the read through silently
(a broken optimizer must never block real work — match the existing
`hook_run` fail-soft policy exercised by `tests/hook_run_failsoft.rs`).

### PostCompact

Check whether the event table in `src/adapter/claude_code.rs:56` already
maps a compaction event. If Claude Code exposes `PostCompact`/
`SessionCompact` (verify against current hook docs, don't assume): add it
to the table and clear the session cache on it. If not available: TTL is
the fallback — file a follow-up issue for the event and note it in the
docs.

### Diff mode (phase 2 of this issue — implement last)

When `diff: true` and the file *changed* (step 3 with an existing entry):
deny the read and return a unified diff (old content hash isn't enough —
store a content copy or hash+git; **simplest correct option:** shell
`git diff --no-index` between a cached snapshot under
`read_once/snapshots/` and the current file). Only snapshot files below a
size cap (e.g. 256 KiB) — larger files always pass through. If this turns
out to balloon, ship warn/deny first and split diff mode into a follow-up
issue rather than stalling.

### Stats + manual reset (CLI)

- `llmenv status` gains a read-once section (total reads seen, cache hits,
  est. tokens saved, top 5 re-read paths) — aggregate across session files
  in the state dir. See `src/cli/status.rs`.
- `llmenv read-once clear` — delete the cache dir. Wire as a small
  subcommand next to existing ones in `src/cli/`.

## Tests

1. Schema: round-trip + defaults + validation (mirror `ContextMode` tests).
2. Handler unit tests in `src/hook_run/`: warn-hit advisory, deny-hit
   envelope, partial-read bypass, changed-mtime bypass, TTL expiry,
   missing-file pass-through, corrupt cache file → fail-soft pass-through.
3. Integration: extend the hook_run test pattern
   (`tests/hook_run_failsoft.rs`) — invoke the binary with a synthetic
   PreToolUse stdin payload twice for the same path; assert second call
   emits the advisory (warn) / deny envelope (deny).
4. Stats: seed a cache file, assert `llmenv status` output section.

## Acceptance criteria

Match the issue's checklist: warn mode advisory, deny mode blocking (first
read always passes), partial-read bypass, changed-file bypass, TTL
fallback, `read-once clear`, stats surfaced, all knobs configurable, and
user-facing docs for the feature (place alongside the existing feature
docs — find where `features.memory`/context-mode are documented). Diff
mode per the phase-2 section or an explicitly filed follow-up. CHANGELOG
`[Unreleased]` entry via the keepachangelog skill.

## Out of scope

- Other engines (opencode/crush hook surfaces) — claude-code only;
  follow-up issue if wanted.
- Cross-session persistent advisory state (upstream has it; YAGNI here —
  the session cache + TTL covers the reported win).
