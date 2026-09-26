# Issue #2151 — permission tiers for codebase-memory-mcp 0.11.0 tools; guard `persistence`

- **Issue:** https://github.com/phaedrus1992/llmenv/issues/2151
- **Milestone:** `v3.11.2`
- **Base branch:** `release/3.x` (forward-merges to `release/4.x`)
- **Type:** bug fix

This is a spec, not a plan.

## Problem

1. codebase-memory-mcp (cbm) 0.11.0 adds two MCP tools, `get_file_outline` and `compare_graphs`.
   llmenv's permission tier table does not list them, so Claude Code prompts on every call.
2. cbm's `index_repository` takes `persistence: true`, which writes `.codebase-memory/graph.db.zst` into the indexed repository.
   The tool is in the `Mutation` tier (allowed without a prompt), so a model can write that artifact into a repo unprompted.
   The existing `PreToolUse` guard blocks only the `name` override.

## cbm facts (source at tag `v0.11.0`, `DeusData/codebase-memory-mcp`)

`TOOL_ANNOTATIONS` in `src/mcp/mcp.c` lines 772 to 790, fields `{name, read_only, destructive, idempotent, open_world}`:

| Tool | read_only | destructive | llmenv tier |
| --- | --- | --- | --- |
| `index_repository` | false | false | Mutation (unchanged) |
| `search_graph` | true | false | ReadOnly (unchanged) |
| `query_graph` | true | false | ReadOnly (unchanged) |
| `trace_path` | true | false | ReadOnly (unchanged) |
| `get_code_snippet` | true | false | ReadOnly (unchanged) |
| `get_file_outline` | **false** | **true** | **ReadOnly (new; see below)** |
| `get_graph_schema` | true | false | ReadOnly (unchanged) |
| `compare_graphs` | true | false | **ReadOnly (new)** |
| `get_architecture` | true | false | ReadOnly (unchanged) |
| `search_code` | true | false | ReadOnly (unchanged) |
| `list_projects` | true | false | ReadOnly (unchanged) |
| `delete_project` | false | true | Destructive (unchanged) |
| `index_status` | true | false | ReadOnly (unchanged) |
| `check_index_coverage` | true | false | ReadOnly (unchanged) |
| `detect_changes` | true | false | ReadOnly (unchanged) |
| `manage_adr` | false | true | Destructive (unchanged) |
| `ingest_traces` | false | false | Mutation (unchanged) |

**Why `get_file_outline` is ReadOnly despite its annotation.**
Its handler, `handle_get_file_outline` (`src/mcp/mcp.c` line 12041), validates its arguments, calls `resolve_store()` (the query-only store path the comment above `TOOL_ANNOTATIONS` describes as strictly non-mutating), and calls `cbm_store_get_file_outline` (`src/store/store.c` line 2930), which runs two `SELECT` statements (count and page) and nothing else.
The annotation is deliberate but not reasoned: the upstream test (`tests/test_mcp.c` line 1395) says the tool "arrived after this split and keeps its upstream conservative annotation".
It came in commit `0b9df183` ("feat(mcp): add bounded file outline tool", 2026-08-28).
The existing table doc comment already justifies a tier from source for `manage_adr`; do the same here.

**`index_repository` input schema** (`src/mcp/mcp.c` lines 466 to 480): `repo_path` (string), `mode` (`full`, `moderate`, `fast`, `cross-repo-intelligence`), `target_projects` (array), `name` (string, override), `persistence` (boolean, default `false`, "Write .codebase-memory/graph.db.zst").
`persistence` also exists in 0.10.8; it is not new.

`manage_adr` gained `mode: "set_sections"` in 0.11.0; it is still annotated destructive and keeps no history. Its tier does not change.

## Verified locations (release/3.x)

| Fact | Location |
| --- | --- |
| `CBM_READ_ONLY`, `CBM_MUTATION`, `CBM_DESTRUCTIVE` | `src/adapter/claude_code.rs` lines 216 to 232, doc comment above them |
| Tiers become rules through `apply_mcp_tier_permissions` (ReadOnly and Mutation allow, Destructive ask) | `src/adapter/claude_code.rs` near line 1704; tests near lines 3773 to 3948 |
| Guard: `handle_pre_tool_use` denies `index_repository` with a non-empty `name` using a `__DENY__:` reason | `src/hook_run/cbm_index_guard.rs` line 35 |
| Guard is wired as a `PreToolUse` hook with matcher `^mcp__codebase-memory-mcp__index_repository$` for Claude Code, and through the opencode shim | `src/adapter/claude_code.rs`, `src/adapter/opencode.rs` (`index_repository_guard_hook_*` tests) |
| llmenv's own auto-index passes only `repo_path` | `build_index_repository_command`, `src/hook_run/mod.rs` |

## Changes

### Tier table

1. Add `get_file_outline` and `compare_graphs` to `CBM_READ_ONLY`, in the upstream table's order.
2. Extend the doc comment: name cbm `v0.11.0` as the version the table is checked against; one sentence for why `get_file_outline` is ReadOnly (query-only store path, two `SELECT`s, upstream annotation is a leftover of a scoping split); one sentence that `manage_adr` `set_sections` does not change its tier.

### Guard

Extend `handle_pre_tool_use`: after the `name` check, deny when `tool_input.persistence` is JSON `true`.
Reason text:
`__DENY__:llmenv blocked index_repository with persistence=true. It writes .codebase-memory/graph.db.zst into the repository. Call it without persistence. To share a graph artifact on purpose, run codebase-memory-mcp from a shell.`

Rules:

- `persistence` absent, `null`, `false`, or not a boolean: allow (the tool treats non-boolean as invalid input itself).
- Both `name` and `persistence: true`: return the `name` reason (checked first, unchanged).

Update the module doc comment to name both checks.

### Drift test

Add a test constant with the 17 tool names from cbm `v0.11.0`, and a test that asserts `CBM_READ_ONLY ∪ CBM_MUTATION ∪ CBM_DESTRUCTIVE` equals that set exactly, with no tool in two tiers.
The failure message says: `codebase-memory-mcp tool list changed: update the tier table in src/adapter/claude_code.rs and this list; see docs/design/issue-2151-cbm-011-tool-tiers.md`.

## Tests

1. Tier rendering: `get_file_outline` and `compare_graphs` land in `permissions.allow` with the `mcp__codebase-memory-mcp__` prefix.
2. Guard table: `persistence: true` denies; `false`, absent, `null`, `"true"` (string) allow; `name` plus `persistence` gives the `name` reason.
3. Drift test as above.

## Acceptance criteria

1. With cbm 0.11.0, a Claude Code session calls `get_file_outline` and `compare_graphs` without a prompt.
2. A model call to `index_repository` with `persistence: true` is denied with the reason text.
3. Changelog `Fixed`: codebase-memory 0.11.0's new read-only tools no longer prompt; `index_repository` can no longer write a graph artifact into the repo unprompted.
4. The permissions or codebase-memory docs page lists the two new tools and the `persistence` guard, tagged `(changed in v3.11.2)`.

## Out of scope

- Reporting the annotation upstream. The maintainer decides whether to file it on `DeusData/codebase-memory-mcp`.
