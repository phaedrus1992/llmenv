# Issue #2406 — configurable codebase-memory allowed roots, startup probe, index guard

- **Issue:** https://github.com/phaedrus1992/llmenv/issues/2406
- **Milestone:** `v3.12.0`
- **Base branch:** `release/3.x` (forward-merges to `release/4.x`)
- **Type:** feature (config, resolve, SessionStart probe, doctor, PreToolUse guard)
- **Split from:** #2376 (shipped: no `CBM_ALLOWED_ROOT` pin, stale env keys dropped on re-render)
- **Touches the same struct as:** #2154 (`mem_budget_mb`, see `issue-2154-cbm-index-health.md`)

This is a spec, not a plan.

## Problem

The llmenv-launched codebase-memory server refuses to index paths outside its allowed roots.
Today the only way to allow a root is the tool's own `allow-root` command, run by hand, and nothing in llmenv knows which roots are allowed.
So:

1. The code-explorer cache (`~/.cache/nbl-diag/repos`), the llmenv config dir, and llmenv's own cache and state dirs are refused until someone notices.
2. Nothing confirms at session start that the server can index the project it is running in, or that its cache dir is the one the account daemon uses.
3. The `index_repository` PreToolUse guard lets a refused path through to the server, whose error sends the agent to `allow-root`.

## cbm facts to pin before implementing

The design below has one open fact: **how cbm stores allowed roots**.
Read the cbm source at the installed version (the code-explorer cache holds it; check `codebase-memory-mcp --version` and pin the clone to that tag) and record the answer in this table before writing code.

| Question | Where to look | Why it matters |
| --- | --- | --- |
| Is `allow-root` a CLI command that writes a runtime config file under `CBM_CACHE_DIR`, or does the server read an env list? | `src/cli/cli.c` (the `allow-root` and `config` handlers), `src/foundation/` config loading | Picks between "llmenv runs `codebase-memory-mcp allow-root <path>` with the same `CBM_CACHE_DIR`" and "llmenv sets an env var in both launch paths" |
| Does the index refusal come back as a tool error with a stable `reason`? | `handle_index_repository` in `src/mcp/mcp.c` | The guard message and the doctor check key off it |
| Does the daemon share the allowed-roots list across all clients of one cache dir? | daemon startup in `src/mcp/` | Decides whether a per-project list is even possible, or whether roots are per cache dir |
| Is there a `cli` form that lists the roots (for the probe)? | `src/cli/cli.c` | The probe and doctor need a read-only query |

If roots are a runtime config file: llmenv writes them through the tool's own command (decision 2), never by editing the file.
If roots are an env list: llmenv sets it in both launch paths (decision 3).

## Verified facts (release/3.x)

