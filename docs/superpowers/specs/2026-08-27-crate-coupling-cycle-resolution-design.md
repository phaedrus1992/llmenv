<!-- markdownlint-disable MD013 -->
# Crate coupling: cycle resolution design

Target milestone: **v4.0.0**. Tracked in #1462 (part of #1339's epic).

## Problem

The build-time audit
(`docs/superpowers/specs/2026-08-20-crate-coupling-build-time-audit.md`)
found `adapter`, `cli`, `hook_run`, `materialize`, and `merge` locked in
circular internal dependencies. Rust does not allow circular crate
dependencies, so none of these five modules — the five largest in the
codebase — can become a standalone workspace crate until its cycles are
broken. This doc is the resolution design #1462 asks for: for each cycle,
which symbol moves where, or why the cycle should be accepted as-is.

**Scope note:** this is a design deliverable, not an implementation one for
crate extraction. It does relocate the specific symbols that cause each
cycle (small, mechanical moves), and verifies afterward that no cycle
remains. It does **not** extract `adapter`/`cli`/`hook_run`/`materialize`/
`merge`/`consolidation` into their own workspace crates — that follows in
separate issues, the same pattern `scope`/`mcp`/`task` used (#1459–#1461).

## Re-verification against current `HEAD`

The audit is dated 2026-08-20; `scope`, `mcp`, and `task` were extracted into
`llmenv-scope`/`llmenv-mcp`/`llmenv-task` since then (#1459–#1461), changing
import paths elsewhere in the tree. Before designing on top of the audit's
findings, every claimed cycle was re-verified by direct `rg` in both
directions.

**Result: `adapter` ↔ `cli` is no longer circular.** `src/cli/` has many real
references into `crate::adapter` (registry lookups, `AgentAdapter`, tool
lists — `src/cli/setup.rs`, `src/cli/doctor.rs`). `src/adapter/` has exactly
one match for `crate::cli`, and it is a doc comment
(`src/adapter/codex.rs:50`), not code. This pair is a one-way dependency
(`cli` → `adapter`), not a cycle. It needs no resolution.

The other five audit-flagged pairs, plus the two lower-priority pairs
(`consolidation` ↔ `hook_run`, `hook_run` ↔ `session_log`), are confirmed
real: **seven cycles total**, covered below.

## Resolution philosophy

Every real cycle here has the same shape: one direction is a genuine,
heavy, layered dependency (e.g. `cli` orchestrating `materialize`'s
render/cache engine, one-way, dozens of call sites). The other direction is
one or two small symbols — a constant, a data struct, a stateless helper
function — that happen to live on the wrong side of the boundary. In every
case checked, that thin symbol has no reason to stay where it is: it is pure
data or pure logic with no dependency on its current module's other
internals. Relocating it to a lower shared layer (an existing zero-fan-out
crate, or the module that is its natural conceptual owner) removes the cycle
without introducing a trait, an event boundary, or a merged module. No cycle
in this set needed either of those heavier tools.

## Per-cycle resolutions

### 1. `adapter` ↔ `hook_run`

- **Thin edge:** `hook_run::cbm_index_guard::INDEX_REPOSITORY_TOOL` (a
  `pub(crate) const &str`, `src/hook_run/cbm_index_guard.rs:29`), used by
  `src/adapter/claude_code.rs:1485,3779`.
- **Heavy edge (kept, one-way):** `hook_run` uses the adapter registry —
  `AgentAdapter`, `adapter_for_engine`, `unknown_engine_error`,
  `registered_adapters`, `engine_id` (`src/hook_run/mod.rs:359-2765`).
- **Fix:** move `INDEX_REPOSITORY_TOOL` into `llmenv-mcp`, next to the
  existing `CODEBASE_MEMORY_MCP_NAME` (`crates/llmenv-mcp/src/resolve.rs`).
  It is the MCP tool name that identifies the codebase-memory index tool —
  a natural fit alongside the other MCP-identity constants. Both `adapter`
  and `hook_run` reference it from there afterward.

### 2. `adapter` ↔ `materialize`

- **Thin edge:** `src/materialize/inherit.rs:99,851` calls
  `crate::adapter::skills::create_dir_owner_only`, which is itself a 2-line
  wrapper (`src/adapter/skills.rs:21-24`) around
  `crate::paths::create_dir_owner_only`.
- **Heavy edge (kept, one-way):** `adapter` uses
  `materialize::schema_gen::with_root_additional_properties`,
  `materialize::bundle_file_mode`, `materialize::prune_empty_dirs`.
- **Fix:** no relocation needed. Point `materialize`'s two call sites at
  `crate::paths::create_dir_owner_only` directly. `adapter`'s own wrapper
  stays — it has 10+ internal callers
  (`output_styles.rs`, `codex.rs`, `crush.rs`, `opencode.rs`,
  `claude_code.rs`) and is not itself part of the cycle.

### 3. `cli` ↔ `hook_run`

- **Thin edge:** `hook_run` calls several `pub(crate)` items defined in
  `src/cli/mod.rs`:
  - Bundle/marker selection: `firing_bundles`, `build_bundle_refs`,
    `marker_enabled_bundle_names`, `marker_disabled_bundle_names`,
    `tag_or_marker_selected` — pure functions over `Bundle`/`ActiveScopes`,
    no I/O.
  - Drift/staleness: `should_use_color`, `run_check_stale`
    (`src/hook_run/mod.rs:490-491`, called from the `SessionStart` path to
    warn on config drift).
- **Heavy edge (kept, one-way):** `cli` invokes the hook execution engine —
  `hook_run::run`, `HookExit`, `detached_store::run_icm_store`,
  `detached_consolidation::run_consolidation`, `read_once::*`,
  `suppressed_memory_bundles`, `load_cached_config`.
- **Fix:**
  - New module `crate::bundle_select` holds the five bundle/marker-selection
    functions, moved out of `cli/mod.rs`. Both `cli` and `hook_run` depend
    on it one-way; it depends on `scope`/`config` only.
  - `should_use_color` moves to `llmenv-util` (pure, no internal deps).
  - `run_check_stale` moves into `materialize` alongside `StaleStatus`/
    `stale_status()` (cycle 4) — it is the shared orchestration point both
    `cli`'s `check-stale` subcommand and `hook_run`'s drift check need.

### 4. `cli` ↔ `materialize`

- **Thin edge:** `src/materialize/status_data.rs:17` imports `StatusData`
  from `crate::cli::statusline::data`, and calls `crate::cli::stale_status`/
  `StaleStatus` (`src/materialize/status_data.rs:336-339`).
- **Heavy edge (kept, one-way):** `cli` drives materialize's whole
  render/cache engine — `CacheManifest`, `materialize_with_mode`,
  `collect_status_data`, `state::*`, `inherit::*`, `merge_cache::write`,
  `cache::*` (dozens of call sites in `src/cli/mod.rs` and
  `src/cli/doctor.rs`).
- **Fix:** move `StatusData` (currently `cli::statusline::data::StatusData`)
  into `materialize::status_data` — materialize is the module that computes
  and writes it; `cli` only reads it back for rendering. Move `StaleStatus`,
  `stale_status()`, and `run_check_stale()` (cycle 3) into `materialize` as
  well; `cli`'s statusline/doctor code and `hook_run`'s drift check both
  call into `materialize` afterward, one-way.

### 5. `materialize` ↔ `merge`

- **Thin edge:** `src/merge/mod.rs:202` (`merge_signature`) calls
  `crate::materialize::cache::update_len_prefixed` — a pure SHA-256
  length-prefix helper (`src/materialize/cache.rs:197`) that `materialize`
  also uses internally, 15+ call sites.
- **Heavy edge (kept, one-way):** `materialize` consumes merge's output
  types — `MergedManifest`, `rules::RuleFile`.
- **Fix:** move `update_len_prefixed` into `llmenv-util` (already
  zero-fan-out). Both `materialize` and `merge` call
  `llmenv_util::update_len_prefixed` afterward.

### 6. `consolidation` ↔ `hook_run`

- **Thin edge:** `consolidation/mod.rs:29` imports
  `hook_run::mcp_client::McpHttpClient`. `src/hook_run/mcp_client.rs` (1143
  lines) has zero internal `crate::` dependencies today — only
  `std`/`anyhow`/`serde_json`/`url` — so it is already portable as-is.
- **Heavy edge (kept, one-way):** `hook_run::detached_consolidation` drives
  `consolidation`'s work directly.
- **Fix:** move the whole `mcp_client` module into `llmenv-mcp`.
  `consolidation` depends on `llmenv_mcp::McpHttpClient` afterward instead of
  reaching into `hook_run`.

### 7. `hook_run` ↔ `session_log`

- **Thin edge:** `session_log` needs four things from `hook_run`:
  - `action::{bundle_keyword, tag_keyword}` (`src/session_log/scope_header.rs:6`)
    — pure string formatters over a prefix constant.
  - `mcp_client::McpHttpClient` (`src/session_log/detached.rs:17`,
    `dispatch.rs:6`) — already relocating per cycle 6.
  - `redirect_stderr_to_detached_log`, `detached_child_log_path`
    (`src/session_log/detached.rs:63-65`) — both only touch
    `paths::state_dir()` and `std::process::Command`, no other `hook_run`
    dependency.
  - `memory_url` (`src/session_log/detached.rs:113`) — builds the memory-MCP
    endpoint from config + `ActiveScopes`; internally calls
    `cli::firing_bundles` today (moving to `bundle_select` per cycle 3).
- **Heavy edge (kept, one-way):** `hook_run` drives session logging —
  `dispatch`, `event::*`, `ScopeContext`, `scope_header_content`,
  `scope_metadata_json`, `state`, `default_file_path`, `FileSink`,
  `detached::spawn_record`, `transcript::*`.
- **Fix:**
  - `bundle_keyword`/`tag_keyword` and their prefix constants move into
    `llmenv-scope` — bundle/tag are scope-selector concepts, and `scope` is
    already the base-layer crate every other module here depends on.
  - `McpHttpClient` → `llmenv-mcp` (cycle 6, shared fix).
  - `redirect_stderr_to_detached_log`/`detached_child_log_path` move into
    `session_log` itself — it is the sole real consumer and the natural
    owner of "detached logging" as a concept. `hook_run`'s own use of them,
    if any remains, calls `session_log::*` afterward (already a one-way
    dependency).
  - `memory_url` moves into `memory`. `memory` already depends on
    `hook_run::memory_url` and `hook_run::mcp_client::McpHttpClient` one-way
    (`src/memory/mod.rs:14,31`, `src/memory/prune.rs:25,88`) — `hook_run`
    never depends back on `memory`. Moving `memory_url` there removes a
    needless hop through `hook_run` for every caller (`session_log`,
    `consolidation`, `memory` itself) rather than creating a new
    dependency direction. `memory_url` internally calls
    `bundle_select::firing_bundles` afterward instead of `cli::firing_bundles`.

## New / changed module surface

| Module | Change |
| --- | --- |
| `crate::bundle_select` (new) | `firing_bundles`, `build_bundle_refs`, `marker_enabled_bundle_names`, `marker_disabled_bundle_names`, `tag_or_marker_selected`, moved from `cli/mod.rs` |
| `llmenv-util` | Gains `update_len_prefixed`, `should_use_color` |
| `llmenv-mcp` | Gains the `mcp_client` module (`McpHttpClient`) and `INDEX_REPOSITORY_TOOL` |
| `llmenv-scope` | Gains `bundle_keyword`, `tag_keyword`, and their prefix constants |
| `materialize` | Gains `StatusData` (from `cli::statusline::data`), `StaleStatus`, `stale_status()`, `run_check_stale()` |
| `memory` | Gains `memory_url()` (from `hook_run`) |
| `session_log` | Gains `redirect_stderr_to_detached_log()`, `detached_child_log_path()` (from `hook_run`) |
| `adapter` | Unchanged — its `create_dir_owner_only` wrapper stays for its own callers |

After these moves, `adapter`, `cli`, `hook_run`, `materialize`, `merge`,
`consolidation`, and `session_log` each have a one-directional dependency
graph with no cycles. None of the heavy, legitimate dependency directions
change.

## Sequencing

Independent moves; grouped here for review-sized commits/PRs:

1. `update_len_prefixed` → `llmenv-util` (resolves cycle 5)
2. `should_use_color` → `llmenv-util` (part of cycle 3)
3. `mcp_client` module + `INDEX_REPOSITORY_TOOL` → `llmenv-mcp` (resolves
   cycles 1 and 6)
4. `bundle_keyword`/`tag_keyword` → `llmenv-scope` (part of cycle 7)
5. New `crate::bundle_select`, moved out of `cli` (part of cycle 3)
6. `StatusData`, `StaleStatus`, `stale_status()`, `run_check_stale()` →
   `materialize` (resolves cycles 3 and 4)
7. `memory_url()` → `memory` (part of cycle 7)
8. `redirect_stderr_to_detached_log`/`detached_child_log_path` →
   `session_log` (part of cycle 7)
9. Redirect `materialize::inherit`'s two call sites to
   `paths::create_dir_owner_only` (resolves cycle 2)

## Verification

After all moves land, re-run the audit's own method for all seven pairs:
`rg -l "crate::X" src/Y/` in both directions, cross-checked with
`cargo-modules dependencies`. Confirm zero remaining edges in the direction
each fix targeted. `cargo build`, `cargo test --workspace`, and
`cargo clippy --all-targets --all-features -- -D warnings` must stay green
through every step.

## Follow-up issues (not implemented here)

Once the cycles are broken, `adapter`, `cli`, `hook_run`, `materialize`,
`merge`, `consolidation`, and `session_log` become independently extractable
into their own workspace crates. Each extraction is filed as its own issue
after this design lands, following the pattern #1459–#1461 used for
`scope`/`mcp`/`task`.
