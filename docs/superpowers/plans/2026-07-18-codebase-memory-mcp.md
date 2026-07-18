# Codebase-Memory-MCP Integration Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add `features.codebase_memory` as a first-class llmenv feature that
materializes [DeusData/codebase-memory-mcp](https://github.com/DeusData/codebase-memory-mcp)
(a local, tag-activated, stdio MCP server) into each engine's MCP config, with
lifecycle env vars, doctor/status checks, and a SessionStart hook that
registers the project for the server's own background auto-reindex watcher.

**Architecture:** `CodebaseMemory` is a new, minimal config struct (tags +
optional index-path override) that resolves like a **local stdio MCP entry**
(`ResolvedKind::Stdio`), not like ICM's always-remote `Memory` topology —
codebase-memory-mcp has no network/remote-serve mode; its own cross-machine
story is a git-committed graph artifact, not a live daemon. `CBM_CACHE_DIR`
and `CBM_ALLOWED_ROOT` env vars are always computed by llmenv (state dir +
project root), never user-configurable, so a declared entry can't accidentally
scope the indexer outside the intended project. Freshness after the initial
index is handled entirely by upstream's own background watcher
(`auto_watch`, default `true`) — llmenv's only active job is a fire-and-forget
`codebase-memory-mcp cli index_repository` subprocess call on `SessionStart`
for the active project, mirroring the existing `post_session_consolidation()`
detached-spawn pattern in `src/hook_run/mod.rs`.

**Tech Stack:** Rust (llmenv core + `llmenv-config` crate), serde, proptest,
`assert_cmd` for CLI integration tests. No new dependencies.

## Global Constraints

- Core feature code only in `src/` + `crates/llmenv-config/` — never
  `examples/` (AGENTS.md).
- No new Rust dependencies.
- Functions ≤100 lines, complexity ≤8, ≤5 positional params (project
  CLAUDE.md).
- Mock the process/MCP boundary in tests — never shell out to the real
  `codebase-memory-mcp` binary (it may not be installed in CI).
- Full unit + integration coverage is a hard, explicit requirement for this
  feature — every task below ends with tests, not just the acceptance
  criteria minimum.
- `cargo fmt` after every Rust file edit, before staging.

## Upstream facts this plan relies on (verified against DeusData/codebase-memory-mcp v0.9.0)

- MCP transport is **stdio only** (JSON-RPC 2.0). No native remote/HTTP mode
  for the protocol itself (the `--ui --port=9749` flag is an unrelated
  optional graph-visualization web UI).
- 14 MCP tools, confirmed names: `index_repository`, `list_projects`,
  `delete_project`, `index_status`, `search_graph`, `trace_path` (alias
  `trace_call_path`), `detect_changes`, `query_graph`, `get_graph_schema`,
  `get_code_snippet`, `get_architecture`, `search_code`, `manage_adr`,
  `ingest_traces`.
- **CLI mode** — every MCP tool is also invocable as a subprocess:
  `codebase-memory-mcp cli <tool> '<json-args>'`. This is what the
  SessionStart hook uses (llmenv's hook-run is itself a short-lived process,
  not a persistent MCP client — CLI mode is the correct fit, no new
  stdio-JSON-RPC client needed).
- Env vars: `CBM_CACHE_DIR` (index storage dir, default
  `~/.cache/codebase-memory-mcp/`), `CBM_ALLOWED_ROOT` (restricts
  `index_repository` to paths within a directory — **security-relevant,
  llmenv must always set this**), `CBM_LOG_LEVEL`, `CBM_WORKERS`. No
  `embedding_model` or `port` knob exists for the MCP server itself.
- Background watcher (`auto_watch`, default `true`) automatically re-indexes
  on git changes once a project has been registered via one
  `index_repository` call — this is why llmenv doesn't need a
  reindex-on-every-tool-use hook, just a register-once-per-session call.

---

## Task 1: `CodebaseMemory` schema struct + `Features` field

**Files:**
- Modify: `crates/llmenv-config/src/schema.rs:20-54` (`Features` struct, both
  the top-level and the `Capabilities` copy at `schema.rs:543-549`)
- Modify: `crates/llmenv-config/src/schema.rs:570-605`
  (`Capabilities::is_empty()`)
- Modify: `crates/llmenv-config/src/schema.rs` (new struct, placed directly
  after `Memory` at line ~1130)
- Test: same file, `#[cfg(test)] mod tests` block (schema.rs already has one
  — search for `mod tests` in the file and add alongside existing
  `Memory`/`Features` round-trip tests)

**Interfaces:**
- Produces: `pub struct CodebaseMemory { pub when: Vec<String>, pub index_path: Option<String> }`,
  field `Features.codebase_memory: Vec<CodebaseMemory>`
- Consumed by: Task 2 (validation), Task 4 (resolver), Task 5 (doctor), Task 6
  (status)

- [ ] **Step 1: Write the failing round-trip test**

Add to `crates/llmenv-config/src/schema.rs`'s existing test module:

```rust
#[test]
fn codebase_memory_round_trips_through_yaml() {
    let yaml = r#"
when: [my-project]
index_path: /custom/index/path
"#;
    let cm: CodebaseMemory = serde_yaml::from_str(yaml).unwrap();
    assert_eq!(cm.when, vec!["my-project".to_string()]);
    assert_eq!(cm.index_path.as_deref(), Some("/custom/index/path"));

    let cm_defaults: CodebaseMemory = serde_yaml::from_str("when: [x]").unwrap();
    assert_eq!(cm_defaults.index_path, None);
}

#[test]
fn features_codebase_memory_defaults_to_empty() {
    let features: Features = serde_yaml::from_str("{}").unwrap();
    assert!(features.codebase_memory.is_empty());
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p llmenv-config codebase_memory_round_trips_through_yaml -- --nocapture`
Expected: FAIL with "cannot find type `CodebaseMemory` in this scope" (compile error)

- [ ] **Step 3: Add the struct and field**

Insert directly after the `Memory` struct (after line ~1130 in
`crates/llmenv-config/src/schema.rs`):

```rust
/// A local, tag-activated `codebase-memory-mcp` server. Unlike `Memory`
/// (ICM), this resolves to a **local stdio** MCP entry, not a network
/// client — codebase-memory-mcp has no remote-serve mode. `CBM_CACHE_DIR`
/// and `CBM_ALLOWED_ROOT` are always computed by llmenv (state dir +
/// project root), never user-configurable, so a declared entry can't
/// accidentally scope the indexer outside the intended project.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct CodebaseMemory {
    /// Tags that activate this server, intersected with active scope tags
    /// (same selection model as `mcp`/`memory`).
    #[serde(default)]
    pub when: Vec<String>,
    /// Override the index storage directory (`CBM_CACHE_DIR` env var).
    /// Defaults to `<state_dir>/codebase-memory/<project>` when unset.
    #[serde(default)]
    pub index_path: Option<String>,
}
```

Then update `Features` (schema.rs:20-54) — add after the `task_tracker` field:

```rust
    #[serde(default)]
    pub codebase_memory: Vec<CodebaseMemory>,
```

Do the same for the second `Features` occurrence embedded in `Capabilities`
(schema.rs:543-549 — it's the same `Features` type, so this is automatic once
the type itself has the field; no separate edit needed there beyond what the
type change already provides).

Update `Capabilities::is_empty()` (schema.rs:570-605) — add to the chain:

```rust
    && self.features.as_ref().is_none_or(|f| f.codebase_memory.is_empty())
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p llmenv-config codebase_memory`
Expected: PASS (2 tests)

- [ ] **Step 5: Commit**

```bash
cargo fmt -p llmenv-config
git add crates/llmenv-config/src/schema.rs
git commit -m "feat(config): add CodebaseMemory schema struct and Features field"
```

---

## Task 2: Validation

**Files:**
- Modify: `crates/llmenv-config/src/validate.rs:557-644` (`validate_mcps`,
  add a codebase_memory block alongside the existing `memory`/`throttle`
  loops)
- Modify: `crates/llmenv-config/src/validate.rs:47-54` (`ValidateError` enum
  — add new variant)
- Modify: `src/merge/mod.rs:230-261` (`read_bundle_yaml` — duplicate
  bundle-side check, matching how `features.memory`/`features.throttle` are
  double-validated there since `Config::validate()` never sees bundle
  capabilities)
- Test: `crates/llmenv-config/src/validate.rs` (new `#[test]` functions —
  this task ALSO fixes a coverage gap found during upstream research: the
  three existing `Memory`-specific `ValidateError` variants
  (`MemoryNoTags`, `MemoryUnknownServerHost`, `MemoryInvalidListenHost`) have
  **no dedicated unit tests today** — only proptest round-trip coverage.
  Add missing tests for both `Memory` and the new `CodebaseMemory` in the
  same task so the gap doesn't propagate.)

**Interfaces:**
- Consumes: `CodebaseMemory` from Task 1
- Produces: `ValidateError::CodebaseMemoryNoTags` variant; validation runs
  inside `Config::validate()` (called via `validate_mcps`)

- [ ] **Step 1: Write the failing tests**

Add to `crates/llmenv-config/src/validate.rs`'s test module:

```rust
#[test]
fn codebase_memory_requires_tags() {
    let mut config = minimal_valid_config(); // existing test helper in this file
    config.features = Some(Features {
        codebase_memory: vec![CodebaseMemory { when: vec![], index_path: None }],
        ..Default::default()
    });
    let err = config.validate().unwrap_err();
    assert!(matches!(err, ValidateError::CodebaseMemoryNoTags));
}

#[test]
fn codebase_memory_with_tags_is_valid() {
    let mut config = minimal_valid_config();
    config.features = Some(Features {
        codebase_memory: vec![CodebaseMemory {
            when: vec!["my-project".to_string()],
            index_path: None,
        }],
        ..Default::default()
    });
    assert!(config.validate().is_ok());
}

// Coverage gap found during #365 research: Memory's own validate errors had
// no dedicated unit tests, only proptest round-trips. Fill it here.
#[test]
fn memory_requires_tags() {
    let mut config = minimal_valid_config();
    config.host.insert("h".to_string(), HostEntry { addr: "127.0.0.1".to_string() });
    config.features = Some(Features {
        memory: vec![Memory {
            server_host: "h".to_string(),
            port: 9000,
            listen_host: "127.0.0.1".to_string(),
            when: vec![],
            default_topics: vec![],
            default_type: None,
            default_importance: None,
            type_importance: Default::default(),
            consolidation: None,
            retention: None,
            auto_prune: false,
        }],
        ..Default::default()
    });
    let err = config.validate().unwrap_err();
    assert!(matches!(err, ValidateError::MemoryNoTags));
}

#[test]
fn memory_unknown_server_host_rejected() {
    let mut config = minimal_valid_config();
    config.features = Some(Features {
        memory: vec![Memory {
            server_host: "does-not-exist".to_string(),
            port: 9000,
            listen_host: "127.0.0.1".to_string(),
            when: vec!["t".to_string()],
            default_topics: vec![],
            default_type: None,
            default_importance: None,
            type_importance: Default::default(),
            consolidation: None,
            retention: None,
            auto_prune: false,
        }],
        ..Default::default()
    });
    let err = config.validate().unwrap_err();
    assert!(matches!(err, ValidateError::MemoryUnknownServerHost(h) if h == "does-not-exist"));
}
```

(If `minimal_valid_config()` doesn't exist as a shared helper, grep the file
for the pattern the existing `Memory`-adjacent tests use to build a base
`Config` — mirror that exact helper instead of introducing a second one.)

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p llmenv-config codebase_memory_requires_tags memory_requires_tags memory_unknown_server_host_rejected -- --nocapture`
Expected: FAIL — `CodebaseMemoryNoTags` variant doesn't exist (compile error)

- [ ] **Step 3: Add the error variant and validation block**

In `crates/llmenv-config/src/validate.rs:47-54`, add to `ValidateError`:

```rust
    #[error("features.codebase_memory entry has no `when` tags")]
    CodebaseMemoryNoTags,
```

In `validate_mcps` (validate.rs:607-641), add after the `throttle` loop:

```rust
    for cm in &features.codebase_memory {
        if cm.when.is_empty() {
            return Err(ValidateError::CodebaseMemoryNoTags);
        }
    }
```

- [ ] **Step 4: Add the bundle-side duplicate check**

In `src/merge/mod.rs:230-261` (`read_bundle_yaml`), add alongside the
existing `features.memory`/`features.throttle` bundle-level checks:

```rust
    for cm in &capabilities.features.as_ref().map(|f| f.codebase_memory.as_slice()).unwrap_or_default() {
        if cm.when.is_empty() {
            anyhow::bail!("bundle {bundle_name}: features.codebase_memory entry has no `when` tags");
        }
    }
```

(Match the exact surrounding variable names — `bundle_name`,
`capabilities` — from the existing `memory`/`throttle` blocks in this
function; don't invent new names.)

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test -p llmenv-config codebase_memory memory_requires_tags memory_unknown_server_host_rejected`
Expected: PASS (4 tests)

- [ ] **Step 6: Commit**

```bash
cargo fmt -p llmenv-config
git add crates/llmenv-config/src/validate.rs src/merge/mod.rs
git commit -m "feat(config): validate features.codebase_memory tags, add missing Memory validation tests"
```

---

## Task 3: Proptest coverage for `CodebaseMemory`

**Files:**
- Modify: `crates/llmenv-config/src/validate.rs:984-1011` (`arb_memory` /
  `arb_config` proptest generators)
- Test: same file's proptest module

**Interfaces:**
- Consumes: `CodebaseMemory` from Task 1

- [ ] **Step 1: Write the failing property test**

Add alongside `arb_memory()`:

```rust
fn arb_codebase_memory() -> impl Strategy<Value = CodebaseMemory> {
    (
        prop::collection::vec("[a-z][a-z0-9_-]{0,10}", 1..3),
        prop::option::of("[a-zA-Z0-9/_.-]{1,40}"),
    )
        .prop_map(|(when, index_path)| CodebaseMemory { when, index_path })
}

proptest! {
    #[test]
    fn codebase_memory_yaml_roundtrips(cm in arb_codebase_memory()) {
        let yaml = serde_yaml::to_string(&cm).unwrap();
        let parsed: CodebaseMemory = serde_yaml::from_str(&yaml).unwrap();
        prop_assert_eq!(cm, parsed);
    }
}
```

Then wire `arb_codebase_memory()` into `arb_config()` (the same way
`arb_memory()` feeds `Features.memory` there) so whole-`Config` round-trip
tests exercise it too — add a `codebase_memory: vec![arb_codebase_memory()...]`
(or the empty-vec default, matching however `arb_memory` is optionally
included) alongside the existing `memory` field construction in
`arb_config()`.

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p llmenv-config codebase_memory_yaml_roundtrips -- --nocapture`
Expected: FAIL — `arb_codebase_memory` undefined (compile error)

- [ ] **Step 3: Implementation is the generator itself (already written above)** — no separate step; this task's "implementation" is the test infrastructure.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p llmenv-config codebase_memory_yaml_roundtrips arb_config`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
cargo fmt -p llmenv-config
git add crates/llmenv-config/src/validate.rs
git commit -m "test(config): add proptest coverage for CodebaseMemory"
```

---

## Task 4: Resolver — materialize into a local stdio MCP entry

**Files:**
- Modify: `src/mcp/resolve.rs` (new `resolve_codebase_memory` function +
  wiring into `resolve_mcps`; new `ResolveError` variant if needed)
- Modify: `src/mcp/resolve.rs` call sites — `resolve_mcps`'s signature gains
  a `codebase_memory: &[CodebaseMemory]` parameter; update all callers:
  `src/cli/mod.rs:1583` (`build_manifest`), `src/materialize/status_data.rs:131-143`
  (`collect_mcps`), `src/cli/status.rs:222-276` (`run_mcp_ls`),
  `src/hook_run/mod.rs:1086-1160` (`memory_url` — this one only cares about
  ICM's URL, so it can ignore the new parameter or the signature split
  can leave `memory_url` alone if it calls a narrower helper; check at
  implementation time which is cleaner and prefer the narrower helper if
  `memory_url` doesn't already route through the shared `resolve_mcps`
  wrapper — read the current call site first)
- Modify: `crates/llmenv-paths` or wherever `state_dir()` is re-exported (see
  `src/paths.rs`) — need `state_dir()` for the default `CBM_CACHE_DIR`
- Test: `src/mcp/resolve.rs` (new `#[test]` functions mirroring the existing
  `memory_*` test shapes)

**Interfaces:**
- Consumes: `CodebaseMemory` (Task 1), `ResolvedKind::Stdio { command, args, env }` (existing)
- Produces: `pub fn resolve_codebase_memory(cm: &CodebaseMemory, project_root: &Path, state_dir: &Path) -> ResolvedMcp`

- [ ] **Step 1: Write the failing tests**

Add to `src/mcp/resolve.rs`'s test module:

```rust
#[test]
fn codebase_memory_resolves_to_local_stdio() {
    let cm = CodebaseMemory { when: vec!["proj".to_string()], index_path: None };
    let resolved = resolve_codebase_memory(&cm, Path::new("/repos/proj"), Path::new("/state"));
    match resolved.kind {
        ResolvedKind::Stdio { command, args, env } => {
            assert_eq!(command, "codebase-memory-mcp");
            assert!(args.is_empty());
            assert_eq!(env.get("CBM_ALLOWED_ROOT").map(String::as_str), Some("/repos/proj"));
            assert_eq!(
                env.get("CBM_CACHE_DIR").map(String::as_str),
                Some("/state/codebase-memory")
            );
        }
        ResolvedKind::Remote { .. } => panic!("codebase_memory must resolve to local stdio, not remote"),
    }
}

#[test]
fn codebase_memory_index_path_override_wins() {
    let cm = CodebaseMemory {
        when: vec!["proj".to_string()],
        index_path: Some("/custom/path".to_string()),
    };
    let resolved = resolve_codebase_memory(&cm, Path::new("/repos/proj"), Path::new("/state"));
    match resolved.kind {
        ResolvedKind::Stdio { env, .. } => {
            assert_eq!(env.get("CBM_CACHE_DIR").map(String::as_str), Some("/custom/path"));
        }
        _ => panic!("expected Stdio"),
    }
}

#[test]
fn codebase_memory_not_selected_when_tags_inactive() {
    let cm = vec![CodebaseMemory { when: vec!["other-tag".to_string()], index_path: None }];
    let active_tags: BTreeSet<String> = ["proj".to_string()].into_iter().collect();
    let resolved = resolve_mcps(&[], &[], &cm, &BTreeMap::new(), &active_tags, Path::new("/repos/proj"), Path::new("/state")).unwrap();
    assert!(resolved.is_empty());
}
```

(Signature of `resolve_mcps` in the last test above is illustrative of the
new parameter shape — adjust to match exactly what Step 3 below actually
implements; keep the parameter list ≤5 by grouping `project_root`/`state_dir`
into a small struct if needed, per the project's positional-param limit —
mirror how `SessionLogCall` groups borrowed inputs in `src/hook_run/mod.rs`
if `resolve_mcps` grows past 5 params.)

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p llmenv resolve_codebase_memory -- --nocapture`
Expected: FAIL — function undefined (compile error)

- [ ] **Step 3: Implement the resolver**

Add to `src/mcp/resolve.rs`, next to `resolve_memory`:

```rust
/// Resolve a `CodebaseMemory` entry to a local stdio MCP entry. Unlike
/// `resolve_memory` (always remote — ICM's daemon/proxy topology),
/// codebase-memory-mcp has no remote-serve mode: it always runs as a local
/// process per project. `CBM_CACHE_DIR` and `CBM_ALLOWED_ROOT` are always
/// computed here, never left to the user, so a declared entry can't
/// accidentally scope the indexer outside the intended project (#365).
pub fn resolve_codebase_memory(
    cm: &CodebaseMemory,
    project_root: &Path,
    state_dir: &Path,
) -> ResolvedMcp {
    let mut env = BTreeMap::new();
    let cache_dir = cm
        .index_path
        .clone()
        .unwrap_or_else(|| state_dir.join("codebase-memory").display().to_string());
    env.insert("CBM_CACHE_DIR".to_string(), cache_dir);
    env.insert(
        "CBM_ALLOWED_ROOT".to_string(),
        project_root.display().to_string(),
    );
    ResolvedMcp {
        name: CODEBASE_MEMORY_MCP_NAME.to_string(),
        kind: ResolvedKind::Stdio {
            command: "codebase-memory-mcp".to_string(),
            args: vec![],
            env,
        },
        headers: BTreeMap::new(),
        timeout: None,
        disabled_tools: vec![],
    }
}
```

Add the name constant next to `MEMORY_MCP_NAME`:

```rust
pub const CODEBASE_MEMORY_MCP_NAME: &str = "codebase-memory-mcp";
```

Wire into `resolve_mcps` (the tag-intersection loop, mirroring the existing
`active_mem` pattern but WITHOUT the "at most one" ambiguity restriction —
multiple `codebase_memory` entries for different projects can coexist since
each is local-stdio, not a shared network resource like `Memory`):

```rust
    for cm in codebase_memory
        .iter()
        .filter(|c| c.when.iter().any(|t| active_tags.contains(t)))
    {
        out.push(resolve_codebase_memory(cm, project_root, state_dir));
    }
```

Update the `resolve_mcps` signature and every call site listed in the Files
section above to pass `codebase_memory`, `project_root`, and `state_dir`
through. Read each call site first — some (like `memory_url` in
`src/hook_run/mod.rs`) may not need the new parameters if they only care
about the ICM memory URL; only thread the new params through call sites that
actually need the resolved codebase-memory entries (status, doctor, manifest
build).

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo build -p llmenv 2>&1 | tail -50` (fix any call-site signature
mismatches first), then:
`cargo test -p llmenv resolve_codebase_memory codebase_memory_`
Expected: PASS (3+ tests), full build green

- [ ] **Step 5: Commit**

```bash
cargo fmt -p llmenv
git add src/mcp/resolve.rs src/cli/mod.rs src/materialize/status_data.rs src/cli/status.rs
git commit -m "feat(mcp): resolve codebase_memory entries to local stdio MCP servers"
```

---

## Task 5: Doctor checks

**Files:**
- Modify: `src/cli/doctor.rs:152-211` (`run_doctor_tool_availability`) — add
  a `codebase-memory-mcp` binary-on-PATH check, gated on
  `config.features.as_ref().is_some_and(|f| !f.codebase_memory.is_empty())`
- Modify: `src/cli/doctor.rs:540-580` — add the orphan-tag check (no active
  scope emits the entry's `when` tags), mirroring the existing `memory`
  orphan loop exactly but without the "host table" check (codebase_memory
  has no `server_host`)
- Test: `src/cli/doctor.rs` or wherever doctor has existing test coverage —
  grep for `run_doctor_tool_availability` tests first and mirror their
  harness (likely constructs a `Config` + captures stdout/stderr)

**Interfaces:**
- Consumes: `CodebaseMemory` (Task 1)

- [ ] **Step 1: Write the failing test**

Find the existing test(s) for `run_doctor_tool_availability`'s ICM check
(search `doctor.rs` for `has_memory` in test functions) and add a sibling:

```rust
#[test]
fn doctor_flags_missing_codebase_memory_binary_when_feature_enabled() {
    let mut config = minimal_valid_config(); // reuse existing test helper
    config.features = Some(Features {
        codebase_memory: vec![CodebaseMemory { when: vec!["t".to_string()], index_path: None }],
        ..Default::default()
    });
    // Mirror however the existing ICM-binary-missing test injects a PATH
    // without `icm` — same technique, checking for `codebase-memory-mcp`
    // absence instead. Assert the output contains a warning naming
    // `codebase-memory-mcp` and pointing at the install URL.
}

#[test]
fn doctor_flags_orphan_codebase_memory_entry() {
    let mut config = minimal_valid_config();
    config.features = Some(Features {
        codebase_memory: vec![CodebaseMemory {
            when: vec!["never-active".to_string()],
            index_path: None,
        }],
        ..Default::default()
    });
    // Mirror the existing orphan-memory test: no scope emits "never-active",
    // assert the orphan warning names the tag.
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p llmenv doctor_flags_missing_codebase_memory doctor_flags_orphan_codebase_memory -- --nocapture`
Expected: FAIL — behavior not implemented (assertions fail, or the test
harness itself needs the same PATH-injection helper the ICM test uses —
confirm that helper's exact name from the existing test file first)

- [ ] **Step 3: Implement the checks**

In `run_doctor_tool_availability` (`doctor.rs:152-211`), add after the
existing `has_memory`/`icm` check:

```rust
    let has_codebase_memory = config
        .features
        .as_ref()
        .is_some_and(|f| !f.codebase_memory.is_empty());
    if has_codebase_memory && which::which("codebase-memory-mcp").is_err() {
        // match the exact eprintln!/warn glyph pattern the `icm` check above uses
        doctor_warn(use_color, "codebase-memory-mcp not found on PATH — install: https://github.com/DeusData/codebase-memory-mcp");
    }
```

(Use whatever the file's actual PATH-check helper is — grep for how the
existing `icm`/`mcp-proxy` checks test binary presence, since `which` may not
be the crate already in use; mirror it exactly rather than introducing a new
dependency.)

In the orphan-check block (`doctor.rs:540-580`), add a loop mirroring the
existing `memory` orphan loop but checking only tag-emission (no host-table
check):

```rust
    for cm in &config.features.as_ref().map(|f| f.codebase_memory.as_slice()).unwrap_or_default() {
        if !cm.when.iter().any(|t| emitted.contains(t)) {
            orphan_count += 1;
            // match the exact warn glyph pattern of the memory orphan loop above
        }
    }
```

(Do the same for `doctor_bundle_caps.features.codebase_memory`, matching the
existing dual top-level + bundle loop shape used for `memory`.)

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p llmenv doctor_flags_missing_codebase_memory doctor_flags_orphan_codebase_memory`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
cargo fmt -p llmenv
git add src/cli/doctor.rs
git commit -m "feat(doctor): check codebase-memory-mcp binary availability and orphan tags"
```

---

## Task 6: Status reporting

**Files:**
- Modify: `src/cli/status.rs:222-316` (`run_mcp_ls`) — extend the existing
  loop that appends `Memory` rows to also append `CodebaseMemory` rows

**Interfaces:**
- Consumes: `CodebaseMemory` (Task 1), resolved entries (Task 4)

- [ ] **Step 1: Write the failing test**

Find `run_mcp_ls`'s existing test(s) (grep `status.rs` for a test that
asserts a memory row appears) and add a sibling asserting a
`codebase-memory-mcp` row appears when the feature is active, with the
project path shown in its detail column.

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p llmenv run_mcp_ls -- --nocapture` (or whatever the exact
existing test name pattern is once found)
Expected: FAIL — no such row in output

- [ ] **Step 3: Implement**

In `run_mcp_ls` (`status.rs:222-316`), add after the existing `for mem in
&all_memory_ls` loop:

```rust
    for cm in &all_codebase_memory_ls {
        let is_active = cm.when.iter().any(|t| active.tags.contains(t));
        let is_orphan = !cm.when.iter().any(|t| emitted.contains(t));
        let detail = mcp_kind_detail(CODEBASE_MEMORY_MCP_NAME, "codebase-memory", &all_resolved);
        rows.push((CODEBASE_MEMORY_MCP_NAME.to_string(), is_active, is_orphan, detail));
    }
```

(`all_codebase_memory_ls` needs to be built the same way `all_memory_ls` is
— combining top-level + bundle-contributed entries — find that construction
just above the existing loop and mirror it.)

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p llmenv run_mcp_ls`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
cargo fmt -p llmenv
git add src/cli/status.rs
git commit -m "feat(status): report codebase_memory activation in llmenv status mcps"
```

---

## Task 7: SessionStart hook — register project for auto-watch

**Files:**
- Modify: `src/hook_run/mod.rs` — new function `fn trigger_codebase_memory_index(...)`,
  called from `run_inner`'s `SessionStart` handling
- Test: `tests/hook_run_failsoft.rs` (new integration test, mirroring the
  existing `pre_tool_use_read_twice_*`/`session_end_dedup_*` test shape) +
  a unit test for the command-building logic in isolation

**Interfaces:**
- Consumes: `CodebaseMemory`/resolved entries (Tasks 1, 4)
- Produces: `fn build_index_repository_command(project_root: &Path, cbm: &CodebaseMemory, state_dir: &Path) -> std::process::Command`
  (kept separate from the actual spawn so it's unit-testable without
  spawning a real process)

- [ ] **Step 1: Write the failing unit test for command construction**

Add to `src/hook_run/mod.rs`'s test module:

```rust
#[test]
fn index_repository_command_sets_env_and_args() {
    let cm = CodebaseMemory { when: vec!["proj".to_string()], index_path: None };
    let cmd = build_index_repository_command(Path::new("/repos/proj"), &cm, Path::new("/state"));
    let program = cmd.get_program().to_string_lossy().to_string();
    assert_eq!(program, "codebase-memory-mcp");
    let args: Vec<_> = cmd.get_args().map(|a| a.to_string_lossy().to_string()).collect();
    assert_eq!(args[0], "cli");
    assert_eq!(args[1], "index_repository");
    assert!(args[2].contains("/repos/proj"));
    let envs: std::collections::BTreeMap<_, _> = cmd
        .get_envs()
        .filter_map(|(k, v)| v.map(|v| (k.to_string_lossy().to_string(), v.to_string_lossy().to_string())))
        .collect();
    assert_eq!(envs.get("CBM_ALLOWED_ROOT").map(String::as_str), Some("/repos/proj"));
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p llmenv index_repository_command_sets_env_and_args -- --nocapture`
Expected: FAIL — function undefined (compile error)

- [ ] **Step 3: Implement the command builder and detached spawn**

Add to `src/hook_run/mod.rs`, near `post_session_consolidation()`:

```rust
/// Builds the `codebase-memory-mcp cli index_repository` subprocess command
/// for `project_root`, without spawning it — kept separate so tests can
/// assert on the command shape without launching a real process.
fn build_index_repository_command(
    project_root: &Path,
    cm: &CodebaseMemory,
    state_dir: &Path,
) -> std::process::Command {
    let args_json = serde_json::json!({ "repo_path": project_root.display().to_string() }).to_string();
    let cache_dir = cm
        .index_path
        .clone()
        .unwrap_or_else(|| state_dir.join("codebase-memory").display().to_string());
    let mut cmd = std::process::Command::new("codebase-memory-mcp");
    cmd.args(["cli", "index_repository", &args_json])
        .env("CBM_ALLOWED_ROOT", project_root.display().to_string())
        .env("CBM_CACHE_DIR", cache_dir)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    cmd
}

/// Fire-and-forget: registers `project_root` with codebase-memory-mcp's
/// index + background auto-watch (#365). Mirrors `post_session_consolidation`'s
/// detached-spawn pattern — indexing a large repo can take minutes (the
/// Linux kernel takes ~3, per upstream benchmarks), so this must never block
/// SessionStart. Best-effort: spawn failures are logged at debug level only.
fn trigger_codebase_memory_index(project_root: &Path, cm: &CodebaseMemory, state_dir: &Path) {
    let mut cmd = build_index_repository_command(project_root, cm, state_dir);
    crate::mcp::proxy::detach_process_group(&mut cmd);
    if let Err(e) = cmd.spawn() {
        tracing::debug!("codebase-memory-mcp index_repository: failed to spawn: {e}");
    }
}
```

Wire into `run_inner`'s `SessionStart` path — find where `dispatch()` is
called for `SessionStart` and, alongside it, check for an active
`codebase_memory` entry (same tag-intersection test used elsewhere) and call
`trigger_codebase_memory_index` with the current working directory as
`project_root` and `crate::paths::state_dir()?` as `state_dir`. Only fire
this when exactly one `codebase_memory` entry matches the active tags (if
none match, no-op; if the project has no matching entry, this whole block is
skipped).

- [ ] **Step 4: Write the failing integration test**

Add to `tests/hook_run_failsoft.rs` (mirroring `config_with_read_once` /
`hook_cmd` helpers already in the file):

```rust
fn config_with_codebase_memory() -> String {
    format!(
        r#"
scope:
  network: []
  host: []
  user:
    - id: test-user
      match:
        user: {user}
      tags: [test]

tag:
  test: ""

bundle:
  - name: test-bundle
    when: [test]

features:
  codebase_memory:
    - when: [test]

cache:
  sync_interval_minutes: 60

adapter:
  engine: claude-code
"#,
        user = current_user(),
    )
}

// #365: SessionStart with an active codebase_memory scope must not block or
// fail even when the codebase-memory-mcp binary isn't installed (fail-soft
// contract — the spawn attempt itself must never surface as a hook failure).
#[test]
fn session_start_with_codebase_memory_exits_zero_without_binary() {
    let (dir, config_path) = setup_config(&config_with_codebase_memory());
    hook_cmd(dir.path(), &config_path, "session_start")
        .timeout(Duration::from_secs(10))
        .assert()
        .success();
}
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test -p llmenv index_repository_command_sets_env_and_args`
Run: `cargo test --test hook_run_failsoft session_start_with_codebase_memory`
Expected: PASS (both)

- [ ] **Step 6: Commit**

```bash
cargo fmt -p llmenv
git add src/hook_run/mod.rs tests/hook_run_failsoft.rs
git commit -m "feat(hook-run): register project with codebase-memory-mcp on SessionStart"
```

---

## Task 8: Docs + CHANGELOG

**Files:**
- Modify: `website/docs/` — find wherever `features.memory` is documented
  (grep `website/docs/` for `features.memory` or `server_host`) and add a
  sibling `features.codebase_memory` section: config shape, the two env vars
  llmenv always computes (`CBM_CACHE_DIR`, `CBM_ALLOWED_ROOT`), the
  SessionStart auto-register behavior, and a link to
  https://github.com/DeusData/codebase-memory-mcp for install instructions.
- Modify: `CHANGELOG-3.md` (`[Unreleased]` → `### Added`) via the
  `keepachangelog` skill

**Interfaces:** none (docs-only)

- [ ] **Step 1: Find the existing `features.memory` doc page**

Run: `git grep -l "features.memory" website/docs/`

- [ ] **Step 2: Write the `features.codebase_memory` doc section**

Add a section with the same structure as the `features.memory` one found
above: YAML example, field reference table (`when`, `index_path`), the
auto-computed env vars, the SessionStart auto-register behavior, and doctor
checks it adds.

- [ ] **Step 3: Run the keepachangelog skill**

Invoke the `keepachangelog` skill for a `[Unreleased]` → `### Added` entry
in `CHANGELOG-3.md`: "Add `features.codebase_memory` — first-class
integration for the codebase-memory-mcp MCP server: tag-activated local
stdio entry, auto-computed index-path/allowed-root env vars, SessionStart
registration for the server's own background reindex watcher, and
`llmenv doctor`/`llmenv status` checks (#365)."

- [ ] **Step 4: Regenerate the synced changelog doc**

Run: `bash scripts/sync-changelog-doc.sh`

- [ ] **Step 5: Commit**

```bash
git add website/docs/ CHANGELOG-3.md
git commit -m "docs: document features.codebase_memory"
```

---

## Task 9: Full-suite verification

- [ ] **Step 1: Full workspace build**

Run: `cargo build --workspace 2>&1 | tail -100`
Expected: no warnings, no errors

- [ ] **Step 2: Full workspace test suite**

Run: `cargo test --workspace 2>&1 | tail -60`
Expected: all pass, no regressions vs. the pre-task baseline

- [ ] **Step 3: Clippy**

Run: `cargo clippy --workspace --all-targets --all-features -- -D warnings 2>&1 | tail -80`
Expected: no issues

- [ ] **Step 4: Commit if fmt/clippy made any final touch-ups**

```bash
cargo fmt --all
git add -A
git commit -m "chore: fmt" --allow-empty-message -m "cargo fmt pass" 2>/dev/null || true
```
