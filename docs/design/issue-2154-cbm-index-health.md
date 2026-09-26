# Issue #2154 — report codebase-memory auto-index results; memory budget setting

- **Issue:** https://github.com/phaedrus1992/llmenv/issues/2154
- **Milestone:** `v3.12.0`
- **Base branch:** `release/3.x` (forward-merges to `release/4.x`)
- **Type:** feature

This is a spec, not a plan.

## Problem

On `SessionStart`, llmenv starts `codebase-memory-mcp cli index_repository '{"repo_path": …}'` as a detached child (`trigger_codebase_memory_index`, `src/hook_run/mod.rs`).
Its stdout goes to `/dev/null` and its stderr to a bounded `index.log`.
cbm prints the index result as JSON on stdout, so llmenv throws away the one structured answer about whether indexing worked.

cbm 0.11.0 makes failures explicit:

- An index over its memory budget fails whole, keeps the previous index, and says so in the result.
- A daemon start failure names the stage, errno and path.

Users also have no llmenv setting for cbm's memory budget (`CBM_MEM_BUDGET_MB`).

## cbm facts (source at tag `v0.11.0`)

| Fact | Location |
| --- | --- |
| `cli <tool> <json>` prints the tool result with `printf("%s\n", result)` on stdout | `src/main.c` line 1037 |
| Log lines go to stderr, as JSON or as `level=… msg=… key=value` text | `src/foundation/log.c` line 264 |
| Over-budget result fields: `status: "error"`, `reason: "over_memory_budget"`, `previous_index: "preserved"`, `budget_mb`, `peak_rss_mb`, `suggested_budget_mb`, `hint` | `src/mcp/mcp.c` lines 11224 to 11260 |
| Other non-success `status` values in `handle_index_repository`: `aborted_previous_preserved`, `ambiguous`, `persist_failed` | `src/mcp/mcp.c` from line 10984 |
| `CBM_MEM_BUDGET_MB`: strict decimal parse; a value above total RAM is refused; unset means a fraction of RAM | `src/foundation/mem.c` near lines 200 to 240 |
| `watcher_enabled` is a key in cbm's own config (`codebase-memory-mcp config`), default `true`, not an environment variable | `src/cli/cli.h` line 428, `src/cli/cli.c` line 7403 |

## Verified locations (release/3.x)

| Fact | Location |
| --- | --- |
| `CodebaseMemory { when, index_path, mcp_permissions }` | `crates/llmenv-config/src/schema.rs` line 1407 |
| MCP launch env: `CBM_CACHE_DIR` only when `index_path` is set | `resolve_codebase_memory`, `src/mcp/resolve.rs` near line 285 |
| Auto-index command: same env rule, stdout and stderr set to null in the builder | `build_index_repository_command`, `src/hook_run/mod.rs` |
| Log path: `codebase_memory_cache_dir(cm, state_dir).join("index.log")` | `trigger_codebase_memory_index`, `src/hook_run/mod.rs` |
| `state_dir` is global (`crate::paths::state_dir()`), not per project | `codebase_memory_paths`, `src/mcp/resolve.rs` line 267 |
| `sha2` and `hex` are dependencies | `Cargo.toml` lines 68 to 69 |

## Decisions

1. **Keep the structured result, not a parsed log.** Send the auto-index child's stdout to a result file. Parse it when doctor reads it.
2. **One result file per project.** `state_dir` is shared, so the file name carries a stable hash of the project root.
3. **Doctor shows the result.** `llmenv status` is not changed.
4. **Budget is a setting; the watcher is not.** `mem_budget_mb` becomes a `codebase_memory` field passed as `CBM_MEM_BUDGET_MB`. `watcher_enabled` stays cbm's own config (the same stance llmenv takes on `CBM_ALLOWED_ROOT`); the docs say how to set it.

## Design

### Config

Add to `CodebaseMemory`:

```rust
/// Memory budget for codebase-memory-mcp indexing, in MB (`CBM_MEM_BUDGET_MB`).
/// Unset leaves the variable unset, so the server uses its own default.
#[serde(default, skip_serializing_if = "Option::is_none")]
pub mem_budget_mb: Option<u32>,
```

