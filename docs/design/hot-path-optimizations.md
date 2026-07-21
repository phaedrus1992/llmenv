# Hot-Path Performance Optimizations

Four optimizations to the llmenv hook-run hot path, implemented as a single
change set following the plan in `linear-stirring-storm.md` (Items V, 2, 3, 4).

Every Claude Code tool call triggers llmenv hooks. At ~3-10 prompts per coding
minute, 5 lifecycle events (`TurnStart`, `SessionStart`, `SessionEnd`,
`PostToolUse`, `PostSession`) run the full pipeline on every agent turn —
hundreds of times per session. Each millisecond saved compounds across every
event.

---

## Item V: Version Logging

**Problem:** Session-scoped diagnostics needed the exact Claude Code version to
correlate hook behavior across agent versions. Without it, a behavioral change
between versions that subtly alters context quality or hook timing is invisible
in the session log — you'd notice a regression but couldn't attribute it.

**What changed:** `claude_code_version` is threaded through the hook pipeline:

1. `run()` reads `CLAUDE_CODE_VERSION` from env (set by the Claude Code adapter
   at the top of every session)
2. `run_inner()` accepts it as the 5th parameter
3. `build_scope_context()` accepts it as the 6th parameter and stores it in
   `ScopeContext`
4. `ScopeContext` gained a `claude_code_version: String` field
5. `scope_metadata_json()` serializes it alongside the other scope fields into
   the session log's metadata block
6. `scope_header_content()` includes it in the FTS-searchable session header

**Why this way:** The env var is set once per session by the adapter
(`src/adapter/claude_code.rs`) when it emits `CLAUDE_CODE_VERSION=<semver>` in
the hook-run environment. Reading it at the `run()` boundary keeps the
extraction in one place rather than plumbing it through every adapter.

**Files modified:**

- `src/hook_run/mod.rs` — reads env var, passes through to run_inner and
  build_scope_context
- `src/session_log/scope_header.rs` — adds field to ScopeContext, serializes it
  in both content and metadata paths

---

## Item 2: Bundle Merge Cache

**Problem:** `memory_url()` calls `merge::merge()` — a 731-line function that
reads firing bundle YAML files from disk and walks the merged capability tree
to produce a `MergedManifest`. This runs on every non-early-exit event. The
firing bundle set (determined by active tags) only changes on session boundary
or config edit — not between events within a session.

**What changed:** A `MergeCacheEntry` caches the merge result keyed by
`(config_mtime, firing_bundle_set_hash)`:

```rust
struct MergeCacheEntry {
    key: u64,
    bundle_memory: Vec<Memory>,
    bundle_host: BTreeMap<String, HostEntry>,
}
```

Only the `memory` and `host` entries from the merged result are cached — those
are the only fields `memory_url()` needs for URL resolution. The full
`MergedManifest` is still produced on cache miss.

The cache key combines the config file mtime (from `metadata().modified()`) with
a hash of the firing bundle names. Any config edit or bundle membership change
invalidates the cache.

The cache itself is a `Mutex<Option<MergeCacheEntry>>` — no `OnceLock` needed
since it's write-on-read (written on miss, read on every call). The `Mutex`
serializes concurrent access, but since the hook pipeline is single-threaded
per process (one hook-run invocation at a time, short-lived CLI process), this
never contends.

**Trade-offs:**

- **Why `memory` and `host` only, not the full `MergedManifest`?** The cache is
  consumed exclusively by `memory_url()`, which only needs those two fields.
  Storing the full manifest would cache unused data (capabilities, feature
  flags, etc.) that consumes memory for zero benefit.
- **Why not move merge outside the hot path entirely?** The merge result is
  theoretically constant for the lifetime of a session, but the hook-run binary
  is a short-lived process — it starts, handles one event, and exits. There's
  no in-process persistence across events within a single hook-run. Moving
  merge to session boundaries would require either a background daemon or
  a persistent process, which is a much larger change.

**Files modified:**

- `src/hook_run/mod.rs` — added `MergeCacheEntry` struct, `merge_cache_key()`
  function, and cache check inside `memory_url()`

---

## Item 3: Tokio Runtime Reuse

**Problem:** Each event built a fresh Tokio runtime:

```rust
tokio::runtime::Builder::new_current_thread()
    .enable_all()
    .build()?
```

This allocates I/O driver state, timer wheels, and thread-local storage — then
drops it all when the event finishes. The hook only needs `block_on` for a
single sequential HTTP call to the ICM MCP backend.

**What changed:** A `OnceLock<io::Result<Runtime>>` replaces the per-event build,
avoiding `.expect()` (banned by workspace lints):

