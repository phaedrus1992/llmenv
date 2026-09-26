# Issue #2152 — update the codebase-memory skill reference for cbm 0.11.0

- **Issue:** https://github.com/phaedrus1992/llmenv/issues/2152
- **Milestone:** `v3.11.2`
- **Base branch:** `release/3.x` (forward-merges to `release/4.x`)
- **Type:** documentation (model-facing text and user docs)

This is a spec, not a plan.

## Problem

`skills/llmenv/references/codebase-memory.md` (15 lines on `release/3.x`) is the only guidance the model gets on the codebase-memory (cbm) tools.
It predates cbm 0.11.0 and misses:

- the new tools `get_file_outline` and `compare_graphs`;
- compact output by default (`format: "tree"`) and paging with cursors;
- the one-time full reindex after an upgrade from 0.10.8 or earlier.

`llmenv_skill.rs` embeds this file with `include_str!` and ships it only when `features.codebase_memory` has entries.
The model reads it as context, so it must stay short.

## cbm facts (source at tag `v0.11.0`)

| Fact | Source |
| --- | --- |
| `get_file_outline`: declaration outline of one exact repository-relative file; optional label filter; source order; `limit` 1 to 200 (default 100) and `offset` | `src/mcp/mcp.c` line 582 |
| `compare_graphs`: compares two indexed snapshots; lists additions and removals of stable node and edge identities, with exact totals and truncation reasons | `src/mcp/mcp.c` line 604 |
| Most query tools take `format` (`tree` default, or `json`) and `max_output_tokens` (default 3200, minimum 128) | tool schemas in `src/mcp/mcp.c` |
| Continuation fields are named per tool: `cursor`, `module_cursor`, `impact_cursor`, `changed_cursor` | same |
| Cursors are bound to the query arguments, result state and index generation; a stale cursor fails instead of paging a different snapshot | v0.11.0 release notes, "Lean, lossless output" |
| Prose (docstrings, comments) is indexed for BM25 search | v0.11.0 release notes, "Extraction and coverage improvements" |
| The first index after an upgrade from 0.10.8 or earlier is a full rebuild; ADRs are kept | v0.11.0 release notes, "Highlights" |

## Change 1: replace the reference file

Replace the whole body of `skills/llmenv/references/codebase-memory.md` with this text.
Keep the file's title line.

```markdown
# Codebase Memory

An indexed graph of this repo's code. Use it before an open-ended grep when the question is about structure: where X is defined, what calls Y, how a subsystem fits together.

- `search_graph` / `search_code`: find symbols, routes and text. Comments and docstrings are searchable too.
- `get_file_outline`: the declarations in one file, in source order. Use it instead of reading a whole file to learn its shape.
- `get_code_snippet`: the source of one symbol you already found.
- `trace_path`: follow a call chain between two symbols.
- `get_architecture`: an overview of the project.
- `compare_graphs`: what changed between two indexed snapshots.
- `index_status` / `index_repository`: check or rebuild the index.

Results are compact by default. A result says when it is truncated and gives a cursor field (such as `cursor`); pass that value back to get the next page instead of re-running a broader query. A cursor stops working when the index changes; run the query again then.

After a codebase-memory upgrade, the first index of each project is a full rebuild and can take minutes. `index_status` showing a run in progress then is expected.
```

This replaces the text; it must not grow past about 20 lines.
Do not list parameters beyond the cursor rule; the tool schemas carry them.

## Change 2: user docs

In `website/docs/mcp.md`, section `Codebase memory (codebase_memory:)` (line 192 on `release/3.x`), add a short paragraph tagged `(changed in v3.11.2)`:

- llmenv's model guidance covers codebase-memory-mcp 0.11.0 tools, including `get_file_outline` and `compare_graphs`.
- After upgrading codebase-memory-mcp from 0.10.8 or earlier, the first session in each project rebuilds that project's index once (index format change); llmenv starts that index at session start, so large repositories are slow once.

## Tests

The existing test in `src/adapter/llmenv_skill.rs` that checks the reference files ship with the feature stays green.
Add no test on the wording.

## Acceptance criteria

1. The reference file matches the text above, apart from wrapping.
2. `website/docs/mcp.md` has the paragraph with the version tag.
3. Changelog `Changed`: the codebase-memory guidance now covers cbm 0.11.0 tools and paging.

## Out of scope

- Guidance for `manage_adr`, `ingest_traces`, `delete_project`, `query_graph`, `get_graph_schema`, `check_index_coverage`, `detect_changes`, `list_projects`. The model finds them through the tool list; the reference covers the common path only.