| Fact | Location |
| --- | --- |
| `CodebaseMemory { when, index_path: Option<String>, mcp_permissions: Option<McpPermissions> }`, not `deny_unknown_fields`; doc comment says `CBM_ALLOWED_ROOT` is never set (#1495) | `crates/llmenv-config/src/schema.rs` |
| `resolve_codebase_memory(cm, project_root, state_dir)` sets only `CBM_CACHE_DIR` (when `index_path` is set), command `codebase-memory-mcp`, no args; `resolve_codebase_memory_entries` enforces one active entry | `src/mcp/resolve.rs` |
| `codebase_memory_paths()` returns `(cwd, state_dir)`; `state_dir` is global | `src/mcp/resolve.rs` |
| Tests `resolve_codebase_memory_never_sets_allowed_root` and `codebase_memory_resolves_to_local_stdio` assert `CBM_ALLOWED_ROOT` is absent; the index-command test in `src/hook_run/mod.rs` asserts the same | tests |
| Auto-index: `trigger_codebase_memory_index` → `build_index_repository_command` runs `codebase-memory-mcp cli index_repository '{"repo_path": …}'` with the same `CBM_CACHE_DIR` rule (the "agreement rule": the MCP launch env and the index command env must match) | `src/hook_run/mod.rs` |
| Guard: `cbm_index_guard::handle_pre_tool_use` matches `INDEX_REPOSITORY_TOOL`, denies a non-empty `name` override and `persistence: true`, replies with the `__DENY__:` prefix, is first in `resolve_pre_tool_decision`; no `repo_path` check | `src/hook_run/cbm_index_guard.rs`, `src/hook_run/mod.rs` |
| SessionStart health probe: `mcp_health::session_start_notice` sends an MCP `initialize` to managed servers, including cbm over stdio; `effect_and_fix(CODEBASE_MEMORY_MCP_NAME)` supplies fix text | `src/hook_run/mcp_health.rs` |
| Doctor: `run_doctor_dependent_tools` has the cbm version floor (#2153); `run_doctor_mcp_servers` probes; `orphan_codebase_memory_entries` under `--all` | `src/cli/doctor.rs` |
| #2376 added `RENDERED_ENTRY_KEYS` and `carry_runtime_keys` so stale env pins do not survive a re-render | `src/adapter/claude_code/mod.rs` |
| `$NBL_DIAG_CACHE` is referenced nowhere in the repo; the nbl-diag plugin defaults it to `~/.cache/nbl-diag` | none |
| Docs: `configuration.md` `### features.codebase_memory:`, `mcp.md` "Codebase memory", "The `index_repository` name guard", "Session-start health check" | docs |

## Decisions

1. **`allowed_roots` is a list on `features.codebase_memory[]`**, and llmenv always adds a default set.
   Defaults: the project root (cwd at session start), `~/.config/llmenv`, the llmenv cache dir, the llmenv state dir, and `${NBL_DIAG_CACHE:-~/.cache/nbl-diag}/repos`.
   User entries are appended.
   Entries expand `~` and `$VAR` at resolve time; an entry whose variable is unset is dropped with a debug log, not an error.
   A relative entry is an error (`ValidateError`, with the fix: use an absolute path, `~`, or a variable).
2. **If cbm stores roots in its own config, llmenv applies them through the tool's command**, once per session start, with the same `CBM_CACHE_DIR` the server gets.
   llmenv never edits cbm's config file by hand (same stance as `watcher_enabled` in #2154).
   The command runs synchronously with a 5 s timeout before the index trigger, so the index sees the roots.
3. **If cbm reads an env list, both launch paths set it**, in `resolve_codebase_memory` and `build_index_repository_command`, by the agreement rule.
   The two existing tests that assert `CBM_ALLOWED_ROOT` is absent change to assert the two paths agree.
4. **The SessionStart probe is part of `session_start_notice`.**
   After the `initialize` probe succeeds for cbm, ask the server (read-only) whether each resolved root is allowed and which cache dir it serves.
   Report in the session-start context block: `codebase-memory: roots <a>, <b>, …` on success; on a refused root or a cache-dir mismatch, a loud `{warn}`-style line that names the root, the fix (`features.codebase_memory[].allowed_roots`), and the cache dir mismatch (`index_path` vs the daemon's dir).
   If the read-only query does not exist in cbm, the probe reports only the cache dir and the configured roots, and the guard (decision 6) does the per-call check.
5. **Doctor does the same check** in the codebase-memory part of `run_doctor_mcp_servers` (or a sibling `run_doctor_codebase_memory` if that function is near the size limit).
6. **The guard pre-checks `repo_path`.**
   Resolve the roots the same way the probe does, canonicalize `repo_path` (`fs::canonicalize`, so a symlink into a root is allowed and a `..` escape is not), and deny with:
   `__DENY__: <path> is outside the codebase-memory allowed roots (<list>). Add it to features.codebase_memory[].allowed_roots in llmenv config and run llmenv regenerate; do not run allow-root by hand.`
   A `repo_path` that does not exist is denied with that fact, since cbm would fail anyway.
   A missing `repo_path` is left to the server (it indexes cwd).
7. **One resolver for the roots.**
   `resolve_allowed_roots(cm, project_root, env) -> Vec<PathBuf>` in `src/mcp/resolve.rs`, used by the launch env or command, the probe, doctor, and the guard.
   No second copy of the default list anywhere.

## Design

### Config (`crates/llmenv-config/src/schema.rs`, `validate.rs`)

```rust
/// Extra roots codebase-memory-mcp may index, on top of llmenv's defaults
/// (project root, the llmenv config, cache and state dirs, and the
/// code-explorer cache). `~` and `$VAR` expand at session start.
#[serde(default, skip_serializing_if = "Vec::is_empty")]
pub allowed_roots: Vec<String>,
```

Validation: each entry non-empty and, after a syntactic `~`/`$VAR` check, absolute.

### Resolve (`src/mcp/resolve.rs`)

- `DEFAULT_ALLOWED_ROOTS` as a function (it needs the project root and env), not a const.
- `resolve_allowed_roots` dedups and keeps order (defaults first).
- `resolve_codebase_memory` and `build_index_repository_command` call it and apply decision 2 or 3.

### Probe (`src/hook_run/mcp_health.rs`)

- `cbm_roots_report(resolved_mcp, roots, timeout) -> RootsReport { allowed: Vec<PathBuf>, refused: Vec<PathBuf>, cache_dir: Option<PathBuf>, expected_cache_dir: Option<PathBuf> }`.
- `session_start_notice` appends the report's lines.
  Keep this in a helper so `session_start_notice` stays under the function limit.

### Guard (`src/hook_run/cbm_index_guard.rs`)

- `handle_pre_tool_use` gains access to the resolved roots (pass them in from `resolve_pre_tool_decision`, which already has the config and cwd).
- `path_outside_roots(repo_path, roots) -> Option<String>` is pure over canonical paths; the canonicalize step sits in the caller so the pure function is testable with temp dirs.

### Doctor (`src/cli/doctor.rs`)

- Print the roots, the cache dir, and each refused root with the same message as the probe, as `{warn}`.

### Docs

- `website/docs/configuration.md` `### features.codebase_memory:`: `allowed_roots`, the default list, expansion rules, `(added in v3.12.0)`.
- `website/docs/mcp.md`: update "The `index_repository` name guard" to cover the path check; the session-start section lists the roots line; remove any sentence that tells users to run `allow-root` by hand and say to use the config field.
- Changelog `Added`: `features.codebase_memory[].allowed_roots` with llmenv defaults; session start and doctor report refused roots and cache-dir mismatches; the index guard names the config fix.

## Tests

1. `resolve_allowed_roots`: defaults present in order; a user root appended; `~` and `$NBL_DIAG_CACHE` expanded; an unset variable drops the entry; duplicates removed.
2. Validation: a relative entry errors; an empty entry errors.
3. Agreement: the MCP launch env and the index command agree on the roots (whichever mechanism decision 2/3 picks).
4. Guard table: path inside a root allowed; `..` escape denied; symlink into a root allowed; missing path denied; no `repo_path` passes through; the deny text names `allowed_roots`.
5. Probe formatting from a synthetic `RootsReport` (pure).
6. Doctor formatting from the same report.
7. Property test: for any set of roots and any path under one of them, `path_outside_roots` is `None`; for a path with no root prefix it is `Some`.

## Acceptance criteria

1. In a project with the defaults, `index_repository` on `~/.cache/nbl-diag/repos/<repo>` succeeds through the llmenv-launched server.
2. `index_repository` on `/tmp/elsewhere` is denied by the guard with the config fix, without reaching the server.
3. The session-start context lists the roots; with `index_path` pointing at a dir the daemon does not use, it warns about the mismatch.
4. `llmenv doctor` shows the same.
5. Changelog entry; configuration and MCP docs updated with the version tag.

## Out of scope

- Owning cbm's `watcher_enabled` or other runtime keys (#2154 stance).
- Indexing more than one project per session (one active entry stays the rule).
- Changing cbm.

## As built

Pinned facts about codebase-memory-mcp 0.11.0:

| Command | Effect |
| --- | --- |
| `allow-root <path>` | Records the root in `<cache dir>/allowed_roots`. |
| `allow-root --list` | Prints `allowed roots:` and one path per line. Read-only. With no roots it prints that indexing is unconfined. |
| `allow-root --approve-sensitive <path>` | Approves a sensitive path. llmenv does not use it. |

Differences from the plan:

1. `src/mcp/cbm_roots.rs` holds the resolver, the command runner, the report, and the guard decision.
2. The guard allows the configured roots of every `codebase_memory` entry plus the roots that `allow-root --list` prints, not the active entry only. This avoids scope evaluation in a hook.
3. llmenv cannot detect a cache-dir mismatch, because the server has no query for its cache dir. The notice and doctor print the expected cache dir (`index_path`, or the server default) instead.
4. The SessionStart step runs `allow-root` only for roots that `--list` lacks, and skips a root that does not exist.
