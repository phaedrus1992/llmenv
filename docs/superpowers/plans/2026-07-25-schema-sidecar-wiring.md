# Schema Sidecar Wiring Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Wire the dead `src/materialize/schema_gen.rs` module into the crate and make the opencode adapter actually emit an `opencode.schema.json` sidecar next to `opencode.json`, closing #1001 and making the existing (already-shipped) 3.3.0 changelog claim true.

**Architecture:** `OpencodeConfig` (and its nested output structs) already exist as typed structs deriving `serde::Serialize` in `src/adapter/opencode.rs` — only the `mcp` field is still an untyped `serde_json::Value` because it goes through a post-construction native-overlay merge. Add `schemars::JsonSchema` derives to every one of those structs, introduce one new typed `McpEntry` enum that the MCP-assembly code actually builds (replacing today's manual `serde_json::Map` building, byte-identical), point schemars at that type for the `mcp` field via `#[schemars(with = ...)]`, add a `config_schema()` default-`None` method to the `AgentAdapter` trait, implement it for `OpencodeAdapter` by calling `schemars::schema_for!(OpencodeConfig)`, and write the sidecar inside `OpencodeAdapter::materialize()` right after `opencode.json` is written — registering it in the same `owned: Vec<PathBuf>` the caller already unions into the stale/GC tracking set, so no changes to `src/cli/mod.rs` are needed.

**Tech Stack:** Rust, `schemars` 1.2.1 (new dependency), `serde`/`serde_json` (existing).

## Global Constraints

- `materialize()` output for existing manifests must be **byte-identical** before/after the `McpEntry` refactor — the full existing test suite (`cargo test`) is the guard; zero new failures allowed at any point.
- Do **not** touch `ClaudeCodeAdapter` or `CrushAdapter` — they keep the trait default (`None`), no sidecar, out of scope per the issue.
- All new dependency additions must be pinned exact (`=1.2.1`), matching every other entry in `Cargo.toml`.
- `cargo deny check` must pass after the dependency add; regenerate `THIRD-PARTY-LICENSES.md` and `website/docs/third-party-licenses.md` via `scripts/gen-attribution.sh` in the same commit as the `Cargo.lock` update if anything changed.
- CHANGELOG entry goes under `[Unreleased]` in `CHANGELOG-3.md` (the real source file; `website/docs/changelog.md` is generated — do not hand-edit it).
- Base branch: `release/3.x`. Branch: `fix/1001-schema-sidecar-wiring` (already created).
- Closing reference: `Fixes #1001`.

---

### Task 1: Add `schemars` dependency and wire the dead `schema_gen` module into the crate

**Files:**
- Modify: `Cargo.toml` (add dependency under `[dependencies]`)
- Modify: `src/materialize/mod.rs:1-4` (add `mod schema_gen;`)
- Test: `src/materialize/schema_gen.rs` (tests already exist in this file — currently dead because the module isn't declared anywhere, so `cargo test` never compiles or runs them)

**Interfaces:**
- Produces: `crate::materialize::schema_gen::with_root_additional_properties(schema: serde_json::Value) -> serde_json::Value` — already implemented in the file, just needs to become reachable. Later tasks call this as `crate::materialize::schema_gen::with_root_additional_properties(...)`.

- [ ] **Step 1: Confirm the RED state — the existing tests don't run today**

Run: `cargo test --lib schema_gen 2>&1 | tail -5`
Expected: `running 0 tests` (or a "no tests ran" style summary) — proves the module is currently dead code, matching issue #1001's evidence.

- [ ] **Step 2: Add the dependency**

In `Cargo.toml`, under `[dependencies]` (alphabetically near `reqwest`/`rustix`, doesn't matter, keep near other single-purpose deps):

```toml
schemars = "=1.2.1"
```

- [ ] **Step 3: Declare the module**

In `src/materialize/mod.rs`, change:

```rust
pub mod cache;
pub mod manifest;
pub mod state;
mod status_data;
```

to:

```rust
pub mod cache;
pub mod manifest;
pub mod schema_gen;
pub mod state;
mod status_data;
```

(`pub` because `OpencodeAdapter::config_schema()` in Task 4 calls `crate::materialize::schema_gen::with_root_additional_properties` from `src/adapter/opencode.rs`, a different module.)

- [ ] **Step 4: Run and verify GREEN**

Run: `cargo test --lib schema_gen 2>&1 | tail -10`
Expected: 4 tests pass — `adds_additional_properties_to_root`, `idempotent_when_already_present`, `non_object_value_returns_unchanged`, `generated_schema_is_valid_json_schema_shape`.

- [ ] **Step 5: Commit**

```bash
git add Cargo.toml Cargo.lock src/materialize/mod.rs
git commit -m "feat(materialize): wire dead schema_gen module into the crate

refs #1001"
```

---

### Task 2: Add `config_schema()` to the `AgentAdapter` trait (default `None`)

**Files:**
- Modify: `src/adapter/mod.rs` (trait definition, ~line 71-170; test module at line 439)

**Interfaces:**
- Consumes: nothing new.
- Produces: `AgentAdapter::config_schema(&self) -> Option<serde_json::Value>`, default body `None`. `OpencodeAdapter` overrides it in Task 4; `ClaudeCodeAdapter`/`CrushAdapter` are untouched and inherit `None`.

- [ ] **Step 1: Write the failing test**

In `src/adapter/mod.rs`'s existing `#[cfg(test)] mod tests { ... }` block (starts at line 439), add:

```rust
    #[test]
    fn config_schema_defaults_to_none_for_adapters_without_a_schema() {
        assert!(
            crate::adapter::claude_code::ClaudeCodeAdapter
                .config_schema()
                .is_none()
        );
        assert!(crate::adapter::crush::CrushAdapter.config_schema().is_none());
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib config_schema_defaults_to_none -- --nocapture`
Expected: compile error — `no method named 'config_schema' found for struct 'ClaudeCodeAdapter'`.

- [ ] **Step 3: Add the trait method**

In `src/adapter/mod.rs`, inside `pub trait AgentAdapter { ... }`, add (placement anywhere in the trait body, e.g. right after `fn supported_hook_events(&self) -> &'static [&'static str];`):

```rust
    /// JSON Schema describing this adapter's materialized output, derived
    /// from the same typed structs that build it. `None` (the default)
    /// means the adapter has no typed output structs yet and emits no
    /// schema sidecar.
    fn config_schema(&self) -> Option<serde_json::Value> {
        None
    }
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --lib config_schema_defaults_to_none -- --nocapture`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/adapter/mod.rs
git commit -m "feat(adapter): add config_schema() trait method, default None

refs #1001"
```

---

### Task 3: Typed `McpEntry` + `JsonSchema` derives across the opencode output structs

**Files:**
- Modify: `src/adapter/opencode.rs` (struct derives ~lines 24-145; MCP-assembly code ~lines 662-701; test module, exact line TBD by prior edits — search `mod tests` at the bottom of the file)

**Interfaces:**
- Consumes: `crate::mcp::resolve::ResolvedKind` (`Stdio { command: String, args: Vec<String>, env: BTreeMap<String,String> }` / `Remote { url: String, transport: McpTransport }`), `ResolvedMcp.headers: BTreeMap<String,String>`, `ResolvedMcp.timeout: Option<u32>` — all pre-existing, unchanged.
- Produces: `McpEntry` enum (new, private to `opencode.rs`) — used by Task 4's `#[schemars(with = ...)]` attribute on `OpencodeConfig.mcp`.

- [ ] **Step 1: Confirm baseline GREEN before touching anything**

Run: `cargo test --lib opencode:: 2>&1 | tail -5`
Expected: all existing `opencode` adapter tests pass (this is the byte-identical safety net for this task).

- [ ] **Step 2: Add the `JsonSchema` derive to every typed opencode output struct**

In `src/adapter/opencode.rs`, change each of these derive lines (all currently `#[derive(serde::Serialize)]` or `#[derive(serde::Serialize, Default)]`):

```rust
#[derive(serde::Serialize)]
struct OpencodeConfig {
```
→
```rust
#[derive(serde::Serialize, schemars::JsonSchema)]
struct OpencodeConfig {
```

Same substitution (`serde::Serialize` → `serde::Serialize, schemars::JsonSchema`, preserving any other derives already present) for:
- `struct OpencodeProviderEntry`
- `#[derive(serde::Serialize, Default)] struct OpencodeProviderOptions` → `#[derive(serde::Serialize, schemars::JsonSchema, Default)]`
- `struct OpencodeModelEntry`
- `struct OpencodeModelLimit`
- `struct OpencodeModelCost`
- `struct OpencodeModalities`
- `struct LspServerEntry`
- `#[derive(serde::Serialize)] #[serde(untagged)] enum PermissionValue` → `#[derive(serde::Serialize, schemars::JsonSchema)]` (keep the `#[serde(untagged)]` line unchanged)

- [ ] **Step 3: Define the new `McpEntry` type**

Immediately after the `PermissionValue` enum's closing `}` in `src/adapter/opencode.rs`, add:

```rust
/// A single `mcp.<name>` entry in `opencode.json`. This is the type the
/// MCP-assembly step in `materialize()` actually constructs and serializes
/// — both the JSON output and the generated schema (via
/// `#[schemars(with = ...)]` on `OpencodeConfig::mcp`) derive from it, so
/// there is exactly one definition of what an MCP entry looks like.
#[derive(serde::Serialize, schemars::JsonSchema)]
#[serde(tag = "type", rename_all = "lowercase")]
enum McpEntry {
    Local {
        command: Vec<String>,
        #[serde(skip_serializing_if = "BTreeMap::is_empty")]
        environment: BTreeMap<String, String>,
        #[serde(skip_serializing_if = "BTreeMap::is_empty")]
        headers: BTreeMap<String, String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        timeout: Option<u32>,
    },
    Remote {
        url: String,
        #[serde(skip_serializing_if = "BTreeMap::is_empty")]
        headers: BTreeMap<String, String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        timeout: Option<u32>,
    },
}
```

- [ ] **Step 4: Point the `mcp` field's schema at `McpEntry`**

In the `OpencodeConfig` struct, change:

```rust
    /// MCP server configs — kept as Value because entries go through
    /// per-server native_mcp overlay after construction.
    #[serde(skip_serializing_if = "Option::is_none")]
    mcp: Option<serde_json::Value>,
```

to:

```rust
    /// MCP server configs — kept as Value because entries go through
    /// per-server native_mcp overlay after construction; `#[schemars(with)]`
    /// tells schemars to describe the field as if it were typed, without
    /// changing what's actually serialized.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(with = "Option<BTreeMap<String, McpEntry>>")]
    mcp: Option<serde_json::Value>,
```

- [ ] **Step 5: Refactor the MCP-assembly code to build `McpEntry` instead of a manual `Map`**

In `src/adapter/opencode.rs`, inside the `// 7. MCP servers` block, find:

```rust
                for mcp in &manifest.mcps {
                    let mut e = match &mcp.kind {
                        ResolvedKind::Stdio { command, args, env } => {
                            let mut cmd: Vec<serde_json::Value> =
                                Vec::with_capacity(1 + args.len());
                            cmd.push(serde_json::json!(command));
                            cmd.extend(args.iter().map(|a| serde_json::json!(a)));
                            let mut e = serde_json::Map::new();
                            e.insert("type".into(), serde_json::json!("local"));
                            e.insert("command".into(), serde_json::json!(cmd));
                            if !env.is_empty() {
                                e.insert("environment".into(), serde_json::json!(env));
                            }
                            e
                        }
                        ResolvedKind::Remote { url, transport: _ } => {
                            let mut e = serde_json::Map::new();
                            e.insert("type".into(), serde_json::json!("remote"));
                            e.insert("url".into(), serde_json::json!(url));
                            e
                        }
                    };
                    if !mcp.headers.is_empty() {
                        e.insert("headers".into(), serde_json::json!(mcp.headers));
                    }
                    if let Some(t) = mcp.timeout {
                        e.insert("timeout".into(), serde_json::json!(t));
                    }
                    mcp_obj.insert(mcp.name.clone(), serde_json::Value::Object(e));
                }
```

Replace with:

```rust
                for mcp in &manifest.mcps {
                    let entry = match &mcp.kind {
                        ResolvedKind::Stdio { command, args, env } => {
                            let mut cmd: Vec<String> = Vec::with_capacity(1 + args.len());
                            cmd.push(command.clone());
                            cmd.extend(args.iter().cloned());
                            McpEntry::Local {
                                command: cmd,
                                environment: env.clone(),
                                headers: mcp.headers.clone(),
                                timeout: mcp.timeout,
                            }
                        }
                        ResolvedKind::Remote { url, transport: _ } => McpEntry::Remote {
                            url: url.clone(),
                            headers: mcp.headers.clone(),
                            timeout: mcp.timeout,
                        },
                    };
                    let value = serde_json::to_value(&entry).map_err(|err| {
                        anyhow::anyhow!(
                            "MCP server '{}': failed to serialize entry: {err}",
                            mcp.name
                        )
                    })?;
                    mcp_obj.insert(mcp.name.clone(), value);
                }
```

- [ ] **Step 6: Run the full suite to verify zero behavior change**

Run: `cargo test --quiet 2>&1 | tail -20`
Expected: same pass count as Task 3 Step 1's baseline (plus Task 1/2's new tests) — in particular `materialize_mcp_local_server_written`, `materialize_mcp_remote_server_written`, and `materialize_mcp_optional_fields_omitted` in `src/adapter/opencode.rs` must still pass unchanged, proving the JSON output is byte-identical.

- [ ] **Step 7: Commit**

```bash
git add src/adapter/opencode.rs
git commit -m "refactor(opencode): typed McpEntry struct drives MCP JSON + schema

Adds schemars::JsonSchema to every opencode output struct and replaces the
manual serde_json::Map building for MCP entries with a typed McpEntry enum,
so the generated schema and the rendered opencode.json come from the same
struct. No behavior change — full suite green, zero snapshot diffs.

refs #1001"
```

---

### Task 4: Implement `OpencodeAdapter::config_schema()` and emit the sidecar

**Files:**
- Modify: `src/adapter/opencode.rs` (const near `OPENCODE_JSON_FILE`; `impl AgentAdapter for OpencodeAdapter` block; MCP-write call site ~line 944-946; test module)

**Interfaces:**
- Consumes: `crate::materialize::schema_gen::with_root_additional_properties` (Task 1), `OpencodeConfig` (Task 3).
- Produces: `OpencodeAdapter::config_schema(&self) -> Option<serde_json::Value>` returning `Some(schema)`; `materialize()` writes `opencode.schema.json` and includes it in the returned `owned: Vec<PathBuf>`.

- [ ] **Step 1: Write the failing tests**

In `src/adapter/opencode.rs`'s `#[cfg(test)] mod tests { ... }` block, add:

```rust
    #[test]
    fn config_schema_returns_a_schema_describing_the_authored_keys() {
        let schema = OpencodeAdapter
            .config_schema()
            .expect("opencode adapter must emit a schema");
        assert_eq!(
            schema["$schema"],
            serde_json::json!("https://json-schema.org/draft/2020-12/schema")
        );
        assert_eq!(schema["type"], serde_json::json!("object"));
        assert_eq!(schema["additionalProperties"], serde_json::json!(true));
        let props = schema["properties"].as_object().unwrap();
        assert!(props.contains_key("mcp"), "schema must describe 'mcp'");
        assert!(props.contains_key("lsp"), "schema must describe 'lsp'");
        assert!(
            props.contains_key("permission"),
            "schema must describe 'permission'"
        );
    }

    #[test]
    fn materialize_writes_schema_sidecar_alongside_opencode_json() {
        let tmp = tempfile::tempdir().unwrap();
        let manifest = MergedManifest::default();
        let owned = OpencodeAdapter.materialize(&manifest, tmp.path()).unwrap();

        let sidecar_path = tmp.path().join("opencode.schema.json");
        assert!(
            sidecar_path.exists(),
            "expected opencode.schema.json sidecar to be written"
        );
        let raw = std::fs::read_to_string(&sidecar_path).unwrap();
        let doc: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(doc["type"], serde_json::json!("object"));
        assert!(owned.contains(&PathBuf::from("opencode.schema.json")));
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib config_schema_returns_a_schema -- --nocapture` and `cargo test --lib materialize_writes_schema_sidecar -- --nocapture`
Expected: first fails with "opencode adapter must emit a schema" panic (default trait `None`); second fails with "expected opencode.schema.json sidecar to be written" (file doesn't exist).

- [ ] **Step 3: Add the sidecar filename constant**

Next to the existing `const OPENCODE_JSON_FILE: &str = "opencode.json";`, add:

```rust
const OPENCODE_SCHEMA_FILE: &str = "opencode.schema.json";
```

- [ ] **Step 4: Implement `config_schema()`**

In `impl AgentAdapter for OpencodeAdapter { ... }`, add:

```rust
    fn config_schema(&self) -> Option<serde_json::Value> {
        let schema = schemars::schema_for!(OpencodeConfig);
        let value = serde_json::to_value(&schema).ok()?;
        Some(crate::materialize::schema_gen::with_root_additional_properties(
            value,
        ))
    }
```

- [ ] **Step 5: Write the sidecar in `materialize()`**

Find the existing write of `opencode.json`:

```rust
        let json_bytes = serde_json::to_vec_pretty(&doc_value)?;
        let out_path = out.join(OPENCODE_JSON_FILE);
        crate::paths::write_owner_only(&out_path, &json_bytes)?;
        owned.push(PathBuf::from(OPENCODE_JSON_FILE));
```

Add immediately after it:

```rust
        if let Some(schema) = self.config_schema() {
            let schema_bytes = serde_json::to_vec_pretty(&schema)?;
            let schema_path = out.join(OPENCODE_SCHEMA_FILE);
            crate::paths::write_owner_only(&schema_path, &schema_bytes)?;
            owned.push(PathBuf::from(OPENCODE_SCHEMA_FILE));
        }
```

- [ ] **Step 6: Run tests to verify they pass**

Run: `cargo test --lib opencode:: 2>&1 | tail -20`
Expected: all pass, including the two new tests.

- [ ] **Step 7: Write the failing "no sidecar for adapters without a schema" test**

In `src/adapter/claude_code.rs`'s `#[cfg(test)] mod tests { ... }` block (starts at line 2107), add:

```rust
    #[test]
    fn materialize_emits_no_schema_sidecar_when_adapter_has_none() {
        let tmp = tempfile::tempdir().unwrap();
        let manifest = MergedManifest::default();
        ClaudeCodeAdapter.materialize(&manifest, tmp.path()).unwrap();
        let has_schema_file = std::fs::read_dir(tmp.path())
            .unwrap()
            .filter_map(|entry| entry.ok())
            .any(|entry| entry.file_name().to_string_lossy().ends_with(".schema.json"));
        assert!(
            !has_schema_file,
            "ClaudeCodeAdapter has no config_schema() override — must emit no sidecar"
        );
    }
```

This test should already pass (ClaudeCodeAdapter never calls `config_schema()`-driven sidecar logic — that block only exists in `OpencodeAdapter::materialize()`). Run it to confirm it passes on the first try — if it fails, something leaked the sidecar-writing logic into a shared code path and that's a real bug to fix before continuing.

Run: `cargo test --lib materialize_emits_no_schema_sidecar -- --nocapture`
Expected: PASS immediately.

- [ ] **Step 8: Run the full suite**

Run: `cargo test --quiet 2>&1 | tail -10`
Expected: all tests pass, no regressions.

- [ ] **Step 9: Commit**

```bash
git add src/adapter/opencode.rs src/adapter/claude_code.rs
git commit -m "feat(opencode): emit opencode.schema.json sidecar in materialize

OpencodeAdapter::config_schema() derives a JSON Schema from OpencodeConfig
via schemars::schema_for!, tolerant of passthrough keys via
additionalProperties: true. materialize() writes it alongside opencode.json
and registers it in the owned-files set so it participates in the same
staleness/regeneration lifecycle. Other adapters are unaffected (trait
default stays None).

Fixes #1001"
```

---

### Task 5: Dependency hygiene — `cargo deny` + attribution regeneration

**Files:**
- Modify (if needed): `deny.toml`, `about.toml` (only if `cargo deny check` reports a new license id)
- Regenerate: `THIRD-PARTY-LICENSES.md`, `website/docs/third-party-licenses.md`

**Interfaces:** none (infra/compliance task, no test).

- [ ] **Step 1: Run cargo deny**

Run: `cargo deny check 2>&1 | tail -40`
Expected: passes. `schemars` is MIT-licensed and `MIT` is already in both `deny.toml`'s and `about.toml`'s allow lists — no edit expected. If it fails on a *transitive* dependency's license, add that SPDX id to both `deny.toml`'s `[licenses].allow` and `about.toml`'s `accepted` array (only after confirming it isn't a strong-copyleft license), then re-run until it passes.

- [ ] **Step 2: Regenerate attribution files**

Run: `scripts/gen-attribution.sh`

- [ ] **Step 3: Verify the new dependency appears**

Run: `grep -c schemars THIRD-PARTY-LICENSES.md website/docs/third-party-licenses.md`
Expected: non-zero count in both files.

- [ ] **Step 4: Commit**

```bash
git add Cargo.lock THIRD-PARTY-LICENSES.md website/docs/third-party-licenses.md deny.toml about.toml
git commit -m "chore(deps): regenerate attribution for schemars dependency

refs #1001"
```

(If `deny.toml`/`about.toml` were untouched, `git add` on them is a no-op — fine to include unconditionally in the command.)

---

### Task 6: Docs + CHANGELOG

**Files:**
- Modify: `website/docs/engines.md` (opencode "What the opencode adapter emits" table, ~line 184)
- Modify: `CHANGELOG-3.md` (the real changelog source; `[Unreleased]` section, `### Fixed`)

**Interfaces:** none (docs-only).

- [ ] **Step 1: Document the sidecar in engines.md**

In `website/docs/engines.md`, in the "What the opencode adapter emits" table, change:

```markdown
| `opencode.json` | `$schema`, `instructions`, `mcp`, `lsp`, `permission`, `plugin` — structured render, then `native_*.opencode` overlays deep-merged at the value level |
```

to add a new row directly beneath it:

```markdown
| `opencode.json` | `$schema`, `instructions`, `mcp`, `lsp`, `permission`, `plugin` — structured render, then `native_*.opencode` overlays deep-merged at the value level |
| `opencode.schema.json` | JSON Schema (draft 2020-12) generated from the same typed structs that render `opencode.json`, so it always matches what llmenv actually writes. Root allows `additionalProperties`, so passthrough/native-overlay keys never fail IDE validation. |
```

- [ ] **Step 2: Add the CHANGELOG entry**

In `CHANGELOG-3.md`, under `## [Unreleased] - ReleaseDate`, find or create a `### Fixed` subsection and add:

```markdown
- `llmenv materialize`'s `opencode.schema.json` sidecar — documented as shipping back in 3.3.0 (#660) but never actually wired into the crate — now really gets written alongside `opencode.json`. See [Engines](https://phaedrus1992.github.io/llmenv/docs/engines#what-the-opencode-adapter-emits) (#1001)
```

Place it so `### Fixed` entries stay grouped (if `### Added`/`### Changed` subsections already exist above under `[Unreleased]`, add `### Fixed` after them, following the existing subsection ordering convention in this file).

- [ ] **Step 3: Regenerate the generated changelog doc**

Run: `scripts/sync-changelog-doc.sh` (if the script exists — check `ls scripts/sync-changelog-doc.sh` first; if it doesn't exist, skip this step and note it in the PR description instead of hand-editing `website/docs/changelog.md`)

- [ ] **Step 4: Commit**

```bash
git add website/docs/engines.md CHANGELOG-3.md website/docs/changelog.md
git commit -m "docs(changelog): document the opencode.schema.json sidecar fix

refs #1001"
```

---

## Final verification (do this before handing off to pre-pr-review)

- [ ] `cargo test --quiet` — full suite green
- [ ] `cargo clippy --all-targets --all-features -- -D warnings` — clean
- [ ] `cargo fmt --check` — clean
- [ ] `cargo deny check` — clean
- [ ] Manually inspect one materialized output: create a tempdir, call `OpencodeAdapter.materialize(&MergedManifest::default(), &tmp)`, confirm both `opencode.json` and `opencode.schema.json` exist and the schema's `properties.mcp` describes the `McpEntry` shape (`oneOf`/`anyOf` with `local`/`remote` variants).