```rust
static RUNTIME: OnceLock<io::Result<tokio::runtime::Runtime>> = OnceLock::new();
let runtime = match RUNTIME.get_or_init(|| {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
}) {
    Ok(rt) => rt,
    Err(e) => return Err(anyhow::anyhow!("failed to build tokio runtime: {e}")),
};
```

`OnceLock::get_or_init()` guarantees exactly one build across all events within
the process lifetime. The runtime persists until the process exits.

**Why `OnceLock` and not `Mutex<Option<Runtime>>`:** `OnceLock` is lock-free
after initialization (atomic load on the fast path). A `Mutex` would serialize
every access even though initialization is idempotent. `OnceLock` also allows
const construction, avoiding the `Mutex::new(non_const_fn())` problem.

**Why `current_thread`:** The hook only needs `block_on` — never spawns tasks,
never needs work-stealing. A multi-thread runtime would be strictly worse
(heavier initialization, thread wakeup latency).

**Note on `Send`:** `block_on` on a `current_thread` runtime doesn't require
`Future: Send`, which matters for `McpHttpClient`'s internal types.

**Files modified:**

- `src/hook_run/mod.rs` — replaced per-event `Builder::build()` with
  `OnceLock<Runtime>`

---

## Item 4: MCP HTTP Client Reuse

**Problem:** `McpHttpClient::new(url, HOOK_TIMEOUT)` built a fresh
`reqwest::Client` per event. Each Client allocates a connection pool, TLS
session cache, DNS resolver, and HTTP settings — then gets dropped. The memory
backend URL is constant across events within a session.

**What changed:** A `OnceLock<Mutex<HashMap<String, McpHttpClient>>>` caches
clients keyed by URL:

```rust
static MCP_CLIENT_CACHE: OnceLock<Mutex<HashMap<String, McpHttpClient>>> =
    OnceLock::new();
```

On every event:

1. Look up the resolved memory URL in the cache
2. Cache hit → clone the cached client (cheap — `reqwest::Client` is internally
   `Arc<Inner>`, and the MCP session ID is shared via `Arc`)
3. Cache miss → create the client, insert into cache, return clone

**Why `HashMap<String, McpHttpClient>` and not a single client:** The memory
backend URL can change when the scope changes (different tags → different
memory backend). In practice this doesn't change mid-session, but the HashMap
handles it correctly without a cache-eviction edge case.

**Why `OnceLock<Mutex<HashMap<...>>>` and not `Mutex<HashMap<...>>`:**
`HashMap::new()` isn't const, so `Mutex::new(HashMap::new())` in a static
initializer is a compile error. `OnceLock` wraps the construction:
`OnceLock::get_or_init(|| Mutex::new(HashMap::new()))` — the init closure runs
once, and thereafter the `OnceLock` provides lock-free access to the inner
`Mutex`.

**Why the `serde_json::Value` -> `HostEntry` fix:** The initial implementation
used `BTreeMap<String, serde_json::Value>` for `MergeCacheEntry.bundle_host`,
but `MergedManifest.capabilities.host` is `BTreeMap<String, HostEntry>`.
`HostEntry` is defined at `crates/llmenv-config/src/schema.rs:767` and derives
`Clone` — so the cache directly stores the typed value, no intermediate
serialization needed.

**Fail-soft handling:** When `McpHttpClient::new` fails (e.g., unresolvable
hostname), the cache stores nothing for that URL. The cache-miss path logs a
warning to stderr and continues without a client — the dispatch code[^1]
handles `None` by producing a `DispatchResult::Skipped` event. This preserves
llmenv's fail-soft contract (exit 0, warning on stderr).

[^1]: `src/hook_run/dispatch.rs` — the dispatch path destructures the client
      as `Some(client)` for every action type; `None` is handled by producing
      a no-op `DispatchResult::Skipped` which exits normally.

**Files modified:**

- `src/hook_run/mod.rs` — replaced per-event `McpHttpClient::new()` with
  `OnceLock<Mutex<HashMap<...>>>` cache

---

## Verification

All existing tests pass:

- `cargo test --all-features` — 1126 passed, 3 ignored, 0 failed
- `cargo clippy --all-targets --all-features` — no issues
- `cargo test --test hook_run_failsoft` — 12 passed (all fail-soft contract
  tests, including the `malformed_backend_url` edge case that required the
  `.expect()` -> `eprintln!` fix above)

The fail-soft test `malformed_backend_url_exits_zero_with_warning` caught a
regression: the original `.expect("invalid memory backend URL")` panicked
(exit 101) instead of emitting a warning and continuing (exit 0 with stderr).
The fix replaces `expect` with an `eprintln!` warning path that returns `None`
for the client.
