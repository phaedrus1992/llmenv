# Codebase Memory

An indexed knowledge graph of this repo's code structure — use it instead of
grepping blind when you need architecture-level answers.

- `search_graph` / `search_code` — find functions, routes, symbols by meaning,
  not just text match.
- `trace_path` — follow a call chain from one symbol to another.
- `get_architecture` — a structural overview of the indexed project.
- `index_status` / `index_repository` — check or (re)build the index if it
  looks stale or missing.

Prefer this over an open-ended `grep`/`find` sweep when the question is "where
does X connect to Y" or "what's the shape of this subsystem" rather than "find
this exact string."