Validation: `1..=1_048_576`; outside that, a `ValidateError` naming the field, the value and the range.
cbm refuses a budget above the machine's RAM on its own; llmenv does not check RAM.

When set, add `CBM_MEM_BUDGET_MB=<n>` to the env in **both** `resolve_codebase_memory` and `build_index_repository_command`.
These two must agree, as they already do for `CBM_CACHE_DIR`; update both doc comments.

### Result file

In `trigger_codebase_memory_index`:

- Result path: `codebase_memory_cache_dir(cm, state_dir).join(format!("index-result-{key}.json"))`, where `key` is the first 16 hex characters of SHA-256 over the project root path bytes (`OsStr` bytes on Unix; the UTF-8 lossy form elsewhere).
- Put the key computation in one function, `fn index_result_path(cache_dir: &Path, project_root: &Path) -> PathBuf`, used by both the writer and doctor.
- Open the file with create, truncate and write, and pass it as the child's stdout.
  If the open fails, fall back to null stdout and log at debug level, as the log file does today.
- Apply the same directory permission rule the log file uses (0700 only for the default cache dir).

`build_index_repository_command` keeps returning a command with null stdout; the trigger function replaces stdout, the same way it replaces stderr today.

### Doctor

When `features.codebase_memory` has entries, in the codebase-memory section:

1. Compute the result path for the current directory's project root (`codebase_memory_paths()`).
2. No file: `{info} codebase-memory: no index result for this project yet`.
3. File present: parse it as JSON. The file's modification time is the finish time.
   - Not JSON or empty: `{info} codebase-memory: last index finished <time>; result not readable (the run may still be going or was killed)`.
   - `reason == "over_memory_budget"`: `{warn} codebase-memory: last index (<time>) stopped at the memory budget: budget <budget_mb> MB, peak <peak_rss_mb> MB. The previous index is still served. Set codebase_memory.mem_budget_mb to <suggested_budget_mb>.` Omit a number that is missing.
   - `status` is `error`, `aborted_previous_preserved`, `ambiguous` or `persist_failed` (any other reason): `{warn} codebase-memory: last index (<time>) ended with status <status><, reason <reason>>. <hint if present>. Log: <index.log path>`.
   - Otherwise: `{pass} codebase-memory: last index finished <time>`.
4. Format `<time>` as UTC, `YYYY-MM-DD HH:MM UTC`, with the existing `jiff` dependency (`jiff::Timestamp::try_from(SystemTime)`, then `strftime("%Y-%m-%d %H:%M UTC")`).
   `jiff` is built with `default-features = false, features = ["std"]` on `release/3.x`, which has no time-zone database, so do not print local time and do not add a `jiff` feature for it.

Put the classification in a pure function over `(serde_json::Value)` so tests need no files.

## Tests

1. `mem_budget_mb` validation: 0, 1, 1_048_576, 1_048_577.
2. Env: with `mem_budget_mb: 8192`, both the resolved MCP entry and the index command carry `CBM_MEM_BUDGET_MB=8192`; without it, neither has the key.
3. `index_result_path` is stable for one path and differs for two paths.
4. Classification table: over-budget with all fields, over-budget missing `suggested_budget_mb`, each other status, a success object, invalid JSON, empty text.
5. Command test: the trigger sets stdout to the result file (factor the stdout choice so it is testable, as the stderr choice is).

## Acceptance criteria

1. After a session start in an indexed project, `llmenv doctor` shows `last index finished …`.
2. With `mem_budget_mb: 128` on a large repository, doctor shows the over-budget warning with a suggested value.
3. Changelog `Added`: `codebase_memory.mem_budget_mb`; doctor reports the last codebase-memory index result.
4. `website/docs/mcp.md` codebase-memory section documents `mem_budget_mb`, the doctor line, and `codebase-memory-mcp config set watcher_enabled false` for turning the background watcher off, tagged `(added in v3.12.0)`.

## Out of scope

- Retrying a failed index automatically.
- Owning cbm's `watcher_enabled` or `auto_watch` config keys.
