<!-- markdownlint-disable MD013 -->

# Trace Telemetry

When `LLMENV_TRACE_TIMING` is set, llmenv emits structured telemetry lines to
stderr during the export and hook phases. When `LLMENV_TRACE_FILE` is set, the
same lines are appended as JSON objects to the named file using `O_APPEND`, so
concurrent hooks and detached jobs never interleave output.

Two environment variables control where lines go:

- `LLMENV_TRACE_TIMING` — when set, every trace line is also written to stderr.
  This is the original path and remains the default for interactive shells.
- `LLMENV_TRACE_FILE` — when set, every trace line is appended as one JSON
  object to the named file using `O_APPEND`. Concurrent hooks and detached jobs
  use this path so their output reaches a file instead of a detached stderr.

The JSON marker (`llmenv-trace <json>`) carries a `"v": 1` field so a consumer
can distinguish formats when new fields are added. All timing values use
microseconds (`_us`).

## Line formats

### `[LLMENV_CACHE]`

Emitted when the content hash cache is checked.

```text
[LLMENV_CACHE] <name> <hit|miss> <duration>ms[ <extra>]
```

Fields:

- `<name>` — cache key (e.g. `content_hash`, `merge_sig`, `read_once`).
- `<hit|miss>` — whether the cache returned a valid entry.
- `<duration>ms` — elapsed time in milliseconds with three decimal places.
- `<extra>` — optional extra data (present only on misses, contains the lookup
  key).

### `[LLMENV_MATERIALIZE]`

Emitted after each adapter's materialize step when `LLMENV_TRACE_TIMING` is set.

```text
[LLMENV_MATERIALIZE] <adapter> written=<n> skipped=<n> bytes=<n> <us>us
```

Fields:

- `<adapter>` — adapter name (e.g. `claude-code`, `crush`).
- `written=<n>` — files the adapter wrote during this step.
- `skipped=<n>` — files left alone because they already held the same bytes at the same mode.
- `bytes=<n>` — total bytes of written files.
- `<us>us` — elapsed time in microseconds.

The counts come from the shared owner-only writers, so a second `llmenv regenerate` with no
config change reports `written=0` for an adapter whose files are already current.

### `[LLMENV_SPAWN]`

Emitted when a spawned child process ends. Scope probes, `git`, the keychain `security` call, the statusline `gh` call, the consolidation `claude` call, the sync `git` calls, and the MCP stdio probes each print one line. A detached job start (session-log record, ICM store, consolidation, codebase-memory index, respawn, proxy start) prints one line with `exit=detached`, or `exit=spawn-error` when the start fails. Its duration is the start time, not the job's run time.

```text
[LLMENV_SPAWN] <label> <duration>us exit=<code|signal|timeout|spawn-error|detached>
```

Fields:

- `<label>` — fixed string such as `probe:ssid` or `git:local-query`. Never prints
  arguments, because they can carry secrets such as keychain item names.
- `<duration>us` — elapsed time in microseconds.
- `exit=<...>` — the exit code, `signal` (ended by a signal), `timeout` (killed at the deadline), `spawn-error` (did not start), or `detached` (started and not waited for).

### `[LLMENV_MCP_CALL]`

Emitted for MCP handshake and tool calls.

Handshake:

```text
[LLMENV_MCP_CALL] <method> <duration>us req=<bytes> resp=<bytes>[ status=<code>]
```

- `<method>` — `initialize` or `notifications/initialized`.
- `<duration>us` — elapsed time in microseconds.
- `req=<bytes>` — request payload size in bytes.
- `resp=<bytes>` — response body size in bytes. It is `0` when the call does not read the reply body.
- `status=<code>` — present only on failure, the HTTP status code.

Tool calls:

```text
[LLMENV_MCP_CALL] <tool> <duration>us req=<bytes> resp=<bytes>
```

- `<tool>` — tool name (e.g. `icm_memory_recall`).
- `<duration>us` — elapsed time in microseconds.
- `req=<bytes>` — request payload size in bytes.
- `resp=<bytes>` — response payload size in bytes.

