# opencode engine support — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a third `AgentAdapter` — `OpencodeAdapter` — so llmenv materialises config for [opencode](https://opencode.ai) with full feature parity vs the Claude adapter.

**Architecture:** Single new file `src/adapter/opencode.rs` (~800–1200 lines following the crush pattern) + registration changes in `src/adapter/mod.rs`. Adapter renders `AGENTS.md`, `opencode.json` (MCP/LSP/permissions/instructions), `skills/*/SKILL.md`, plugin content, and a generated `plugin/llmenv.js` shim that bridges llmenv hooks into opencode's JS plugin API. Open-code config schema fields verified against `packages/core/src/v1/config/{mcp,lsp,config}.ts` at `4a1982f5c`.

**Tech Stack:** Rust, `serde_json`, the existing `AgentAdapter` trait, shared adapter helpers (`adapter::skills`, `util::{merge_json,dedup}`). No new crate deps. No build-time JS tooling — the shim is a Rust string template baked in at write time.

**Reference:** Spec at `docs/superpowers/specs/2026-07-10-opencode-adapter-design.md`. Issue #657.

## Global Constraints

- Branch from `main` (Large Projects milestone).
- `Clippy` deny-level lints apply (unwrap_used, panic, todo). Test code uses `#[expect(…)]`.
- All public types derive `Debug`. See `adapter/mod.rs` for the trait contract.
- Engine id `opencode`, env signal `OPENCODE_CONFIG_DIR`, binary `opencode`.
- opencode MCP discriminator: `type: "local"` / `type: "remote"` (not crush's stdio/http/sse).
- opencode MCP: `command` is `Vec<String>`, env key is `environment` (not `env`), no `disabled_tools`.
- opencode LSP: `command` is `Vec<String>`, `extensions` (not `filetypes`), `initialization` (not `init_options`), no `root_markers`/`timeout` fields.
- Supported hook events: `SessionStart`, `SessionEnd`, `UserPromptSubmit`, `PreToolUse`, `PostToolUse`, `Stop`. Others warn-and-skip (crush precedent, `#543`).
- `mcp_tool` hook handlers: warn-and-skip (crush precedent).
- `supports_plugins()` = true, `supports_lsp()` = true.

---

### Task 1: Adapter registration + env_vars + trait-skeleton tests

**Files:**
- Modify: `src/adapter/mod.rs:1-5,115-121,98-107` (add `pub mod opencode;`, register in `registered_adapters()`, add arm to `active_adapter()`)
- Create: `src/adapter/opencode.rs`

**Interfaces:**
- Consumes: `AgentAdapter` trait (mod.rs), `MergedManifest`, `Path/PathBuf`
- Produces: `pub struct OpencodeAdapter;`, `impl AgentAdapter for OpencodeAdapter` with full trait surface, `const SUPPORTED_HOOK_EVENTS: &[&str]`, `const OPENCODE_JSON_FILE: &str`

- [ ] **Step 1: Write tests for trait probes and env_vars**

Add a test module at the bottom of `src/adapter/mod.rs` (inside the existing `#[cfg(test)] mod tests` block — add new tests next to `registry_contains_claude_and_crush_adapters`):

```rust
// In mod.rs tests, add:

#[test]
fn registry_contains_opencode_adapter() {
    let adapters = registered_adapters();
    assert_eq!(adapters.len(), 3, "registry should now have three adapters");
    let names: Vec<&str> = adapters.iter().map(|a| a.name()).collect();
    assert!(names.contains(&"opencode"), "registry missing opencode adapter");
}

#[test]
fn opencode_adapter_trait_probes() {
    let adapters = registered_adapters();
    let o = adapters.iter().find(|a| a.name() == "opencode")
        .expect("opencode adapter must be registered");
    assert_eq!(o.binary_name(), "opencode");
    assert!(o.supports_plugins(), "OpencodeAdapter supports plugins");
    assert!(o.supports_lsp(), "OpencodeAdapter supports LSP");
    let events = o.supported_hook_events();
    for expected in ["SessionStart", "SessionEnd", "UserPromptSubmit", "PreToolUse", "PostToolUse", "Stop"] {
        assert!(events.contains(&expected), "supported_hook_events missing {expected}");
    }
    // Explicitly verify Claude-only events are NOT supported
    for claude_only in ["Notification", "SubagentStop", "PreCompact"] {
        assert!(!events.contains(&claude_only), "OpencodeAdapter must not claim support for {claude_only}");
    }
}

#[test]
fn known_engine_ids_includes_opencode() {
    let ids = known_engine_ids();
    assert!(ids.contains(&"opencode".to_string()), "known_engine_ids missing opencode");
}
```

Run: `cargo test engine_id_ opencode_adapter_trait registry_contains_opencode known_engine_ids_includes -- --quiet`
Expected: FAIL (open_code adapter not found, module doesn't compile)

- [ ] **Step 2: Run tests to confirm they fail**

```bash
cargo test --lib engine_id_ opencode_adapter_trait registry_contains_opencode known_engine_ids_includes 2>&1 | tail -5
```

Expected: compilation error(s) — `known_engine_ids()` returns `["claude_code", "crush"]`, registry length is 2.

- [ ] **Step 3: Create `src/adapter/opencode.rs` skeleton**

```rust
use std::path::{Path, PathBuf};

use serde_json::json;

use super::AgentAdapter;
use super::resolve_bundle_relative_paths;
use crate::merge::MergedManifest;
use crate::util::{dedup, merge_json};

/// Adapter for opencode: writes `AGENTS.md` and `opencode.json` into the
/// cache dir and exports `OPENCODE_CONFIG_DIR` so opencode discovers them.
///
/// Skills use the claude-compatible `SKILL.md` format opencode reads natively.
/// Hooks are bridged via a generated `plugin/llmenv.js` shim (§3).
#[derive(Debug, Default, Clone, Copy)]
pub struct OpencodeAdapter;

const OPENCODE_JSON_FILE: &str = "opencode.json";

/// opencode supports exactly these hook events via its JS plugin API.
/// See spec §3 for the event mapping.
const SUPPORTED_HOOK_EVENTS: &[&str] = &[
    "SessionStart",
    "SessionEnd",
    "UserPromptSubmit",
    "PreToolUse",
    "PostToolUse",
    "Stop",
];

impl AgentAdapter for OpencodeAdapter {
    fn name(&self) -> &'static str {
        "opencode"
    }

    fn binary_name(&self) -> &'static str {
        "opencode"
    }

    fn supports_plugins(&self) -> bool {
        true
    }

    fn supports_lsp(&self) -> bool {
        true
    }

    fn supported_hook_events(&self) -> &'static [&'static str] {
        SUPPORTED_HOOK_EVENTS
    }

    fn env_vars(
        &self,
        cache_dir: &Path,
        _state_dir: &Path,
    ) -> anyhow::Result<Vec<(String, String)>> {
        let dir = cache_dir.to_str().ok_or_else(|| {
            anyhow::anyhow!("cache_dir is not valid UTF-8: {}", cache_dir.display())
        })?;
        Ok(vec![("OPENCODE_CONFIG_DIR".into(), dir.to_owned())])
    }

    fn materialize(&self, _manifest: &MergedManifest, _out: &Path) -> anyhow::Result<Vec<PathBuf>> {
        anyhow::bail!("not yet implemented")
    }

    fn emit_hook_context(&self, hook_event_name: &str, text: &str) -> String {
        if text.is_empty() {
            return String::new();
        }
        let wrapped = format!("[ICM MEMORY CONTEXT (auto-injected)]\n{text}");
        serde_json::json!({
            "hookSpecificOutput": {
                "hookEventName": hook_event_name,
                "additionalContext": wrapped
            }
        })
        .to_string()
    }
}
```

- [ ] **Step 4: Register in `src/adapter/mod.rs`**

Add `pub mod opencode;` next to `pub mod crush;` (line 2), and add `Box::new(opencode::OpencodeAdapter)` to `registered_adapters()` after crush (line 118):

```rust
// Line 1-3, change to:
pub mod claude_code;
pub mod crush;
pub mod opencode;

// registered_adapters(), line 116-121, change to:
pub fn registered_adapters() -> Vec<Box<dyn AgentAdapter>> {
    vec![
        Box::new(claude_code::ClaudeCodeAdapter),
        Box::new(crush::CrushAdapter),
        Box::new(opencode::OpencodeAdapter),
    ]
}
```

Add detection arm in `active_adapter()` (line 101-105):

```rust
fn active_adapter() -> Box<dyn AgentAdapter> {
    registered_adapters()
        .into_iter()
        .find(|a| match a.name() {
            "claude-code" => std::env::var("CLAUDE_CONFIG_DIR").is_ok(),
            "crush" => std::env::var("CRUSH_GLOBAL_CONFIG").is_ok(),
            "opencode" => std::env::var("OPENCODE_CONFIG_DIR").is_ok(),
            _ => false,
        })
        .unwrap_or_else(|| Box::new(claude_code::ClaudeCodeAdapter))
}
```

- [ ] **Step 5: Run tests to verify they pass now**

```bash
cargo fmt && cargo test --lib engine_id_ opencode_adapter_trait registry_contains_opencode known_engine_ids_includes -- --quiet
```

Expected: all 6 new tests PASS.

- [ ] **Step 6: Commit**

```bash
git add src/adapter/mod.rs src/adapter/opencode.rs
git commit -m "feat(adapter): register opencode adapter with env_vars and trait probes"
```

---

### Task 2: AGENTS.md + rules materialization

**Files:**
- Modify: `src/adapter/opencode.rs` (implement `materialize()` starting with AGENTS.md + rules)

**Interfaces:**
- Uses: `manifest.agents_md`, `manifest.rules`, `super::skills::create_dir_owner_only`
- Produces: written `AGENTS.md`, `rules/*.md` files; `instructions` list building towards opencode.json

- [ ] **Step 1: Write failing test**

```rust
// In opencode.rs #[cfg(test)] module:

#[test]
fn materialize_empty_manifest_writes_agents_md_and_json() {
    let tmp = tempfile::tempdir().unwrap();
    let manifest = MergedManifest::default();
    let owned = OpencodeAdapter.materialize(&manifest, tmp.path()).unwrap();
    assert!(owned.contains(&PathBuf::from("AGENTS.md")));
    assert!(owned.contains(&PathBuf::from(OPENCODE_JSON_FILE)));
}

#[test]
fn materialize_agents_md_content_is_preserved() {
    let tmp = tempfile::tempdir().unwrap();
    let mut manifest = MergedManifest::default();
    manifest.agents_md = "# Test Rules\n\nSome content here.".to_string();
    OpencodeAdapter.materialize(&manifest, tmp.path()).unwrap();
    let content = std::fs::read_to_string(tmp.path().join("AGENTS.md")).unwrap();
    assert_eq!(content, "# Test Rules\n\nSome content here.");
}

#[test]
fn materialize_rules_copied_and_listed_in_instructions() {
    let tmp = tempfile::tempdir().unwrap();
    let mut manifest = MergedManifest::default();
    manifest.rules = vec![
        (PathBuf::from("rules/security.md"), PathBuf::from("/src/rules/security.md")),
        (PathBuf::from("rules/style.md"), PathBuf::from("/src/rules/style.md")),
    ];
    // Write the source files
    std::fs::create_dir_all("/tmp/test_rules_src").unwrap();
    std::fs::write("/tmp/test_rules_src/security.md", "# Security").unwrap();
    std::fs::write("/tmp/test_rules_src/style.md", "# Style").unwrap();
    // Adjust paths: the MergedManifest uses the materialize-time source, not our temp
    // — we'll test with a proper source path.
    // Actually, use a fresh tempdir for source:
    let src = tempfile::tempdir().unwrap();
    let sec = src.path().join("rules/security.md");
    let sty = src.path().join("rules/style.md");
    std::fs::create_dir_all(src.path().join("rules")).unwrap();
    std::fs::write(&sec, "# Security").unwrap();
    std::fs::write(&sty, "# Style").unwrap();
    let manifest = crate::merge::MergedManifest {
        rules: vec![
            (PathBuf::from("rules/security.md"), sec.clone()),
            (PathBuf::from("rules/style.md"), sty.clone()),
        ],
        ..Default::default()
    };
    let owned = OpencodeAdapter.materialize(&manifest, tmp.path()).unwrap();
    assert!(tmp.path().join("rules/security.md").exists());
    assert!(tmp.path().join("rules/style.md").exists());
    let json_raw = std::fs::read_to_string(tmp.path().join(OPENCODE_JSON_FILE)).unwrap();
    let doc: serde_json::Value = serde_json::from_str(&json_raw).unwrap();
    let instr = doc["instructions"].as_array().unwrap();
    assert!(instr.contains(&serde_json::json!("rules/security.md")));
    assert!(instr.contains(&serde_json::json!("rules/style.md")));
}
```

Run: `cargo test --lib materialize_empty_manifest materialize_agents_md materialize_rules -- --quiet`
Expected: FAIL (materialize bails)

- [ ] **Step 2: Implement AGENTS.md + rules in materialize()**

Replace the stub `materialize()`:

```rust
fn materialize(&self, manifest: &MergedManifest, out: &Path) -> anyhow::Result<Vec<PathBuf>> {
    super::skills::create_dir_owner_only(out)?;

    let mut owned: Vec<PathBuf> = Vec::new();

    // 1. AGENTS.md
    super::skills::reject_hardcoded_config_path(&manifest.agents_md, "AGENTS.md")?;
    crate::paths::write_owner_only(
        &out.join("AGENTS.md"),
        manifest.agents_md.as_bytes(),
    )?;
    owned.push(PathBuf::from("AGENTS.md"));

    // 2. rules/*.md — copied verbatim; paths collected for instructions[]
    let mut instructions: Vec<String> = Vec::new();
    for (rel, abs) in &manifest.rules {
        let dest = out.join(rel);
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::copy(abs, &dest)?;
        instructions.push(rel.to_string_lossy().into_owned());
        owned.push(rel.clone());
    }

    // 3. Build opencode.json with what we have so far
    let mut doc = serde_json::Map::new();
    doc.insert("$schema".into(), json!("https://opencode.ai/config.json"));
    if !instructions.is_empty() {
        doc.insert("instructions".into(), json!(instructions));
    }

    // 4. Write opencode.json
    let json_bytes = serde_json::to_vec_pretty(&doc)?;
    let out_path = out.join(OPENCODE_JSON_FILE);
    crate::paths::write_owner_only(&out_path, &json_bytes)?;
    owned.push(PathBuf::from(OPENCODE_JSON_FILE));

    Ok(owned)
}
```

Note: `reject_hardcoded_config_path` is `pub(crate)` in `adapter/skills.rs`. We need to verify we can call it from `opencode.rs` (same crate). It's already `pub(crate)`, so accessible. The import path is `super::skills::reject_hardcoded_config_path`.

- [ ] **Step 3: Verify tests pass**

```bash
cargo fmt && cargo test --lib materialize_empty_manifest_ materialize_agents_md materialize_rules_copied -- --quiet
```

Expected: PASS.

- [ ] **Step 4: Commit**

```bash
git add src/adapter/opencode.rs
git commit -m "feat(adapter): materialize AGENTS.md and rules for opencode"
```

---

### Task 3: Skills materialization (first-class + plugin-sourced)

**Files:**
- Modify: `src/adapter/opencode.rs` (`materialize()` — add skills section)

**Interfaces:**
- Uses: `crate::adapter::skills::write_first_class_skills(out, &manifest.capabilities.skills) -> Vec<PathBuf>`
- Uses: `crate::adapter::skills::project_plugin_skills(payload_dir, out) -> Vec<PathBuf>`
- Uses: `crate::adapter::skills::validate_skills(out)`
- Uses: `super::resolve_plugin_payload(plugin, &manifest.marketplaces)` — but this is `pub(crate)` in `crush.rs`, not shared. We need to either make it shared or duplicate it.

- [ ] **Step 1: Make `resolve_plugin_payload` a shared helper**

Move `resolve_plugin_payload` from `src/adapter/crush.rs` to `src/adapter/skills.rs` (or `src/adapter/mod.rs`). The crush version is at crush.rs:359–388. Let's make it a `pub(crate) fn` in `mod.rs` alongside `resolve_bundle_relative_paths`:

```rust
// In mod.rs, after the existing resolve_ helpers:

/// Resolve the on-disk payload directory for a plugin.
///
/// Shared across adapters that project plugin skills into the output dir.
/// External plugins (`install_path = Some`) use that path directly.
/// First-party plugins look up their marketplace `install_location`.
pub(crate) fn resolve_plugin_payload(
    plugin: &crate::plugins::resolve::ResolvedPlugin,
    marketplaces: &[crate::plugins::resolve::ResolvedMarketplace],
) -> anyhow::Result<PathBuf> {
    if !crate::paths::is_valid_short_name(&plugin.plugin) {
        anyhow::bail!("plugin name '{}' is not a valid name", plugin.plugin);
    }
    if let Some(p) = &plugin.install_path {
        return Ok(PathBuf::from(p));
    }
    let mkt = marketplaces
        .iter()
        .find(|m| m.name == plugin.marketplace)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "plugin '{}': marketplace '{}' not found in resolved marketplaces",
                plugin.plugin,
                plugin.marketplace
            )
        })?;
    let install_location = mkt.install_location.as_deref().ok_or_else(|| {
        anyhow::anyhow!(
            "plugin '{}': marketplace '{}' has no install_location (not yet synced?)",
            plugin.plugin,
            plugin.marketplace
        )
    })?;
    Ok(PathBuf::from(install_location).join(&plugin.plugin))
}
```

In `crush.rs`, replace the local `resolve_plugin_payload` with `use super::resolve_plugin_payload;` and remove the function definition. Update crush test imports accordingly.

- [ ] **Step 2: Write test for opencode skills materialization**

```rust
#[test]
fn materialize_first_class_skills_written() {
    let tmp = tempfile::tempdir().unwrap();
    let skill_src = tempfile::tempdir().unwrap();
    std::fs::write(
        skill_src.path().join("SKILL.md"),
        "---\nname: my-skill\ndescription: A test skill.\n---\n# MySkill\n",
    ).unwrap();
    let mut caps = Capabilities::default();
    caps.skills.push(crate::config::SkillSource {
        name: "my-skill".into(),
        path: skill_src.path().to_string_lossy().into_owned(),
        when: Vec::new(),
    });
    let manifest = MergedManifest { capabilities: caps, ..Default::default() };
    let owned = OpencodeAdapter.materialize(&manifest, tmp.path()).unwrap();
    assert!(tmp.path().join("skills/my-skill/SKILL.md").exists());
    // Verify owned includes the skill path
    let has_skill = owned.iter().any(|p| p.starts_with("skills/"));
    assert!(has_skill, "owned must include skill paths, got: {owned:?}");
}

#[test]
fn materialize_plugin_skills_projected() {
    let tmp = tempfile::tempdir().unwrap();
    let plugin_dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(plugin_dir.path().join("skills/foo")).unwrap();
    std::fs::write(
        plugin_dir.path().join("skills/foo/SKILL.md"),
        "---\nname: foo\ndescription: A foo skill.\n---\n# Foo\n",
    ).unwrap();
    let mut manifest = MergedManifest::default();
    manifest.plugins.push(crate::plugins::resolve::ResolvedPlugin {
        marketplace: "local".into(),
        plugin: "my-plugin".into(),
        collection: String::new(),
        install_path: Some(plugin_dir.path().to_string_lossy().into_owned()),
        git_commit_sha: None,
    });
    let owned = OpencodeAdapter.materialize(&manifest, tmp.path()).unwrap();
    assert!(tmp.path().join("skills/foo/SKILL.md").exists());
    assert!(owned.iter().any(|p| p.starts_with("skills/")));
}
```

- [ ] **Step 3: Add skills section to materialize()**

After the rules section and before the opencode.json build:

```rust
// 3. First-class skills
let skill_paths =
    crate::adapter::skills::write_first_class_skills(out, &manifest.capabilities.skills)?;
owned.extend(skill_paths.iter().cloned());

// 4. Plugin-projected skills — unlike crush, opencode supports all plugin content,
// so we don't skip plugins with agents/commands/hooks dirs. Skills are projected
// regardless; other content is handled in later tasks.
let mut plugin_skill_paths: Vec<PathBuf> = Vec::new();
for plugin in &manifest.plugins {
    let payload = super::resolve_plugin_payload(plugin, &manifest.marketplaces)?;
    let paths = crate::adapter::skills::project_plugin_skills(&payload, out)?;
    plugin_skill_paths.extend(paths);
}
owned.extend(plugin_skill_paths.iter().cloned());

// 5. Validate skills (frontmatter + hardcoded-path scan)
crate::adapter::skills::validate_skills(out)?;
```

- [ ] **Step 4: Verify tests pass**

```bash
cargo fmt && cargo test --lib materialize_first_class_skills materialize_plugin_skills -- --quiet
```

Check no regressions in crush adapter tests: `cargo test --lib crush_ -- --quiet`

- [ ] **Step 5: Commit**

```bash
git add src/adapter/mod.rs src/adapter/crush.rs src/adapter/opencode.rs
git commit -m "feat(adapter): skills materialization for opencode adapter"
```

---

### Task 4: MCP rendering

**Files:**
- Modify: `src/adapter/opencode.rs` (add MCP to the doc build)

**Interfaces:**
- Uses: `manifest.mcps`, `manifest.capabilities.native_mcp`

Key differences from crush adapter:
- Discriminator: `type: "local"` (not `"stdio"`) / `type: "remote"` (same)
- `command` is `Vec<String>` (combine command + args)
- env key is `"environment"` (not `"env"`)
- No `disabled_tools` field
- Has optional `cwd` field for local servers

- [ ] **Step 1: Write tests**

```rust
#[test]
fn materialize_mcp_local_server_written() {
    let tmp = tempfile::tempdir().unwrap();
    let mut manifest = MergedManifest::default();
    manifest.mcps.push(ResolvedMcp {
        name: "local-srv".into(),
        kind: ResolvedKind::Stdio {
            command: "npx".into(),
            args: vec!["-y".into(), "@anthropic-ai/mcp-server".into()],
            env: std::collections::BTreeMap::from([
                ("FOO".into(), "bar".into()),
            ]),
        },
        headers: std::collections::BTreeMap::new(),
        timeout: Some(10_000),
        disabled_tools: vec![],
    });
    OpencodeAdapter.materialize(&manifest, tmp.path()).unwrap();
    let raw = std::fs::read_to_string(tmp.path().join(OPENCODE_JSON_FILE)).unwrap();
    let doc: serde_json::Value = serde_json::from_str(&raw).unwrap();
    let srv = &doc["mcp"]["local-srv"];
    assert_eq!(srv["type"], json!("local"));
    let cmd = srv["command"].as_array().unwrap();
    assert_eq!(cmd[0], json!("npx"));
    assert_eq!(cmd[1], json!("-y"));
    assert_eq!(srv["environment"]["FOO"], json!("bar"));
    assert_eq!(srv["timeout"], json!(10_000));
}

#[test]
fn materialize_mcp_remote_server_written() {
    let tmp = tempfile::tempdir().unwrap();
    let mut manifest = MergedManifest::default();
    manifest.mcps.push(ResolvedMcp {
        name: "remote-srv".into(),
        kind: ResolvedKind::Remote {
            url: "http://localhost:3000/mcp".into(),
            transport: crate::config::McpTransport::Http,
        },
        headers: std::collections::BTreeMap::from([
            ("Authorization".into(), "Bearer xyz".into()),
        ]),
        timeout: Some(5000),
        disabled_tools: vec![],
    });
    OpencodeAdapter.materialize(&manifest, tmp.path()).unwrap();
    let raw = std::fs::read_to_string(tmp.path().join(OPENCODE_JSON_FILE)).unwrap();
    let doc: serde_json::Value = serde_json::from_str(&raw).unwrap();
    let srv = &doc["mcp"]["remote-srv"];
    assert_eq!(srv["type"], json!("remote"));
    assert_eq!(srv["url"], json!("http://localhost:3000/mcp"));
    assert_eq!(srv["headers"]["Authorization"], json!("Bearer xyz"));
}

#[test]
fn materialize_mcp_optional_fields_omitted() {
    let tmp = tempfile::tempdir().unwrap();
    let mut manifest = MergedManifest::default();
    manifest.mcps.push(ResolvedMcp {
        name: "minimal".into(),
        kind: ResolvedKind::Remote {
            url: "http://example.com".into(),
            transport: crate::config::McpTransport::Http,
        },
        headers: std::collections::BTreeMap::new(),
        timeout: None,
        disabled_tools: vec![],
    });
    OpencodeAdapter.materialize(&manifest, tmp.path()).unwrap();
    let raw = std::fs::read_to_string(tmp.path().join(OPENCODE_JSON_FILE)).unwrap();
    let doc: serde_json::Value = serde_json::from_str(&raw).unwrap();
    let srv = &doc["mcp"]["minimal"];
    assert!(srv.get("headers").is_none());
    assert!(srv.get("timeout").is_none());
    assert!(srv.get("cwd").is_none());
    assert!(srv.get("disabled_tools").is_none(), "opencode has no disabled_tools field");
}
```

- [ ] **Step 2: Add MCP rendering to materialize()**

After the skills section, before the opencode.json build:

```rust
// 6. MCP servers
if !manifest.mcps.is_empty() || manifest.capabilities.native_mcp.contains_key("opencode") {
    let mut mcp_obj = serde_json::Map::new();
    for mcp in &manifest.mcps {
        use crate::mcp::resolve::ResolvedKind;
        let mut e = match &mcp.kind {
            ResolvedKind::Stdio { command, args, env } => {
                let mut cmd: Vec<serde_json::Value> = Vec::with_capacity(1 + args.len());
                cmd.push(json!(command));
                cmd.extend(args.iter().map(|a| json!(a)));
                let mut e = serde_json::Map::new();
                e.insert("type".into(), json!("local"));
                e.insert("command".into(), json!(cmd));
                if !env.is_empty() {
                    e.insert("environment".into(), json!(env));
                }
                e
            }
            ResolvedKind::Remote { url, transport } => {
                let mut e = serde_json::Map::new();
                e.insert("type".into(), json!("remote"));
                e.insert("url".into(), json!(url));
                e
            }
        };
        if !mcp.headers.is_empty() {
            e.insert("headers".into(), json!(mcp.headers));
        }
        if let Some(t) = mcp.timeout {
            e.insert("timeout".into(), json!(t));
        }
        mcp_obj.insert(mcp.name.clone(), serde_json::Value::Object(e));
    }
    // Overlay native_mcp.opencode
    let mut mcp_value = serde_json::Value::Object(mcp_obj);
    super::overlay_native_json(
        &mut mcp_value,
        manifest.capabilities.native_mcp.get("opencode"),
        "native_mcp.opencode",
    )?;
    if !mcp_value.as_object().is_none_or(serde_json::Map::is_empty) {
        doc.insert("mcp".into(), mcp_value);
    }
}
```

Note: `overlay_native_json` is the crush `overlay_native_crush` function generalized — we'll extract it to `mod.rs` in the next task. For now, copy the simple helper inline.

- [ ] **Step 3: Run tests, commit**

```bash
cargo fmt && cargo test --lib materialize_mcp_ -- --quiet
git add src/adapter/opencode.rs
git commit -m "feat(adapter): MCP rendering for opencode adapter"
```

---

### Task 5: LSP rendering

**Files:**
- Modify: `src/adapter/opencode.rs` (add LSP to doc)

Key differences from crush:
- `command` is `Vec<String>` (combine command + args)
- field is `"extensions"` not `"filetypes"`
- field is `"initialization"` not `"init_options"`
- No `root_markers` or `timeout` fields in the v1 schema

- [ ] **Step 1: Write tests**

```rust
#[test]
fn materialize_lsp_server_written() {
    let tmp = tempfile::tempdir().unwrap();
    let mut caps = Capabilities::default();
    caps.lsp.push(llmenv_config::LspServer {
        name: "rust-analyzer".into(),
        command: "rust-analyzer".into(),
        args: vec!["--quiet".into()],
        filetypes: vec!["rust".into()],
        env: std::collections::BTreeMap::from([("RUST_LOG".into(), "info".into())]),
        timeout: Some(60),
        ..Default::default()
    });
    let manifest = MergedManifest { capabilities: caps, ..Default::default() };
    OpencodeAdapter.materialize(&manifest, tmp.path()).unwrap();
    let raw = std::fs::read_to_string(tmp.path().join(OPENCODE_JSON_FILE)).unwrap();
    let doc: serde_json::Value = serde_json::from_str(&raw).unwrap();
    let srv = &doc["lsp"]["rust-analyzer"];
    let cmd = srv["command"].as_array().unwrap();
    assert_eq!(cmd[0], json!("rust-analyzer"));
    assert_eq!(cmd[1], json!("--quiet"));
    assert_eq!(srv["env"]["RUST_LOG"], json!("info"));
    let exts = srv["extensions"].as_array().unwrap();
    assert!(exts.contains(&json!("rust")));
}

#[test]
fn materialize_lsp_with_init_options() {
    let tmp = tempfile::tempdir().unwrap();
    let mut caps = Capabilities::default();
    caps.lsp.push(llmenv_config::LspServer {
        name: "rust-analyzer".into(),
        command: "rust-analyzer".into(),
        init_options: Some(serde_yaml::from_str("checkOnSave: true").unwrap()),
        ..Default::default()
    });
    let manifest = MergedManifest { capabilities: caps, ..Default::default() };
    OpencodeAdapter.materialize(&manifest, tmp.path()).unwrap();
    let raw = std::fs::read_to_string(tmp.path().join(OPENCODE_JSON_FILE)).unwrap();
    let doc: serde_json::Value = serde_json::from_str(&raw).unwrap();
    assert_eq!(doc["lsp"]["rust-analyzer"]["initialization"]["checkOnSave"], json!(true));
    assert!(doc["lsp"]["rust-analyzer"].get("initializationOptions").is_none(), "must use opencode's 'initialization' key");
}

#[test]
fn materialize_lsp_empty_omitted() {
    let tmp = tempfile::tempdir().unwrap();
    OpencodeAdapter.materialize(&MergedManifest::default(), tmp.path()).unwrap();
    let raw = std::fs::read_to_string(tmp.path().join(OPENCODE_JSON_FILE)).unwrap();
    let doc: serde_json::Value = serde_json::from_str(&raw).unwrap();
    assert!(doc.get("lsp").is_none());
}

#[test]
fn materialize_lsp_disabled_server_omitted() {
    let tmp = tempfile::tempdir().unwrap();
    let mut caps = Capabilities::default();
    caps.lsp.push(llmenv_config::LspServer {
        name: "disabled-srv".into(),
        command: "some-ls".into(),
        disabled: true,
        ..Default::default()
    });
    let manifest = MergedManifest { capabilities: caps, ..Default::default() };
    OpencodeAdapter.materialize(&manifest, tmp.path()).unwrap();
    let raw = std::fs::read_to_string(tmp.path().join(OPENCODE_JSON_FILE)).unwrap();
    let doc: serde_json::Value = serde_json::from_str(&raw).unwrap();
    assert!(doc.get("lsp").is_none());
}
```

- [ ] **Step 2: Add LSP rendering to materialize()**

```rust
// 7. LSP servers
if !manifest.capabilities.lsp.is_empty() {
    let mut lsp_obj = serde_json::Map::new();
    for srv in &manifest.capabilities.lsp {
        if srv.disabled {
            continue;
        }
        let mut cmd: Vec<serde_json::Value> = Vec::with_capacity(1 + srv.args.len());
        cmd.push(json!(srv.command));
        cmd.extend(srv.args.iter().map(|a| json!(a)));
        let mut e = serde_json::Map::new();
        e.insert("command".into(), json!(cmd));
        if !srv.filetypes.is_empty() {
            e.insert("extensions".into(), json!(srv.filetypes));
        }
        if !srv.env.is_empty() {
            e.insert("env".into(), json!(srv.env));
        }
        if let Some(opts) = &srv.init_options {
            let as_json = serde_json::to_value(opts).map_err(|err| {
                anyhow::anyhow!(
                    "LSP server '{}': failed to convert init_options to JSON: {err}",
                    srv.name
                )
            })?;
            e.insert("initialization".into(), as_json);
        }
        lsp_obj.insert(srv.name.clone(), serde_json::Value::Object(e));
    }
    if !lsp_obj.is_empty() {
        doc.insert("lsp".into(), serde_json::Value::Object(lsp_obj));
    }
}
```

- [ ] **Step 3: Run tests, commit**

```bash
cargo fmt && cargo test --lib materialize_lsp_ -- --quiet
git add src/adapter/opencode.rs
git commit -m "feat(adapter): LSP rendering for opencode adapter"
```

---

### Task 6: Permissions rendering + native overlay + modeled-key rejection

**Files:**
- Modify: `src/adapter/opencode.rs` (add permissions, native overlay, modeled-key rejection)

**Interfaces:**
- Uses: `manifest.capabilities.permissions`, `manifest.capabilities.native_permissions`, `manifest.native`
- Needs: shared `overlay_native_json` helper (generalize crush's `overlay_native_crush` in mod.rs)

- [ ] **Step 1: Extract shared native-overlay helpers to `mod.rs`**

Move `overlay_native_crush` from crush.rs to `pub(crate) fn overlay_native_json` in mod.rs, and rename crush's calls. Also make `reject_modeled_keys` engine-generic:

```rust
// In mod.rs, after merge_json import:

/// Overlay a native-engine JSON fragment onto a JSON value, converting from YAML.
/// Used by every adapter for `native.<engine>`, `native_mcp.<engine>`, `native_hooks.<engine>`.
pub(crate) fn overlay_native_json(
    dst: &mut serde_json::Value,
    fragment: Option<&serde_yaml::Value>,
    label: &str,
) -> anyhow::Result<()> {
    if let Some(frag) = fragment {
        let as_json = serde_json::to_value(frag).map_err(|e| {
            anyhow::anyhow!("converting {label} fragment to JSON: {e}")
        })?;
        merge_json(dst, as_json);
    }
    Ok(())
}

/// Reject native-engine fragments that carry modeled-feature keys, preventing
/// accidental clobbering of security-rendered output.
pub(crate) fn reject_modeled_native_keys(
    fragment: &serde_yaml::Value,
    modeled_keys: &[&str],
    engine: &str,
) -> anyhow::Result<()> {
    let Some(map) = fragment.as_mapping() else {
        return Ok(());
    };
    for key in modeled_keys {
        if map.contains_key(serde_yaml::Value::String((*key).into())) {
            anyhow::bail!(
                "top-level `native.{engine}` carries the modeled-feature key `{key}`, \
                 which would silently clobber the rendered `{key}`. \
                 Use `native_{key}.{engine}` (or `native_permissions.{engine}` / \
                 `native_hooks.{engine}` / `native_mcp.{engine}`) instead, \
                 which merges in the safe direction."
            );
        }
    }
    Ok(())
}
```

Update `crush.rs` to use `super::overlay_native_json` and `super::reject_modeled_native_keys` with the crush modeled key list. Remove the duplicated functions. Update crush test imports.

- [ ] **Step 2: Write permissions tests for opencode**

```rust
#[test]
fn materialize_permissions_allow_rule_written() {
    let tmp = tempfile::tempdir().unwrap();
    let mut caps = Capabilities::default();
    caps.permissions.allow.push(PermissionRule {
        tool: "Bash".into(),
        pattern: None,
        paths: vec![],
    });
    let manifest = MergedManifest { capabilities: caps, ..Default::default() };
    OpencodeAdapter.materialize(&manifest, tmp.path()).unwrap();
    let raw = std::fs::read_to_string(tmp.path().join(OPENCODE_JSON_FILE)).unwrap();
    let doc: serde_json::Value = serde_json::from_str(&raw).unwrap();
    // opencode uses `allow` / `ask` / `deny` as arrays of tool names
    let allow = doc["permission"]["allow"].as_array().unwrap();
    assert!(allow.contains(&json!("Bash")));
}

#[test]
fn materialize_permissions_empty_when_no_rules() {
    let tmp = tempfile::tempdir().unwrap();
    OpencodeAdapter.materialize(&MergedManifest::default(), tmp.path()).unwrap();
    let raw = std::fs::read_to_string(tmp.path().join(OPENCODE_JSON_FILE)).unwrap();
    let doc: serde_json::Value = serde_json::from_str(&raw).unwrap();
    assert!(doc.get("permission").is_none());
}

#[test]
fn materialize_native_opencode_merged() {
    let tmp = tempfile::tempdir().unwrap();
    let mut caps = Capabilities::default();
    caps.native_permissions.insert(
        "opencode".into(),
        NativePermissionRules { allow: vec!["Bash(echo*)".into()], ask: vec![], deny: vec![] },
    );
    let manifest = MergedManifest { capabilities: caps, ..Default::default() };
    OpencodeAdapter.materialize(&manifest, tmp.path()).unwrap();
    let raw = std::fs::read_to_string(tmp.path().join(OPENCODE_JSON_FILE)).unwrap();
    let doc: serde_json::Value = serde_json::from_str(&raw).unwrap();
    let allow = doc["permission"]["allow"].as_array().unwrap();
    assert!(allow.contains(&json!("Bash(echo*)")));
}

#[test]
fn materialize_native_opencode_rejects_modeled_keys() {
    let tmp = tempfile::tempdir().unwrap();
    let mut manifest = MergedManifest::default();
    let frag: serde_yaml::Value =
        serde_yaml::from_str("permission:\n  allow: [Bash]\n").unwrap();
    manifest.native.insert("opencode".into(), frag);
    let err = OpencodeAdapter.materialize(&manifest, tmp.path()).unwrap_err();
    assert!(err.to_string().contains("permission"), "error must name offending key: {err}");
    assert!(err.to_string().contains("native_permissions"), "must point at correct channel");
}
```

- [ ] **Step 3: Add permissions + native overlay to materialize()**

```rust
// 8. Permissions — opencode uses `permission` (singular), with `allow`/`ask`/`deny` keys
let perms = &manifest.capabilities.permissions;
let native_perms = manifest.capabilities.native_permissions.get("opencode");

let render_rules = |rules: &[crate::config::PermissionRule]| -> Vec<String> {
    rules.iter().flat_map(|r| {
        if let Some(pat) = &r.pattern {
            vec![format!("{}({})", r.tool, pat)]
        } else if !r.paths.is_empty() {
            r.paths.iter().map(|p| format!("{}({})", r.tool, p)).collect()
        } else {
            vec![r.tool.clone()]
        }
    }).collect()
};

let allowed = {
    let mut v = render_rules(&perms.allow);
    if let Some(n) = native_perms { v.extend(n.allow.iter().cloned()); }
    dedup(&mut v);
    v
};
let ask = {
    let mut v = render_rules(&perms.ask);
    if let Some(n) = native_perms { v.extend(n.ask.iter().cloned()); }
    dedup(&mut v);
    v
};
let deny = {
    let mut v = render_rules(&perms.deny);
    if let Some(n) = native_perms { v.extend(n.deny.iter().cloned()); }
    dedup(&mut v);
    v
};

if !allowed.is_empty() || !ask.is_empty() || !deny.is_empty() {
    let mut perm_obj = serde_json::Map::new();
    if !allowed.is_empty() { perm_obj.insert("allow".into(), json!(allowed)); }
    if !ask.is_empty() { perm_obj.insert("ask".into(), json!(ask)); }
    if !deny.is_empty() { perm_obj.insert("deny".into(), json!(deny)); }
    doc.insert("permission".into(), serde_json::Value::Object(perm_obj));
}

// 9. Native overlay — reject modeled keys, then deep-merge
const OPENCODE_MODELED_KEYS: &[&str] = &["instructions", "mcp", "lsp", "permission"];
if let Some(native) = manifest.native.get("opencode") {
    super::reject_modeled_native_keys(native, OPENCODE_MODELED_KEYS, "opencode")?;
}
let mut doc_value = serde_json::Value::Object(doc);
super::overlay_native_json(
    &mut doc_value,
    manifest.native.get("opencode"),
    "native.opencode",
)?;
let json_bytes = serde_json::to_vec_pretty(&doc_value)?;
crate::paths::write_owner_only(&out_path, &json_bytes)?;
// owned.push(OPENCODE_JSON_FILE) already above
```

- [ ] **Step 4: Run tests, commit**

```bash
cargo fmt && cargo test --lib materialize_permissions_ materialize_native_opencode -- --quiet
# Also verify crush tests still pass:
cargo test --lib crush_ -- --quiet
git add src/adapter/mod.rs src/adapter/crush.rs src/adapter/opencode.rs
git commit -m "feat(adapter): permissions and native overlay for opencode adapter"
```

---

### Task 7: Hook bridge — shim plugin generation

**Files:**
- Modify: `src/adapter/opencode.rs` (add generate_shim_js function, hook filtering, auto-hooks)

**This is the largest and most novel task.** The shim is a self-contained ES module written as a Rust string template with `${HOOK_TABLE}` and `${AUTO_COMMANDS}` placeholders populated at render time.

- [ ] **Step 1: Design the shim JS template (inlined as Rust const string)**

The template lives as a `const SHIM_TEMPLATE: &str` in opencode.rs. At render time, we build a JSON table of hooks and substitute it in:

```rust
/// Template for plugin/llmenv.js — a self-contained ES module that bridges
/// opencode's JS plugin API → llmenv hook-run subprocess calls.
/// `${HOOK_TABLE}` is replaced at render time with a JSON array of
/// `{ event, opencode, commands: [{command, timeout}] }` entries.
const SHIM_TEMPLATE: &str = r#"// llmenv hook bridge for opencode — auto-generated, do not edit.
const HOOK_TABLE = ${HOOK_TABLE};

let sessionContext = null;
let sessionStartFired = false;

export default {
  id: "llmenv-hooks",
  name: "llmenv",
  dispose() {
    if (sessionStartFired) {
      runHooks("SessionEnd", null);
    }
  },
  async event(input) {
    const event = input.event;
    if (event.event === "session.created") {
      // SessionStart: collect context, inject on first chat.message
      sessionContext = await runHooks("SessionStart", null);
      sessionStartFired = true;
    } else if (event.event === "session.idle") {
      // Stop: fire-and-forget
      runHooks("Stop", null);
    } else if (event.event === "session.deleted") {
      // SessionEnd: fire-and-forget (also inside dispose above)
      runHooks("SessionEnd", null);
    }
  },
  async "chat.message"(input, output) {
    // UserPromptSubmit: inject context
    const ctx = await runHooks("UserPromptSubmit", null);
    if (ctx) {
      if (output.message && output.message.content) {
        if (Array.isArray(output.message.content)) {
          output.message.content.push({ type: "text", text: ctx });
        }
      }
    }
    // Also inject SessionStart context on the first message
    if (sessionContext && !sessionStartFired === false) {
      // SessionStart context: already stored, inject on first non-system message
      if (output.message && output.message.content && Array.isArray(output.message.content)) {
        output.message.content.push({ type: "text", text: `Additional context: ${sessionContext}` });
      }
      sessionStartFired = true;
    }
  },
  async "tool.execute.before"(input, output) {
    const ctx = await runHooks("PreToolUse", {
      tool_name: input.tool,
      tool_input: JSON.stringify(input.output?.args || {}),
    });
    if (ctx === "__LLMENV_BLOCK__") {
      throw new Error("Blocked by llmenv hook");
    }
  },
  async "tool.execute.after"(input, output) {
    // PostToolUse: fire-and-forget
    runHooks("PostToolUse", {
      tool_name: input.tool,
      tool_input: JSON.stringify(input.args || {}),
    });
  },
};

async function runHooks(event, extra) {
  const entries = HOOK_TABLE.filter(e => e.event === event);
  let collected = "";
  for (const entry of entries) {
    for (const hk of entry.commands) {
      try {
        const result = await spawnHook(event, hk.command, hk.timeout, extra);
        if (result.blocked) {
          if (event === "PreToolUse") return "__LLMENV_BLOCK__";
          continue;
        }
        if (result.stdout) {
          // Try to unwrap Claude's hookSpecificOutput shape
          try {
            const parsed = JSON.parse(result.stdout);
            if (parsed?.hookSpecificOutput?.additionalContext) {
              collected += parsed.hookSpecificOutput.additionalContext + "\n";
            } else {
              collected += result.stdout + "\n";
            }
          } catch {
            collected += result.stdout + "\n";
          }
        }
      } catch (e) {
        console.error(`llmenv hook ${event} failed:`, e);
      }
    }
  }
  return collected || null;
}

async function spawnHook(event, command, timeoutMs, extra) {
  // Build a Claude-compatible payload on stdin
  const payload = {
    hook_event_name: event,
    session_id: process.env.OPENCODE_SESSION_ID || "",
    cwd: process.cwd(),
    ...(extra || {}),
  };
  const { spawn } = await import("node:child_process");
  return new Promise((resolve) => {
    const child = spawn("sh", ["-c", command], {
      stdio: ["pipe", "pipe", "pipe"],
      timeout: timeoutMs || 30000,
    });
    let stdout = "";
    let stderr = "";
    child.stdout.on("data", (d) => { stdout += d.toString(); });
    child.stderr.on("data", (d) => { stderr += d.toString(); });
    child.on("close", (code) => {
      resolve({
        blocked: code === 2 && event === "PreToolUse",
        stdout: stdout.trim() || null,
        stderr: stderr.trim() || null,
      });
    });
    child.stdin.write(JSON.stringify(payload));
    child.stdin.end();
  });
}
"#;
```

- [ ] **Step 2: Write the shim generation function and hook filtering in materialize()**

```rust
fn generate_shim_js(
    hooks: &[&crate::config::Hook],
) -> String {
    use crate::config::HookHandlerKind;

    // Group hooks by event
    let mut by_event: std::collections::BTreeMap<&str, Vec<serde_json::Value>> =
        std::collections::BTreeMap::new();
    for hook in hooks {
        if matches!(hook.handler.kind, HookHandlerKind::McpTool) {
            eprintln!(
                "warning: opencode adapter does not support mcp_tool hooks \
                 (hook event '{}', tool '{}') — skipping this hook. \
                 Use a command hook instead.",
                hook.event,
                hook.handler.tool.as_deref().unwrap_or("<unknown>")
            );
            continue;
        }
        let resolved_command = hook
            .handler
            .command
            .as_deref()
            .map(|cmd| {
                match &hook.bundle_origin {
                    Some(bundle_dir) => resolve_bundle_relative_paths(cmd, bundle_dir)
                        .unwrap_or_else(|| cmd.to_string()),
                    None => cmd.to_string(),
                }
            })
            .unwrap_or_default();
        let timeout = 30_000u64; // ponytail: default 30s, same as claude adapter
        by_event
            .entry(hook.event.as_str())
            .or_default()
            .push(json!({ "command": resolved_command, "timeout": timeout }));
    }

    // Add auto-hooks (parity with claude adapter's auto-emitted hooks)
    // SessionStart: stale check + config context
    by_event
        .entry("SessionStart")
        .or_default()
        .push(json!({
            "command": "llmenv check-stale --engine opencode",
            "timeout": 5000,
        }));
    by_event
        .entry("SessionStart")
        .or_default()
        .push(json!({
            "command": "llmenv config-context --engine opencode",
            "timeout": 5000,
        }));
    // PreToolUse: cache-write guard
    by_event
        .entry("PreToolUse")
        .or_default()
        .push(json!({
            "command": "llmenv config-guard --engine opencode",
            "timeout": 5000,
        }));

    // Build the hook table JSON
    let table: Vec<serde_json::Value> = by_event
        .into_iter()
        .map(|(event, commands)| {
            let opencode_event = match event {
                "SessionStart" => "session.created",
                "SessionEnd" => "session.deleted",
                "UserPromptSubmit" => "chat.message",
                "PreToolUse" => "tool.execute.before",
                "PostToolUse" => "tool.execute.after",
                "Stop" => "session.idle",
                _ => event, // unreachable — filtered earlier
            };
            json!({
                "event": event,
                "opencode": opencode_event,
                "commands": commands,
            })
        })
        .collect();

    let table_json = serde_json::to_string(&table).expect("hook table must be valid JSON");
    SHIM_TEMPLATE.replace("${HOOK_TABLE}", &table_json)
}
```

In `materialize()`, add hook filtering + shim generation after permissions and before the JSON write:

```rust
// 10. Hook shim — filter supported events, warn-and-skip others
let compatible_hooks: Vec<&crate::config::Hook> = manifest
    .capabilities
    .hooks
    .iter()
    .filter(|hook| {
        if !SUPPORTED_HOOK_EVENTS.contains(&hook.event.as_str()) {
            eprintln!(
                "warning: opencode adapter does not support hook event '{}' — \
                 skipping this hook. Supported events: {}. Remove or move \
                 this hook to a claude_code-only bundle to silence this warning.",
                hook.event,
                SUPPORTED_HOOK_EVENTS.join(", ")
            );
            return false;
        }
        if matches!(hook.handler.kind, crate::config::HookHandlerKind::McpTool) {
            eprintln!(
                "warning: opencode adapter does not support mcp_tool hooks \
                 (hook event '{}', tool '{}') — skipping this hook. \
                 Use a command hook instead.",
                hook.event,
                hook.handler.tool.as_deref().unwrap_or("<unknown>")
            );
            return false;
        }
        true
    })
    .collect();

let needs_shim = !compatible_hooks.is_empty(); // auto-hooks always add entries, so always true when hooks or auto-hooks exist
if needs_shim || true {
    // Always emit shim for auto-hooks at minimum
    let shim_js = generate_shim_js(&compatible_hooks);
    let plugin_dir = out.join("plugin");
    std::fs::create_dir_all(&plugin_dir)?;
    crate::paths::write_owner_only(&plugin_dir.join("llmenv.js"), shim_js.as_bytes())?;
    owned.push(PathBuf::from("plugin/llmenv.js"));
}
```

- [ ] **Step 3: Write tests**

```rust
#[test]
fn materialize_hook_unsupported_event_skipped() {
    let tmp = tempfile::tempdir().unwrap();
    let mut caps = Capabilities::default();
    caps.hooks.push(Hook {
        event: "Notification".into(),
        matcher: None,
        handler: HookHandler { kind: HookHandlerKind::Command, command: Some("echo n".into()), tool: None },
        bundle_origin: None,
    });
    let manifest = MergedManifest { capabilities: caps, ..Default::default() };
    OpencodeAdapter.materialize(&manifest, tmp.path()).unwrap();
    // Shim still emitted (auto-hooks), but the table should NOT contain the skipped hook
    let shim_src = std::fs::read_to_string(tmp.path().join("plugin/llmenv.js")).unwrap();
    assert!(!shim_src.contains("\"event\":\"Notification\""));
}

#[test]
fn materialize_shim_contains_auto_hooks() {
    let tmp = tempfile::tempdir().unwrap();
    OpencodeAdapter.materialize(&MergedManifest::default(), tmp.path()).unwrap();
    let shim_src = std::fs::read_to_string(tmp.path().join("plugin/llmenv.js")).unwrap();
    assert!(shim_src.contains("check-stale --engine opencode"));
    assert!(shim_src.contains("config-context --engine opencode"));
    assert!(shim_src.contains("config-guard --engine opencode"));
}

#[test]
fn materialize_hook_with_supported_event_rendered_in_shim() {
    let tmp = tempfile::tempdir().unwrap();
    let mut caps = Capabilities::default();
    caps.hooks.push(Hook {
        event: "PreToolUse".into(),
        matcher: None,
        handler: HookHandler { kind: HookHandlerKind::Command, command: Some("echo hi".into()), tool: None },
        bundle_origin: None,
    });
    let manifest = MergedManifest { capabilities: caps, ..Default::default() };
    OpencodeAdapter.materialize(&manifest, tmp.path()).unwrap();
    let shim_src = std::fs::read_to_string(tmp.path().join("plugin/llmenv.js")).unwrap();
    assert!(shim_src.contains("echo hi"));
}

#[test]
fn materialize_no_hooks_still_emits_shim_for_auto_hooks() {
    let tmp = tempfile::tempdir().unwrap();
    OpencodeAdapter.materialize(&MergedManifest::default(), tmp.path()).unwrap();
    assert!(tmp.path().join("plugin/llmenv.js").exists());
}
```

- [ ] **Step 4: Handle `emit_hook_context` for opencode**

Already implemented in the skeleton (Task 1) — clones the crush adapter's format since the shim accepts Claude's `hookSpecificOutput` JSON. Verify with existing test.

- [ ] **Step 5: Run tests, commit**

```bash
cargo fmt && cargo test --lib materialize_hook_ materialize_shim_ -- --quiet
git add src/adapter/opencode.rs
git commit -m "feat(adapter): hook bridge shim plugin for opencode adapter"
```

---

### Task 8: Plugin content translation (commands, agents, MCP from plugins)

**Files:**
- Modify: `src/adapter/opencode.rs` (add plugin content translation to materialize())

This task implements the "content translation" layer: for each resolved plugin, extract commands/agents/MCP beyond just skills, and translate them into opencode-native formats.

- [ ] **Step 1: Write frontmatter translation helpers**

```rust
/// Translate a Claude command frontmatter to opencode command frontmatter.
/// Returns (description, template_body) — the fields kept from Claude.
fn translate_command_md(source: &str, name: &str) -> anyhow::Result<String> {
    // Minimal parser: split on --- blocks
    let parts: Vec<&str> = source.splitn(3, "---").collect();
    if parts.len() < 3 {
        // No frontmatter — just a body. Pass through as template.
        return Ok(source.to_string());
    }
    let fm_str = parts[1];
    let body = parts[2];
    let mut description = String::new();
    let mut warnings: Vec<String> = Vec::new();

    for line in fm_str.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("description:") {
            description = trimmed.strip_prefix("description:").unwrap().trim().to_string();
        } else if trimmed.starts_with("model:") || trimmed.starts_with("allowed-tools:") {
            let key = trimmed.split_once(':').map(|(k, _)| k.trim()).unwrap_or(trimmed);
            warnings.push(format!(
                "warning: command '{name}': Claude field '{key}' has no opencode equivalent — dropped"
            ));
        }
        // argument-hint is dropped silently (no warning needed — $ARGUMENTS works in both)
    }

    for w in &warnings {
        eprintln!("{w}");
    }

    let mut out = String::new();
    out.push_str("---\n");
    if !description.is_empty() {
        out.push_str(&format!("description: {description}\n"));
    }
    out.push_str("---\n");
    out.push_str(body);
    Ok(out)
}

/// Translate a Claude agent frontmatter to opencode agent frontmatter.
fn translate_agent_md(source: &str, name: &str) -> anyhow::Result<String> {
    let parts: Vec<&str> = source.splitn(3, "---").collect();
    if parts.len() < 3 {
        return Ok(source.to_string());
    }
    let fm_str = parts[1];
    let body = parts[2];
    let mut description = String::new();
    let mut model = String::new();
    let mut tools: Vec<String> = Vec::new();

    for line in fm_str.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("description:") {
            description = trimmed.strip_prefix("description:").unwrap().trim().to_string();
        } else if trimmed.starts_with("model:") {
            model = trimmed.strip_prefix("model:").unwrap().trim().to_string();
        } else if trimmed.starts_with("tools:") {
            let raw = trimmed.strip_prefix("tools:").unwrap().trim();
            for t in raw.split(',') {
                let tool = t.trim();
                if !tool.is_empty() {
                    tools.push(tool.to_string());
                }
            }
        }
        // color is dropped silently
    }

    if !model.is_empty() {
        eprintln!(
            "warning: agent '{name}': model field kept, verify it matches an opencode provider. \
             Set a model in opencode.json provider config if not."
        );
    }

    let mut out = String::new();
    out.push_str("---\n");
    if !description.is_empty() {
        out.push_str(&format!("description: {description}\n"));
    }
    if !model.is_empty() {
        out.push_str(&format!("model: {model}\n"));
    }
    out.push_str("mode: subagent\n");
    if !tools.is_empty() {
        let tools_entries: Vec<String> = tools.iter().map(|t| format!("{t}: true")).collect();
        out.push_str(&format!("tools:\n{}\n", tools_entries.iter().map(|s| format!("  {s}")).collect::<Vec<_>>().join("\n")));
    }
    out.push_str("---\n");
    out.push_str(body);
    Ok(out)
}
```

- [ ] **Step 2: Write tests for frontmatter translation**

```rust
#[test]
fn translate_command_strips_allowed_tools_and_keeps_description() {
    let src = "---\ndescription: Run tests\nallowed-tools: [Bash, Read]\n---\nnpm test\n";
    let result = super::translate_command_md(src, "test-cmd").unwrap();
    assert!(result.contains("description: Run tests"));
    assert!(!result.contains("allowed-tools"));
    assert!(result.contains("npm test"));
}

#[test]
fn translate_agent_adds_subagent_mode() {
    let src = "---\ndescription: A helper agent\ntools: Bash, Read\n---\n# Agent\n";
    let result = super::translate_agent_md(src, "helper").unwrap();
    assert!(result.contains("mode: subagent"));
    assert!(result.contains("description: A helper agent"));
    assert!(result.contains("tools:"));
    assert!(result.contains("Bash: true"));
    assert!(result.contains("Read: true"));
}
```

- [ ] **Step 3: Add plugin content translation to materialize()**

```rust
// 11. Plugin content translation (commands, agents, MCP)
// This extends the plugin loop (already iterating for skills in step 4).
// Replace the simple plugin loop with:
for plugin in &manifest.plugins {
    let payload = super::resolve_plugin_payload(plugin, &manifest.marketplaces)?;

    // Plugins with LLM_PROVIDER_MCP_JSON provide it as an MCP server.
    let mcp_json_path = payload.join("LLM_PROVIDER_MCP_JSON");
    if mcp_json_path.is_file() {
        let mcp_raw = std::fs::read_to_string(&mcp_json_path)?;
        let mcp_val: serde_json::Value = serde_json::from_str(&mcp_raw)
            .map_err(|e| anyhow::anyhow!("plugin '{}': invalid MCP JSON: {e}", plugin.plugin))?;
        // Add to pending MCP servers — merge after loop
        if let Some(obj) = mcp_val.as_object() {
            for (k, v) in obj {
                mcp_entries.insert(k.clone(), v.clone());
            }
        }
        owned.push(PathBuf::from(
            payload
                .strip_prefix(manifest.plugin_cache_root)
                .unwrap_or(&mcp_json_path)
                .to_string_lossy()
                .into_owned(),
        ));
    }

    // Translate commands/
    let cmds_dir = payload.join("commands");
    if cmds_dir.is_dir() {
        let out_cmd_dir = out.join("command");
        std::fs::create_dir_all(&out_cmd_dir)?;
        for entry in std::fs::read_dir(&cmds_dir)? {
            let entry = entry?;
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            if name_str.ends_with(".md") {
                let src = std::fs::read_to_string(entry.path())?;
                let translated = translate_command_md(&src, &name_str)?;
                let dest = out_cmd_dir.join(format!("{}__{}", &plugin.plugin, name_str));
                crate::paths::write_owner_only(&dest, translated.as_bytes())?;
                owned.push(PathBuf::from(format!("command/{}__{}", plugin.plugin, name_str)));
            }
        }
    }

    // Translate agents/
    let agents_dir = payload.join("agents");
    if agents_dir.is_dir() {
        let out_agent_dir = out.join("agent");
        std::fs::create_dir_all(&out_agent_dir)?;
        for entry in std::fs::read_dir(&agents_dir)? {
            let entry = entry?;
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            if name_str.ends_with(".md") {
                let src = std::fs::read_to_string(entry.path())?;
                let translated = translate_agent_md(&src, &name_str)?;
                let dest = out_agent_dir.join(format!("{}__{}", &plugin.plugin, name_str));
                crate::paths::write_owner_only(&dest, translated.as_bytes())?;
                owned.push(PathBuf::from(format!("agent/{}__{}", plugin.plugin, name_str)));
            }
        }
    }

    // Skills (existing logic)
    let paths = crate::adapter::skills::project_plugin_skills(&payload, out)?;
    plugin_skill_paths.extend(paths);
    // Log hooks dir exists but we skip it
    if payload.join("hooks").is_dir() {
        eprintln!(
            "warning: plugin '{}' contains hooks/ directory — opencode adapter \
             does not support Claude-style plugin hooks. Only llmenv-managed hooks \
             (via the hook shim) are bridged. Remove hooks/ or scope this plugin \
             to a claude_code-only bundle.",
            plugin.plugin
        );
    }
}
```

- [ ] **Step 4: Handle bundle-level commands/agents files (from merged manifest)**

The manifest's files may include `commands/` and `agents/` directories from bundle sources. Check `manifest.rules` or a separate manifest field. Looking at MergedManifest... it has `rules: Vec<(PathBuf, PathBuf)>` (rel, abs source). The rules list is already being copied. Bundles contribute `commands/` and `agents/` as regular files in the merged file tree — they end up in `manifest.copied_files` or `manifest.merged_files`. We should check the `MergedManifest` struct to see if there's a dedicated field:

```rust
// From merge module — check if there's a dedicated commands/agents field or if
// we need to scan merged_files for paths starting with "commands/" or "agents/"
```

Actually, looking at how Claude renders them — they use the manifest's `copied_files` iterator. For simplicity, we can add a helper that scans the merged file tree:

```rust
// pony: scan merged file list for commands/ and agents/ from bundles.
// The MergedManifest doesn't have a dedicated commands/agents field;
// these come through as regular copied files. The claude adapter copies
// them verbatim; we translate them.
for (rel, _abs) in &manifest.copied_files {
    let rel_str = rel.to_string_lossy();
    if rel_str.starts_with("commands/") && rel_str.ends_with(".md") {
        let src_content = std::fs::read_to_string(out.join(rel))?;
        let name = rel_str.strip_prefix("commands/").unwrap();
        let translated = translate_command_md(&src_content, name)?;
        let dest = out.join("command").join(name);
        crate::paths::write_owner_only(&dest, translated.as_bytes())?;
        owned.push(PathBuf::from(format!("command/{name}")));
        // Remove the original from owned (it was added by the generic copy pass)
        owned.retain(|p| p != rel);
    }
    if rel_str.starts_with("agents/") && rel_str.ends_with(".md") {
        let src_content = std::fs::read_to_string(out.join(rel))?;
        let name = rel_str.strip_prefix("agents/").unwrap();
        let translated = translate_agent_md(&src_content, name)?;
        let dest = out.join("agent").join(name);
        crate::paths::write_owner_only(&dest, translated.as_bytes())?;
        owned.push(PathBuf::from(format!("agent/{name}")));
        owned.retain(|p| p != rel);
    }
}
```

- [ ] **Step 5: Run tests, commit**

```bash
cargo fmt && cargo test --lib translate_ -- --quiet
git add src/adapter/opencode.rs
git commit -m "feat(adapter): plugin content translation for opencode adapter"
```

---

### Task 9: Integration tests + CHANGELOG + final cleanup

**Files:**
- Modify: `src/adapter/opencode.rs` (add full-config integration test)
- Modify: `CHANGELOG.md`
- Read: `RELEASING.md` (for versioning rules)

- [ ] **Step 1: Write full-config integration test (like crush's `materialize_full_config_*`)**

```rust
#[test]
fn materialize_full_config_matches_opencode_schema() {
    let tmp = tempfile::tempdir().unwrap();
    let mut caps = Capabilities::default();
    // Skills
    let skill_src = tempfile::tempdir().unwrap();
    std::fs::write(
        skill_src.path().join("SKILL.md"),
        "---\nname: test-skill\ndescription: A test skill.\n---\n# Skill\n",
    ).unwrap();
    caps.skills.push(crate::config::SkillSource {
        name: "test-skill".into(), path: skill_src.path().to_string_lossy().into_owned(), when: vec![],
    });
    // Hooks
    caps.hooks.push(Hook {
        event: "PreToolUse".into(), matcher: None,
        handler: HookHandler { kind: HookHandlerKind::Command, command: Some("echo guard".into()), tool: None },
        bundle_origin: None,
    });
    caps.hooks.push(Hook {
        event: "SessionStart".into(), matcher: None,
        handler: HookHandler { kind: HookHandlerKind::Command, command: Some("echo start".into()), tool: None },
        bundle_origin: None,
    });
    // Permissions
    caps.permissions.allow.push(PermissionRule { tool: "Bash".into(), pattern: Some("ls*".into()), paths: vec![] });
    caps.permissions.ask.push(PermissionRule { tool: "WebFetch".into(), pattern: None, paths: vec![] });
    caps.permissions.deny.push(PermissionRule { tool: "Edit".into(), pattern: Some("*.secret".into()), paths: vec![] });
    // LSP
    caps.lsp.push(llmenv_config::LspServer {
        name: "rust-analyzer".into(), command: "rust-analyzer".into(), args: vec!["--quiet".into()],
        filetypes: vec!["rust".into()],
        init_options: Some(serde_yaml::from_str("checkOnSave: true").unwrap()),
        ..Default::default()
    });

    let mut manifest = MergedManifest { capabilities: caps, ..Default::default() };
    manifest.rules.push((PathBuf::from("rules/extra.md"), {
        let p = tmp.path().join("_src/rules/extra.md");
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, "# Extra").unwrap();
        p
    }));
    // MCP
    manifest.mcps.push(ResolvedMcp {
        name: "local-test".into(),
        kind: ResolvedKind::Stdio { command: "npx".into(), args: vec!["test".into()], env: Default::default() },
        headers: Default::default(), timeout: None, disabled_tools: vec![],
    });
    manifest.mcps.push(ResolvedMcp {
        name: "remote-test".into(),
        kind: ResolvedKind::Remote { url: "http://localhost:4000".into(), transport: crate::config::McpTransport::Sse },
        headers: Default::default(), timeout: None, disabled_tools: vec![],
    });

    let owned = OpencodeAdapter.materialize(&manifest, tmp.path()).unwrap();

    // Validate output
    assert!(tmp.path().join("AGENTS.md").exists());
    assert!(tmp.path().join("rules/extra.md").exists());
    assert!(tmp.path().join("skills/test-skill/SKILL.md").exists());
    assert!(tmp.path().join("plugin/llmenv.js").exists());
    assert!(tmp.path().join(OPENCODE_JSON_FILE).exists());
    assert!(owned.contains(&PathBuf::from("AGENTS.md")));
    assert!(owned.contains(&PathBuf::from(OPENCODE_JSON_FILE)));
    assert!(owned.contains(&PathBuf::from("plugin/llmenv.js")));

    let raw = std::fs::read_to_string(tmp.path().join(OPENCODE_JSON_FILE)).unwrap();
    let doc: serde_json::Value = serde_json::from_str(&raw).unwrap();

    // Schema
    assert_eq!(doc["$schema"], json!("https://opencode.ai/config.json"));
    // Instructions
    assert!(doc["instructions"].as_array().unwrap().contains(&json!("rules/extra.md")));
    // MCP
    assert_eq!(doc["mcp"]["local-test"]["type"], json!("local"));
    assert_eq!(doc["mcp"]["local-test"]["command"], json!(["npx", "test"]));
    assert_eq!(doc["mcp"]["remote-test"]["type"], json!("remote"));
    // LSP
    assert_eq!(doc["lsp"]["rust-analyzer"]["command"], json!(["rust-analyzer", "--quiet"]));
    assert_eq!(doc["lsp"]["rust-analyzer"]["extensions"], json!(["rust"]));
    assert_eq!(doc["lsp"]["rust-analyzer"]["initialization"]["checkOnSave"], json!(true));
    // Permissions
    assert!(doc["permission"]["allow"].as_array().unwrap().contains(&json!("Bash(ls*)")));
    assert!(doc["permission"]["ask"].as_array().unwrap().contains(&json!("WebFetch")));
    assert!(doc["permission"]["deny"].as_array().unwrap().contains(&json!("Edit(*.secret)")));
    // Shim
    let shim = std::fs::read_to_string(tmp.path().join("plugin/llmenv.js")).unwrap();
    assert!(shim.contains("echo guard"));
    assert!(shim.contains("echo start"));
    assert!(shim.contains("check-stale --engine opencode"));
}
```

- [ ] **Step 2: Add env_vars tests**

```rust
#[test]
fn env_vars_returns_config_dir() {
    let cache = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    let vars = OpencodeAdapter.env_vars(cache.path(), state.path()).unwrap();
    assert_eq!(vars.len(), 1);
    assert_eq!(vars[0].0, "OPENCODE_CONFIG_DIR");
    assert_eq!(vars[0].1, cache.path().to_str().unwrap());
}

#[test]
fn env_vars_no_side_effect_on_state_dir() {
    // Unlike crush, opencode doesn't create a data subdir — state stays at XDG defaults
    let cache = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    let vars = OpencodeAdapter.env_vars(cache.path(), state.path()).unwrap();
    assert_eq!(vars.len(), 1);
}

#[test]
fn emit_hook_context_non_empty() {
    let out = OpencodeAdapter.emit_hook_context("SessionStart", "hello");
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["hookSpecificOutput"]["hookEventName"], "SessionStart");
    assert!(v["hookSpecificOutput"]["additionalContext"].as_str().unwrap().contains("hello"));
    assert!(v["hookSpecificOutput"]["additionalContext"].as_str().unwrap().contains("[ICM MEMORY CONTEXT"));
}

#[test]
fn emit_hook_context_empty_returns_empty() {
    assert_eq!(OpencodeAdapter.emit_hook_context("Stop", ""), "");
}
```

- [ ] **Step 3: Update CHANGELOG**

Read `CHANGELOG.md`, add under `## [Unreleased]`:

```
### Added
- opencode engine support: new `opencode` adapter with full parity vs. the
  claude-code adapter — rules, skills, MCP, LSP, permissions, hook bridging
  via a generated JS shim plugin, and Claude-plugin content translation
  (#657, #656)
```

- [ ] **Step 4: Run all tests, lint, commit**

```bash
cargo fmt
cargo clippy --all-targets -- -D warnings
cargo test --lib
git add -A
git commit -m "feat(adapter): opencode engine integration tests and changelog"
```

- [ ] **Step 5: Final pre-push check**

```bash
cargo build && cargo test --lib && cargo clippy --all-targets -- -D warnings
```

Push the branch and open a PR from `docs/opencode-adapter-spec` (the current branch).

---

### Task 10: Post-implementation verification

**Checklist** (not in code, run manually):
- [ ] `cargo test --lib` — all tests pass
- [ ] `cargo clippy --all-targets -- -D warnings` — zero warnings
- [ ] Verify crush adapter tests still pass (no regression)
- [ ] `git diff main --stat` — only the expected files
- [ ] CHANGELOG entry present under `## [Unreleased]`
- [ ] Run `scripts/gen-attribution.sh` if any deps changed (none should)
- [ ] Push and file PR

---

## Completion Checklist

- [x] Design spec committed and PR'd (#656)
- [x] GitHub issue filed (#657, Large Projects milestone)
- [ ] Implementation plan written (this document)
- [ ] Tasks 1–9 executed
- [ ] Verified against spec §Testing list
- [ ] PR opened, CI green
- [ ] `verify` skill run against real opencode binary
