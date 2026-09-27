# Codebase Memory

An indexed graph of this repo's code.
Use it before an open-ended grep when the question is about structure: where X
is defined, what calls Y, how a subsystem fits together.

- `search_graph` / `search_code`: find symbols, routes and text.
  Comments and docstrings are searchable too.
- `get_file_outline`: the declarations in one file, in source order.
  Use it instead of reading a whole file to learn its shape.
- `get_code_snippet`: the source of one symbol you already found.
- `trace_path`: follow a call chain between two symbols.
- `get_architecture`: an overview of the project.
- `compare_graphs`: what changed between two indexed snapshots.
- `index_status` / `index_repository`: check or rebuild the index.

Results are compact by default.
A result says when it is truncated and gives a cursor field (such as `cursor`);
pass that value back to get the next page instead of re-running a broader
query.
A cursor stops working when the index changes; run the query again then.

After a codebase-memory upgrade, the first index of each project is a full
rebuild and can take minutes.
`index_status` showing a run in progress then is expected.