Emitted on success only. Failure is logged via `tracing` instead, matching the
precedent of `emit_trace_timing` and `emit_cache_trace`. (added in v3.14.0)

### `llmenv-trace` JSON marker

Emitted at the end of the full export pipeline.

```json
llmenv-trace {"v": 1, "op": "hook_run", "config_load_us": <n>, ...}
```

The object is flat: the timing fields sit beside `v` and `op`. Each timing field is
optional — a phase that did not run is left out.

Fields:

- `config_load_us` — time to load configuration (always present).
- `scope_eval_us` — time to evaluate active scopes (present when scopes exist).
- `prep_us` — time to prepare the manifest (present when scopes exist).
- `mcp_us` — time for MCP calls (present when prep completes).

### `llmenv-trace` export marker

Emitted once at the end of `llmenv export`, and once at the end of `llmenv regenerate`
(with `"op": "regenerate"` and no `render_vars_us`), with per-phase timing.

```json
llmenv-trace {"v": 1, "op": "export", "config_load_us": <n>, ...}
```

Fields:

- `config_load_us` — time to load configuration (always present).
- `detect_env_us` — time to detect the environment (present when env is set).
- `evaluate_active_us` — time to evaluate active scopes (present when scopes
  exist).
- `build_manifest_us` — time to build the merged manifest (present when scopes
  exist).
- `materialize_adapters_us` — time for adapter materialize (present when
  manifest exists).
- `render_vars_us` — time to render environment variables (present when
  adapters materialize).

### `[LLMENV_LEDGER]`

Emitted when adaptive recall ledger timing is enabled.

```text
[LLMENV_LEDGER] <flow> lock_attempts=<n> lock_wait_us=<n> read_us=<n> write_us=<n> bytes=<n> state_unsaved=<true|false>
```

Fields:

- `<flow>` — flow name: `session_start`, `turn_start`, `subagent_start`, or `post_tool_use_failure`.
- `lock_attempts=<n>` — number of lock polling attempts.
- `lock_wait_us=<n>` — time spent waiting for the lock in microseconds.
- `read_us=<n>` — time to read the ledger file in microseconds.
- `write_us=<n>` — time to write the ledger file in microseconds.
- `bytes=<n>` — number of bytes in the ledger file (empty when unavailable).
- `state_unsaved=<true|false>` — whether the change did not reach disk: a busy lock, an unreadable ledger, or a failed save.

A field for a step that did not run prints `na`. A busy lock prints `na` for `read_us`, `write_us`, and `bytes`.

A ledger update from `post_tool_batch` or `subagent_task` prints a shorter line with its total time:

```text
[LLMENV_LEDGER] <op> <duration>us
```

Emitted from `src/hook_run/adaptive.rs` when `LLMENV_TRACE_TIMING` is set.
(added in v3.14.0)

### `[LLMENV_CONTEXT]`

Emitted after each recall wave when the recall budget is non-empty.

```text
[LLMENV_CONTEXT] recall_entries=<n> recall_bytes=<n> injected_entries=<n> injected_bytes=<n> duplicate_records=<n> already_sent=<n> omitted=<n> skipped_actions=<n>
```

Fields:

- `recall_entries=<n>` — total records returned by ICM.
- `recall_bytes=<n>` — total bytes of recalled records.
- `injected_entries=<n>` — records injected into the prompt.
- `injected_bytes=<n>` — bytes of injected records.
- `duplicate_records=<n>` — records dropped as duplicates.
- `already_sent=<n>` — records skipped because the context already holds them.
- `omitted=<n>` — records dropped for the byte budget.
- `skipped_actions=<n>` — recall queries not run because the budget was full.

Emitted from `src/hook_run/recall.rs` when `LLMENV_TRACE_TIMING` is set.

### `llmenv-trace` session-start sub-phases

The `llmenv-trace` JSON marker for `op: "hook_run"` carries optional sub-phase
fields when `SessionStart` runs its fan-out.

Fields:

- `mcp_probe_us` — time for the initial MCP health probe.
- `proxy_restart_us` — time for the proxy restart attempt.
- `drift_check_us` — time for the drift re-probe.
- `stale_hint_us` — time for the stale state/tmp hint check.
- `cbm_roots_us` — time for the codebase-memory allow-root check.
- `session_log_us` — time to capture the session log.
- `checkpoint_us` — time to begin the codebase-memory index checkpoint.
- `spawn_us` — time to restart unfinished detached checkpoint jobs (`session_start` only).
- `spawned_jobs` — number of detached jobs the hook run started: session-log records, consolidation, ICM stores, and codebase-memory indexes. It is `0` when the run started none.

All sub-phase fields are in microseconds and are present only when the
sub-phase ran. The `session_start` sub-phases run inside `prep_us`, so
`prep_us` minus their sum is the time spent on other work. The memory calls
inside `mcp_us` each print their own `[LLMENV_MCP_CALL]` line. (added in v3.14.0)

### `llmenv-trace` statusline marker

Emitted once per `llmenv statusline` render. (added in v3.14.0)

```json
llmenv-trace {"v": 1, "op": "statusline", "total_us": <n>, "<widget>_us": <n>, ...}
```

- `total_us` — wall time of the whole render in microseconds.
- `<widget>_us` — time to render each widget that ran, such as `branch_us` or `pr_us`.

The marker goes to stderr or to `LLMENV_TRACE_FILE`. Stdout is the same with tracing on or off.

### Guard command trace markers

The five guard commands (`config-guard`, `config-context`, `check-stale`,
`read-once clear`, `throttle`) each emit one `llmenv-trace` marker with `op: "guard"`.

```text
llmenv-trace {"v": 1, "op": "guard", "command": "<name>", "total_us": <n>}
```

- `command` — the guard command name.
- `total_us` — wall time of the whole command in microseconds.

These markers let a harness compare per-call guard cost with `hook-run
pre_tool_use`. (added in v3.14.0)

### `[LLMENV_DETACHED]`

Emitted when a detached job (post-session memory consolidation, ICM transcript
record, ICM memory store, or codebase-memory index) exits.

```text
[LLMENV_DETACHED] <kind> <wall_us>us cpu_us=<n> mcp_calls=<n> exit=<code>[ request_id=<id>]
```

Fields:

- `<kind>` — job kind: `consolidation`, `icm-store`, `session-log-record`, or
  `cbm-index`.
- `<wall_us>` — total wall time in microseconds.
- `cpu_us=<n>` — user plus system CPU time of the job process in microseconds.
- `mcp_calls=<n>` — number of MCP tool calls the job process made.
- `exit=<code>` — `0` when the job succeeded, `1` when it failed.
- `request_id=<id>` — the request id the parent logged for the job. Present for
  `icm-store` and `session-log-record` only.

Emitted from the child entrypoints of all detached job types when
a trace sink is on. A detached job has no visible stderr, so the line
also goes to `LLMENV_TRACE_FILE`, or to `detached-trace.log` in the state
directory when that variable is unset. That log moves to `detached-trace.log.1` at 1 MiB, so it stays bounded.
(added in v3.14.0)

## JSON trace file format

When `LLMENV_TRACE_FILE` is set, every trace line is appended as one JSON object
to the named file. The object carries `"ts"` (Unix epoch seconds, a number), `"pid"`, and
`"line"` (the original text). A consumer can reconstruct the original text while
still being able to parse the file as JSON Lines.

(added in v3.14.0) A trace file that cannot be written, a failed stderr write, and a trace
line dropped for a reserved field each reach the agent as an `llmenv:` notice. See
[Troubleshooting](troubleshooting.md#the-agent-sees-an-llmenv-notice).

```json
{"ts": 1729700000, "pid": 12345, "line": "[LLMENV_CACHE] content_hash hit 1.234ms"}
```

## Versioning

The `"v": 1` field in JSON markers allows consumers to distinguish formats when
new fields are added by later sub-issues. All timing values use microseconds
(`_us`). New trace line formats should use a distinct prefix (e.g.
`[LLMENV_<NAME>]`) so consumers can parse them without ambiguity.
