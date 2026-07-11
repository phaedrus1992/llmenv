# Issue #365 — integrate codebase-memory-mcp as an llmenv feature

- **Issue:** https://github.com/phaedrus1992/llmenv/issues/365
- **Milestone:** Large Projects
- **Type:** Feature (large)
- **Difficulty:** Hard. Mirrors the ICM integration pattern, but requires
  upstream research and touches schema, materialize, hooks, and doctor.

## Problem

[codebase-memory-mcp](https://github.com/containers/codebase-memory-mcp)
(graph-based codebase indexing/retrieval MCP server) can only be wired
into llmenv as a raw `mcp:` entry today — no lifecycle management, health
checks, or tag-based activation. It should be a first-class feature like
the ICM memory backend.

## Decisions (so the implementer doesn't have to make them)

The issue's four "Considerations", resolved:

1. **Coexistence with ICM:** fully orthogonal. Both can be active; llmenv
   does **not** coordinate them (no cross-backend dedup/routing — YAGNI).
   They are separate feature keys with separate lifecycles.
2. **Index storage:** under llmenv's stable state dir (the same
   hash-independent state relocation mechanism `Config.state`/
   `StateConfig` exists for — see the `state` field docs in
   `crates/llmenv-config/src/schema.rs`), namespaced per project. The
   server itself owns the DB format; llmenv only hands it a data
   directory.
3. **Activation:** tag-based, same as everything else in llmenv (`mcp`,
   `lsp`, bundles all select by tag intersection). No special repo-root
   auto-detection — a project scope's tags already express "this repo
   wants codebase memory". Consistency beats magic.
4. **Transport:** the same topology model as `features.memory` (ICM):
   host/client entries, `Config.host` directory for cross-machine
   addressing, and the existing mcp-proxy pattern where applicable. Do not
   invent a second lifecycle mechanism.

## Config shape

Mirror the ICM `Memory` entry shape *exactly where the concepts overlap*
(read the `Memory` struct and its docs in
`crates/llmenv-config/src/schema.rs` first — tags, role/topology, host
reference), adding only the knobs the issue names:

```yaml
features:
  codebase_memory:
    - tags: [my-project]
      host: workstation        # key into top-level `host:` directory (remote)
      # or local server management:
      index_path: null          # default: <state_dir>/codebase-memory/<project>
      embedding_model: null     # passthrough to the server; server's default if unset
      port: null                # default chosen like ICM's server port handling
```

`Features.codebase_memory: Vec<CodebaseMemory>` with `#[serde(default)]`
(empty = feature off), validation consistent with `Memory`'s (duplicate
tag targeting, unknown host name, port range).

## Phased implementation (each phase lands green)

### Phase 0 — upstream research (gate for everything)

Clone/read codebase-memory-mcp's README and CLI: exact launch command,
transport (stdio vs HTTP), data-dir flag, embedding-model flag, health
endpoint, tool names. **Do not guess** — record findings in a comment
block at the top of the new module. (The session tool surface shows tools
like `index_repository`, `index_status`, `search_code`, `query_graph` —
verify against the pinned upstream version.)

### Phase 1 — schema + validation

`CodebaseMemory` struct, `Features` field, round-trip + validation tests
(mirror the `Memory` tests in the same file).

### Phase 2 — materialization

Render the server into each selected engine's MCP config the same way the
ICM backend is rendered (find where `features.memory` becomes an MCP
entry — `src/materialize/state.rs` and the adapter MCP rendering paths —
and add the sibling). Tag intersection decides which scopes get it.
Snapshot/adapter tests per engine.

### Phase 3 — lifecycle + doctor

- Auto-start/stop with the session where llmenv manages ICM's lifecycle
  today (follow ICM's start path; if ICM is launch-on-demand via the
  engine's MCP client, then "lifecycle" means correct command rendering
  and nothing more — match reality, don't add a supervisor).
- Health check + `llmenv doctor` entry: server reachable, index dir
  writable, version probe (mirror ICM's doctor checks in
  `src/cli/doctor.rs`).
- `llmenv status`: show whether codebase memory is active for the current
  scope and where its index lives.

### Phase 4 — docs + changelog

Document `features.codebase_memory` alongside `features.memory`;
CHANGELOG `[Unreleased]` entry via the keepachangelog skill.

## Constraints

- Core feature: all code in `src/` + `crates/llmenv-config/` — never
  `examples/` (`AGENTS.md`).
- No new Rust dependencies expected (the server is an external binary the
  user installs; doctor should report if it's missing, with the install
  URL from the issue).
- Functions ≤100 lines; mock the MCP/process boundary in tests.

## Acceptance criteria

- [ ] Declaring one `features.codebase_memory` entry with a matching tag
      materializes a working MCP server entry for claude-code (and any
      other engine whose adapter renders `features.memory` today).
- [ ] Non-matching tags → nothing rendered.
- [ ] Doctor: missing binary, unreachable server, and unwritable index dir
      each produce a distinct, actionable warning.
- [ ] ICM and codebase-memory active simultaneously in one config works.
- [ ] Upstream flags/tool names verified against a pinned upstream version
      and recorded in-module.
- [ ] Docs + CHANGELOG entry; clippy/fmt clean; full suite green.

## Out of scope

- Indexing orchestration (deciding when to index/reindex — the server and
  the agent own that via its own MCP tools).
- Cross-backend memory coordination with ICM.
- Embedding-model management beyond passing the configured value through.
