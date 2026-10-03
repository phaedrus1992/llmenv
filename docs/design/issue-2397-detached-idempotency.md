# Issue #2397 — `request_id` idempotency for detached recorders

- **Issue:** https://github.com/phaedrus1992/llmenv/issues/2397
- **Milestone:** `v3.12.0`
- **Base branch:** `release/3.x` (forward-merges to `release/4.x`)
- **Type:** feature (correctness guard for the detached stores)
- **Pairs with:** #2396 (checkpoint and resume), which needs this to resume safely
- **Report:** `docs/reference/pi-durable-evaluation.md` §4 (deterministic levers row), §9 item 5

This is a spec, not a plan.

## Problem

The detached recorders (web-fetch ICM store, session-log transcript record, consolidation rule store) carry no idempotency key.
A stale-session retry inside the HTTP client, a resumed checkpoint, or two hooks firing for one event can store the same thing twice.
ICM has no way to tell the copies apart.

## Verified facts (release/3.x)

| Fact | Location |
| --- | --- |
| `McpHttpClient::call_tool` runs `try_call_tool`; on HTTP 400 or 404 (`is_stale_session_status`) it re-initializes and retries once. A landed write whose response was lost is sent again | `src/hook_run/mcp_client.rs` |
| Web-fetch store args: `content` (query, tool, epoch seconds, 1000-char preview), `topic = "web-fetch"`, `importance = "low"`; no id | `web_fetch_store_args`, `src/hook_run/mod.rs` |
| Transcript record args: `session_id` (the ICM transcript id), `role`, `content`, `metadata` (the event fields as a string), `tool_name`, `tokens`; `SessionLogEvent` has `ts`, `kind`, `scope`, `role`, `tool_name`, `tokens`, `level`, `content`, `fields`, `trace_fields`, and no id | `record_args` in `src/session_log/transcript.rs`, `src/session_log/event.rs` |
| Consolidation rule store: `store_rule` sends `content`, `topic = "llmenv-consolidation-<project>"`, `type = "semantic"`, `importance = "high"` | `src/consolidation/mod.rs` |
| Content dedup exists for consolidation only: `dedup::is_stored` recalls 5 records and compares with Jaccard at 0.8, negation-aware | `src/consolidation/dedup.rs` |
| The SessionEnd scope store is synchronous and deduped by `chunk_unchanged` against `HOOK_STORE_CHUNK` | `src/hook_run/mod.rs` |
| Hook payloads carry `session_id` (Claude's), and `tool_use_id` on tool events; no sequence number | `src/hook_run/adaptive.rs`, `src/hook_run/session_ledger.rs` |
| `session_ledger` keeps a `sent: BTreeSet` of 16-hex-char SHA-256 prefixes (`HASH_HEX_CHARS`) per session, with the lock-and-write pattern | `src/hook_run/session_ledger.rs` |
| `sha2` and `hex` are dependencies; `uuid` and `ulid` are not | `Cargo.toml` |
| `transcript-sessions.json` caps at 1000 entries with `pop_first` | `src/session_log/state.rs` |
| ICM tools `icm_memory_store` and `icm_transcript_record` accept no request id today | rtk-ai/icm |

## Decisions

1. **The key is derived, not generated.**
   `request_id` is the first 16 hex characters of SHA-256 over a fixed tuple, so the same event always maps to the same id, with no counter to persist.
   Tuples:
   - web-fetch store: (Claude `session_id`, `"icm-store"`, `tool_use_id`, `content`).
   - transcript record: (Claude `session_id`, `"session-log-record"`, event `ts`, event `kind`, `content`).
   - consolidation rule: (project, `"consolidation"`, rule text).
   When `tool_use_id` is missing, fall back to the epoch seconds already in the content.
2. **The parent computes the id and puts it in the payload.**
   The child never derives it, so a resumed checkpoint (#2396) sends the same id.
3. **The child checks a seen-set before the call and records after success.**
   `state_dir/idempotency/{session_id}.json`, a `BTreeSet<String>` of ids, with the `LedgerStore` lock pattern.
   Consolidation uses `{project}` as the file key, since it has no engine session.
   Cap each set at 1000 ids (`pop_first`), prune files at 7 days.
   Recording only after success means a failed call can be retried; recording before would lose the record on a crash between write and call.
4. **The id also goes to ICM.**
   Transcript record: inside `metadata`, as a `request_id` field alongside the event fields.
   Memory store: as a tag `request:<id>` so a future recall or an upstream dedup can see it.
   This costs nothing now and makes server-side dedup possible once rtk-ai/icm accepts an id.
   File an upstream issue on rtk-ai/icm asking for a `request_id` parameter on `icm_memory_store` and `icm_transcript_record`, and link it from the code comment.
5. **The stale-session retry is still allowed.**
   The HTTP client is not changed; the seen-set makes the retry harmless for these three callers.
   A duplicate that lands because the first response was lost is a known residual until ICM dedups on its side; the doc says so.
6. **The synchronous SessionEnd store is not in scope.**
   It already has content dedup, and it is not retried from a checkpoint.

## Design

### Module `src/hook_run/idempotency.rs`

- `pub(crate) fn request_id(parts: &[&str]) -> String` (SHA-256, 16 hex chars; reuse the ledger's hashing helper if it is already a function, do not write a second one).
- `pub(crate) struct SeenSet { path, ids: BTreeSet<String> }` with `load(state_dir, key)`, `contains(&id)`, `record(&id)` (inserts, caps, writes atomically under the lock).
- `MAX_IDS = 1000`, `STALE_DAYS = 7`.

### Parent changes

- `web_fetch_store_args` adds `request_id`.
- `RecordPayload` gains `request_id`.
- `store_new_rules` computes the id per rule and passes it to `store_rule`.

### Child changes

- `run_icm_store_inner`: load the seen-set for the session; if the id is present, log at debug and exit 0; otherwise call, then record.
- `run_record_inner`: same, keyed by the Claude session id (carry it in `RecordPayload`; it is already on stdin, not in `ps`).
- `store_rule`: same, keyed by project; `is_stored` content dedup stays as the second line of defense.

### Wire changes

- `record_args` adds `request_id` to the metadata fields.
- `store_rule` and the web-fetch store add `tags: ["request:<id>"]` (check that `icm_memory_store` accepts `tags`; if not, put it in `metadata` the same way).

### Docs

- `website/docs/mcp.md` memory section: one sentence on request ids and the state dir file, tagged `(added in v3.12.0)`.
- Changelog `Fixed`: detached memory stores and transcript records are no longer duplicated by a retry or a resumed job.

## Tests

1. `request_id` is stable for equal parts, differs when any part differs, and is 16 lowercase hex characters.
2. `SeenSet`: load on a missing file is empty; record then load contains the id; 1001 records keep the last 1000; a corrupt file loads as empty and is overwritten on the next record.
3. Store child with a stub server (`wiremock`): the first run calls the tool once and records; a second run with the same payload makes no call; a run whose call fails records nothing and a third run calls again.
4. Record child: same three cases.
5. Consolidation: two rules with the same text in one batch store once; a resumed run with the same batch stores nothing new.
6. Wire shape: `record_args` metadata contains `request_id`; the store args carry the tag.
7. Property test: for any two distinct part lists, ids differ with overwhelming probability (test on a few thousand random lists for no collision), and `SeenSet` size never exceeds `MAX_IDS`.

## Acceptance criteria

1. Running `llmenv icm-store` twice with the same stdin payload produces one ICM record.
2. A `WebFetch` during a proxy outage, followed by a resume (#2396), produces exactly one record.
3. `state_dir/idempotency/<session>.json` exists after a session with web fetches.
4. The upstream rtk-ai/icm issue for a wire-level `request_id` is filed and linked from the code.
5. Changelog entry and docs with the version tag.

## Out of scope

- Changing ICM.
- Deduping the synchronous SessionEnd store or per-prompt recall.
- Replacing the HTTP client (#2160, v3.13.0).
