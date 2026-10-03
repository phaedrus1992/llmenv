# Issue #2396 — checkpoint and resume detached background work

- **Issue:** https://github.com/phaedrus1992/llmenv/issues/2396
- **Milestone:** `v3.12.0`
- **Base branch:** `release/3.x` (forward-merges to `release/4.x`)
- **Type:** bug fix (lost work) with new state files and a doctor section
- **Pairs with:** #2397 (idempotency); a resumed job can double-store without it. Implement #2397 first or in the same branch.
- **Report:** `docs/reference/pi-durable-evaluation.md` §4 (durability row), §9 item 4

This is a spec, not a plan.

## Problem

Every background job llmenv starts is fire-and-forget.
If the child dies (machine sleeps, the MCP endpoint is down, a call times out), the work is gone.
Nothing retries and nothing reports it.
The only trace is a line in `detached-hook.log` or `index.log` that nobody reads.

## Verified facts (release/3.x)

### The four detached jobs

| Job | Spawner | Child command | Inputs | Timeouts |
| --- | --- | --- | --- | --- |
| Post-session consolidation | `maybe_start_consolidation` → `post_session_consolidation` in `src/hook_run/mod.rs`, on `SessionEnd` and `PostSession`, guarded by `is_consolidation_child` (`LLMENV_CONSOLIDATION_CHILD`) | `llmenv consolidation-run` (`consolidation_run_command`), stdin null | config path from `paths::config_path()`, inherited cwd (the child derives the project with `memory::project::session_project`) | 30 s per MCP call (`CONSOLIDATION_TIMEOUT`), 120 s LLM (`LLM_TIMEOUT`) in `src/hook_run/detached_consolidation.rs` and `src/consolidation/mod.rs` |
| Web-fetch ICM store | `handle_web_fetch_post_tool_use` in `src/hook_run/mod.rs`, on `PostToolUse` | `llmenv icm-store`, JSON on stdin from `web_fetch_store_args` (`content`, `topic`, `importance`) | the stdin JSON | 5 s (`STORE_TIMEOUT`) in `src/hook_run/detached_store.rs` |
| Session-log transcript record | `session_log::detached::spawn_record(session_id, ev)` from `emit_session_log` | `llmenv session-log-record`, `RecordPayload { session_id, event }` on stdin | the stdin JSON | 5 s (`RECORD_TIMEOUT`) in `src/session_log/detached.rs` |
| codebase-memory auto-index | `trigger_codebase_memory_index(project_root, cm, state_dir)` in `src/hook_run/mod.rs`, on `SessionStart` | `codebase-memory-mcp cli index_repository '{"repo_path": …}'` from `build_index_repository_command`; stdout null, stderr `<cache_dir>/index.log` | project root, `CBM_CACHE_DIR` when `index_path` is set | none; exit status never observed |

Common traits:

- Hidden subcommands `SessionLogRecord`, `IcmStore`, `ConsolidationRun` are declared in `src/cli/mod.rs` and re-exec `std::env::current_exe()`.
- Every child gets `detach_process_group` (`src/mcp/proxy.rs`, `process_group(0)` on Unix).
- llmenv children log through `redirect_stderr_to_detached_log` to `<state_dir>/detached-hook.log`, a bounded log (`open_bounded_log`, 512 KiB rotation).
- Children log failures at error level (`tracing::error!`), because the default filter passes only ERROR (#1133).
- Nothing is written before the spawn. Some spawners return `Option<Child>` only so tests can reap it (#1095).
- `McpHttpClient::call_tool` retries once on a stale session; otherwise no retry anywhere.

### State dir patterns

| Fact | Location |
| --- | --- |
| Shared per-session helpers: `prune_stale_json_files(dir, days)`, `reset_read_state`, `context_was_lost`, `unix_now` | `src/hook_run/session_state.rs` |
| Lock-and-write pattern: `LedgerStore` (`.lock` with `File::try_lock`, `LOCK_ATTEMPTS` polls, `write_owner_only_atomic`, prune at `STALE_DAYS`, `prune_orphan_locks`) | `src/hook_run/session_ledger.rs` |
| Path helpers: `state_dir()`, `create_dir_owner_only`, `write_owner_only_atomic`, `is_valid_short_name`, `read_dir_optional` | `crates/llmenv-paths` via `crate::paths` |
| `transcript-sessions.json` caps entries at 1000 with `pop_first` | `src/session_log/state.rs` |
| SessionStart order in `run_inner`: reset read state → scope evaluation → cbm index trigger → `resolve_memory_client` → `mcp_health::session_start_notice` (may restart the proxy) → `rt.block_on` (memory actions, session log) | `src/hook_run/mod.rs` |
| `dispatch` returns `Vec<Action>`; actions are synchronous ICM calls whose text goes into context. A resume step is a side effect, not an `Action` | `src/hook_run/mod.rs`, `src/hook_run/action.rs` |
| Doctor sections: `run_doctor_<name>(use_color, …)` prints a header and `print_check((CheckLevel, String), …)`; `run_doctor_mcp_servers` is the template | `src/cli/doctor.rs` |
| `MAX_TEXT_FILE_BYTES = 64 KiB` is the existing cap for text read from a file | `src/cli/mod.rs` |

## Decisions

1. **A checkpoint is a file the parent writes before it spawns.**
   The child deletes it on success.
   A file that is still there after its job's deadline is unfinished work.
   This is the pi-durable idea with no framework: the file is the whole protocol.
2. **One file per job**, at `state_dir/checkpoints/<kind>-<key>.json`.
   `kind` is `consolidation`, `icm-store`, `session-log-record`, or `cbm-index`.
   `key` is the first 16 hex characters of SHA-256 over the job's inputs, so a re-spawn of the same inputs maps to the same file and does not fork.
3. **The file holds everything the child needs to run again.**
   For the three llmenv children that is the exact stdin payload (or, for consolidation, the config path and cwd).
   For the cbm index it is the project root and the env the command needs.
   Cap the payload at 64 KiB; above that, skip the checkpoint and log at debug (the job still runs, as today).
4. **Phases are for consolidation only.**
   The other three jobs are one call each.
   Consolidation writes `phase` as it goes: `recalled`, `summarized`, `stored`.
   A resume starts at the recorded phase.
   The LLM summary is kept in the checkpoint after `summarized`, so a resume after an LLM success never pays the LLM again.
5. **Resume runs on `SessionStart`**, as a side effect next to the cbm index trigger, after `session_start_notice` so the proxy is up.
   Not on `resume` or `fork` sources (the earlier session already did it).
   Not inside a consolidation child.
   A checkpoint is stale when `now - started_at > deadline(kind)`, where the deadline is the job's longest timeout plus a 60 s margin.
   A stale checkpoint is re-spawned with `attempts + 1` until `MAX_ATTEMPTS = 3`; after that it stays for doctor.
   A fresh (not stale) checkpoint is left alone; its child may still be running.
6. **The child owns the checkpoint after spawn.**
   The child's entry function receives the checkpoint path (an extra hidden argument), updates `phase`, and deletes the file on success.
   On a failure that will not succeed on retry (a validate error, a 4xx), the child deletes the file and logs at error, so doctor does not nag forever.
   On a failure that may succeed later (timeout, connection refused, 5xx) the child leaves the file.
7. **Idempotency comes from #2397.**
   The `request_id` lives in the checkpoint payload, so a resumed store or record sends the same id.
8. **Doctor lists stuck checkpoints.**
   A new section `Background work:` with one line per file: kind, age, attempts, phase, and the log to read.
   `{pass}` when the directory is empty.
9. **Prune at 7 days**, same helper as the other state files, run from the resume step.

## Design

### Module `src/hook_run/checkpoint.rs`

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum JobKind { Consolidation, IcmStore, SessionLogRecord, CbmIndex }

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Checkpoint {
    pub kind: JobKind,
    pub key: String,
    pub phase: String,
    pub inputs: serde_json::Value,
    pub started_at: u64,
    pub attempts: u32,
    pub session_id: Option<String>,
}
```

Functions:

- `dir(state_dir) -> PathBuf`.
- `key_for(kind, inputs: &Value) -> String`.
- `write(state_dir, &Checkpoint) -> Result<PathBuf>` (owner-only dir, atomic write).
- `update_phase(path, phase)`.
- `complete(path)` (remove; a missing file is not an error).
- `list(state_dir) -> Vec<(PathBuf, Checkpoint)>` (unreadable files are listed as `unreadable` entries for doctor, not skipped silently).
- `stale(&Checkpoint, now) -> bool` using `deadline(kind)`.
- `resume_pending(state_dir, now, spawn: impl FnMut(&Checkpoint, &Path) -> Result<()>)` which bumps `attempts`, rewrites, and calls `spawn` for each stale checkpoint under `MAX_ATTEMPTS`.

Keep this module free of spawn logic; the spawners stay where they are and call into it.

### Spawner changes

- `post_session_consolidation`: write the checkpoint (inputs: config path, cwd, project), pass its path to `consolidation-run` as `--checkpoint <path>`.
- `handle_web_fetch_post_tool_use` and `spawn_record`: write the checkpoint with the stdin payload as `inputs`, pass `--checkpoint <path>`.
- `trigger_codebase_memory_index`: write the checkpoint; since the child is not llmenv, the parent cannot learn the exit status without waiting.
  Spawn a tiny llmenv wrapper instead: `llmenv cbm-index-run --checkpoint <path>`, a hidden subcommand that runs the cbm command, waits for it, and completes the checkpoint on exit 0.
  This also gives #2154 a place to capture the result file.
- The spawn functions keep their signatures except for the extra path; the three hidden subcommands gain an optional `--checkpoint` argument so old checkpoints and tests without one still work.

### Resume step (`src/hook_run/mod.rs`)

A function `resume_checkpoints(state_dir, now)` called from the SessionStart branch of `run_inner` after `session_start_notice`.
Its `spawn` closure matches on `kind` and calls the same command builders the spawners use (`consolidation_run_command`, the store and record command builders, the cbm wrapper).
Fail-soft: any error logs at debug and the session continues.

### Doctor (`src/cli/doctor.rs`)

`run_doctor_checkpoints(use_color, state_dir, now)`:

- Empty or missing dir: `{pass} no unfinished background work`.
- Each file: `{warn} <kind> started <age> ago, <attempts>/3 attempts, phase <phase>; log: <path>`; `{info}` when the file is fresh (child may still run); `{warn} unreadable checkpoint <path>` for a corrupt file.

### Docs

- `website/docs/troubleshooting.md`: a short section on background work, where checkpoints live, what doctor shows, and that deleting a checkpoint file abandons the job, tagged `(added in v3.12.0)`.
- `website/docs/mcp.md` memory section: consolidation and the ICM stores resume on the next session start.
- Changelog `Fixed`: background work (consolidation, memory stores, transcript records, the codebase-memory index) is checkpointed and resumed on the next session start instead of being lost.

## Tests

1. `key_for` is stable for equal inputs and differs for different inputs.
2. Write → list → complete round-trip; `complete` on a missing file is `Ok`.
3. `stale`: a checkpoint younger than its deadline is not stale; older is.
4. `resume_pending`: a stale checkpoint under the cap is re-spawned once and `attempts` increments; at `MAX_ATTEMPTS` it is not spawned; a fresh one is not spawned; the closure receives the stored inputs unchanged.
5. Each spawner test (they already exist for the command builders): the checkpoint file exists after the spawn call and the command carries `--checkpoint`.
6. Child tests: `run_icm_store_inner` with a checkpoint path deletes it on success and leaves it on a connection error (use the fail-soft harness with a closed port); same for the record child.
7. Consolidation phases: after a fake LLM success and an ICM store failure, the checkpoint holds `phase = summarized` and the summary; a resume does not call the LLM again (count calls through the backend trait).
8. Doctor formatting from a synthetic list (pure).
9. Property test: for any `Checkpoint`, serialize → deserialize is identity; `attempts` never decreases through `resume_pending`.
10. The cbm wrapper subcommand completes the checkpoint on exit 0 and leaves it on non-zero (use a stub `codebase-memory-mcp` script on `PATH`).

## As built

- The cbm index is checkpointed through `llmenv cbm-index-run`, but it is not resumed: `SessionStart` already starts the indexer and rewrites the same checkpoint, so a resume would run it twice.
- The deadlines are 150 s plus the 60 s margin for consolidation (120 s model call and 30 s ICM calls), 5 s plus the margin for the store and the record, and 1800 s plus the margin for the index.
- The consolidation checkpoint stores the working directory, and a resume starts the child there.
- A payload that does not parse deletes its checkpoint. Any other failure keeps it, and the attempt cap bounds the retries.
- A checkpoint stores the working directory of the first run, and a resume starts the child there. A checkpoint whose directory is gone is not resumed, because the child would pick the memory backend and project of the resuming session.
- A resume starts at most 20 jobs, under a lock, and restores the file when the start fails, so a failed start does not use up an attempt.
- The index job is not resumable, and its checkpoint keeps counting attempts across the rewrites that each session start makes.
- A `--checkpoint` argument must be a regular `.json` file, not a symlink, directly inside `state_dir/checkpoints`.
- Consolidation and session-log checkpoints carry a nonce. Two runs with equal inputs are two jobs, and the consolidation rule ids are scoped to the run.
- The seen-set from #2397 keeps ids in insertion order, so the oldest id goes first (a `BTreeSet` with `pop_first` would drop the smallest id, not the oldest).

## Acceptance criteria

1. Kill the memory proxy, end a session: a `consolidation-*.json` checkpoint appears; start the proxy and a new session: consolidation runs and the file disappears.
2. With the proxy down for three session starts, `llmenv doctor` shows the checkpoint with `3/3 attempts`.
3. A `WebFetch` while the proxy is down produces one ICM record after the proxy is back, not zero and not two (with #2397).
4. Changelog entry; troubleshooting and MCP docs updated with the version tag.

## Out of scope

- Retrying inside a running child (a child still makes one attempt and exits).
- A general job queue or scheduler.
- Checkpoints for the synchronous `Action::Store` at SessionEnd (it already has the `HOOK_STORE_CHUNK` dedup and the hook waits for it).
