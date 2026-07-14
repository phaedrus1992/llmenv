<!-- markdownlint-disable MD013 -->
# context-mode Built-in Feature Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a `features.context_mode.enabled` toggle that auto-wires the context-mode plugin (marketplace + plugin + durable data dir + MCP permission), and remove the broken `LLMENV_BASH_BAN` deny-wiring.

**Architecture:** context-mode is a Claude Code *plugin* (MCP + 7 hooks needing `${CLAUDE_PLUGIN_ROOT}`), so the built-in feature injects it through the existing plugin-resolution path rather than ICM's remote-MCP mechanism. Injection happens in `resolve_plugins` (marketplace + plugin), the #175 `StateTool` machinery (durable `CONTEXT_MODE_DATA_DIR`), and the adapter render (MCP allow-grant). The plugin carries its own hooks; llmenv's existing `reconcile_settings` hooks-merge preserves the plugin's self-registered cache-heal hook.

**Tech Stack:** Rust (workspace: core crate `src/`, config crate `crates/llmenv-config/`), serde/serde_yaml, anyhow, thiserror. Tests are `#[cfg(test)]` inline modules + `tests/*.rs` integration tests. Run via `cargo test`.

## Global Constraints

- Spec: `docs/superpowers/specs/2026-06-30-context-mode-builtin-design.md`. Issue #490.
- Canonical values (copy verbatim, define once in `crates/llmenv-config/src/lib.rs`):
  - `CONTEXT_MODE_MARKETPLACE = "context-mode"`
  - `CONTEXT_MODE_SOURCE = "https://github.com/mksglu/context-mode"`
  - `CONTEXT_MODE_PLUGIN = "context-mode"`
  - `CONTEXT_MODE_MCP_PREFIX = "mcp__plugin_context-mode_context-mode__"`
  - `CONTEXT_MODE_DATA_ENV = "CONTEXT_MODE_DATA_DIR"`
  - `CONTEXT_MODE_STATE_SUBDIR = "context-mode"`
- No tag-scoping: `features.context_mode` is enable/disable only.
- Track latest: injected marketplace source is hardcoded; no version pin.
- Code quality (CLAUDE.md): ≤100 lines/function, ≤8 complexity, 100-char lines, absolute imports, zero warnings. Run `cargo fmt`, `cargo clippy --all-targets --all-features -- -D warnings`, `cargo test` before each commit.
- Branch: `feat/490-context-mode-builtin` (already created).
- Dedup rule everywhere: user-declared config wins; built-in injection is a no-op when the user already declared the same plugin/marketplace/state-tool.

---

### Task 1: Schema — `ContextMode` struct, `Features.context_mode`, constants

**Files:**

- Modify: `crates/llmenv-config/src/schema.rs` (add struct + `Features` field)
- Modify: `crates/llmenv-config/src/lib.rs:5-19` (add constants + re-export `ContextMode`)
- Modify: `src/config/mod.rs:3-11` (re-export `ContextMode` + constants)

**Interfaces:**

- Produces: `pub struct ContextMode { pub enabled: bool }`; `Features.context_mode: Option<ContextMode>`; constants `CONTEXT_MODE_MARKETPLACE`, `CONTEXT_MODE_SOURCE`, `CONTEXT_MODE_PLUGIN`, `CONTEXT_MODE_MCP_PREFIX`, `CONTEXT_MODE_DATA_ENV`, `CONTEXT_MODE_STATE_SUBDIR` (all `pub const &str`).

- [ ] **Step 1: Write the failing test**

In `crates/llmenv-config/src/schema.rs`, inside the existing `#[cfg(test)] mod tests` (or add one if absent), add:

```rust
#[test]
fn context_mode_parses_enabled() {
    let yaml = "features:\n  context_mode:\n    enabled: true\n";
    let cfg: Config = serde_yaml::from_str(yaml).unwrap();
    let cm = cfg.features.unwrap().context_mode.unwrap();
    assert!(cm.enabled);
}

#[test]
fn context_mode_absent_is_none() {
    let cfg: Config = serde_yaml::from_str("features:\n  memory: []\n").unwrap();
    assert!(cfg.features.unwrap().context_mode.is_none());
}

#[test]
fn context_mode_default_disabled() {
    let yaml = "features:\n  context_mode: {}\n";
    let cfg: Config = serde_yaml::from_str(yaml).unwrap();
    assert!(!cfg.features.unwrap().context_mode.unwrap().enabled);
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p llmenv-config context_mode`
Expected: FAIL — `no field 'context_mode' on type 'Features'` (compile error).

- [ ] **Step 3: Add the struct and field**

In `crates/llmenv-config/src/schema.rs`, add the struct near `Memory`:

```rust
/// context-mode built-in feature toggle. Loaded as a Claude Code *plugin*
/// (not an MCP) because its hooks reference `${CLAUDE_PLUGIN_ROOT}`, which only
/// resolves inside the plugin system. When enabled, llmenv auto-injects the
/// context-mode marketplace + plugin, a durable `CONTEXT_MODE_DATA_DIR`, and the
/// MCP permission grant. Unlike `memory`, this is a simple toggle — context-mode
/// is a local FTS5 store with no host topology, so there is nothing to tag-scope.
#[derive(Debug, Clone, Deserialize, Serialize, Default, PartialEq, Eq)]
pub struct ContextMode {
    /// Whether the built-in context-mode plugin is wired up.
    #[serde(default)]
    pub enabled: bool,
}
```

In the `Features` struct, add after the `throttle` field:

```rust
    /// context-mode built-in (token-efficiency). The counterpart to `memory`
    /// (ICM). A simple enable/disable toggle; absent means disabled.
    #[serde(default)]
    pub context_mode: Option<ContextMode>,
```

- [ ] **Step 4: Add constants in `lib.rs`**

In `crates/llmenv-config/src/lib.rs`, after the `MEMORY_MCP_NAME` line:

```rust
/// Marketplace registration name for the built-in context-mode plugin.
pub const CONTEXT_MODE_MARKETPLACE: &str = "context-mode";
/// Canonical git source for the built-in context-mode plugin.
pub const CONTEXT_MODE_SOURCE: &str = "https://github.com/mksglu/context-mode";
/// Plugin name inside the context-mode marketplace.
pub const CONTEXT_MODE_PLUGIN: &str = "context-mode";
/// MCP tool-name prefix Claude Code assigns the context-mode plugin's server.
pub const CONTEXT_MODE_MCP_PREFIX: &str = "mcp__plugin_context-mode_context-mode__";
/// Env var context-mode honors to relocate its FTS5 store (#175 durable dir).
pub const CONTEXT_MODE_DATA_ENV: &str = "CONTEXT_MODE_DATA_DIR";
/// Durable-state subdir name for context-mode's store.
pub const CONTEXT_MODE_STATE_SUBDIR: &str = "context-mode";
```

Add `ContextMode` to the `pub use schema::{...}` list (alphabetical, after `Config`).

- [ ] **Step 5: Re-export through core crate**

In `src/config/mod.rs`, add `ContextMode` to the `pub use llmenv_config::{...}` import list (after `Config`), and add the six constants to the import list.

- [ ] **Step 6: Run tests + lints**

Run: `cargo test -p llmenv-config context_mode && cargo clippy --all-targets --all-features -- -D warnings`
Expected: PASS, zero warnings.

- [ ] **Step 7: Commit**

```bash
git add crates/llmenv-config/src/schema.rs crates/llmenv-config/src/lib.rs src/config/mod.rs
git commit -m "feat: add features.context_mode schema and constants (#490)"
```

---

### Task 2: Inject context-mode marketplace + plugin in `resolve_plugins`

**Files:**

- Modify: `src/plugins/resolve.rs:96-160` (`resolve_plugins`) + its test module

**Interfaces:**

- Consumes: `ContextMode`, `CONTEXT_MODE_MARKETPLACE`, `CONTEXT_MODE_SOURCE`, `CONTEXT_MODE_PLUGIN` from Task 1; existing `ResolvedPlugin`, `ResolvedMarketplace`, `ResolvedPlugins`.
- Produces: when `config.features.context_mode.enabled`, the returned `ResolvedPlugins` contains the context-mode plugin + marketplace (deduped vs user-declared). When the feature is enabled **and** the user has also manually declared the context-mode plugin in a `plugin-collection`, emit a `tracing::warn!` flagging the redundant manual declaration (the built-in feature already wires it).

- [ ] **Step 1: Write the failing tests**

In `src/plugins/resolve.rs` test module, add (reuse the existing `tags`/config-builder helpers in that module — check their names, e.g. `tags(&[...])`):

```rust
#[test]
fn context_mode_feature_injects_plugin_and_marketplace() {
    let mut cfg = Config::default();
    cfg.features = Some(crate::config::Features {
        context_mode: Some(crate::config::ContextMode { enabled: true }),
        ..Default::default()
    });
    let resolved = resolve_plugins(&cfg, &tags(&[])).unwrap();
    assert!(resolved.plugins.iter().any(|p| p.marketplace == "context-mode"
        && p.plugin == "context-mode"));
    assert!(resolved.marketplaces.iter().any(|m| m.name == "context-mode"
        && m.source == "https://github.com/mksglu/context-mode"));
}

#[test]
fn context_mode_disabled_injects_nothing() {
    let cfg = Config::default();
    let resolved = resolve_plugins(&cfg, &tags(&[])).unwrap();
    assert!(!resolved.plugins.iter().any(|p| p.marketplace == "context-mode"));
}

#[test]
fn context_mode_dedups_user_declared() {
    // User declares context-mode via a marketplace + collection AND enables the
    // feature: exactly one plugin entry, user's source preserved.
    let mut cfg = Config::default();
    cfg.marketplace = vec![Marketplace {
        name: "context-mode".into(),
        source: "https://github.com/myfork/context-mode".into(),
        ..Default::default()
    }];
    cfg.plugin_collection = vec![crate::config::PluginCollection {
        name: "core".into(),
        when: vec!["t".into()],
        plugins: vec!["context-mode:context-mode".into()],
    }];
    cfg.features = Some(crate::config::Features {
        context_mode: Some(crate::config::ContextMode { enabled: true }),
        ..Default::default()
    });
    let resolved = resolve_plugins(&cfg, &tags(&["t"])).unwrap();
    let cm: Vec<_> = resolved.plugins.iter()
        .filter(|p| p.marketplace == "context-mode").collect();
    assert_eq!(cm.len(), 1, "no duplicate plugin entry");
    let mk: Vec<_> = resolved.marketplaces.iter()
        .filter(|m| m.name == "context-mode").collect();
    assert_eq!(mk.len(), 1);
    assert_eq!(mk[0].source, "https://github.com/myfork/context-mode",
        "user-declared source wins");
}
```

Note: verify `Marketplace` and `PluginCollection` field names/defaults against `schema.rs` while writing — if `Marketplace` lacks `#[derive(Default)]`, construct it fully instead of `..Default::default()`.

Add a test that the redundant-declaration warning path is exercised (the dedup
branch is taken). Since `tracing::warn!` output isn't easily asserted without a
subscriber, assert the observable proxy — the dedup still yields exactly one
entry — and add a focused boolean helper so the warn condition is unit-testable:

```rust
#[test]
fn context_mode_user_declared_triggers_dedup_branch() {
    // Same setup as the dedup test: feature on + user-declared plugin. The
    // warn fires on the dedup branch; we assert the branch was taken (one entry)
    // which is the same observable the warn guards.
    let mut cfg = Config::default();
    cfg.marketplace = vec![Marketplace {
        name: "context-mode".into(),
        source: "https://github.com/mksglu/context-mode".into(),
        ..Default::default()
    }];
    cfg.plugin_collection = vec![crate::config::PluginCollection {
        name: "core".into(),
        when: vec!["t".into()],
        plugins: vec!["context-mode:context-mode".into()],
    }];
    cfg.features = Some(crate::config::Features {
        context_mode: Some(crate::config::ContextMode { enabled: true }),
        ..Default::default()
    });
    let resolved = resolve_plugins(&cfg, &tags(&["t"])).unwrap();
    assert_eq!(
        resolved.plugins.iter().filter(|p| p.marketplace == "context-mode").count(),
        1
    );
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p llmenv plugins::resolve::tests::context_mode`
(or `cargo test context_mode_feature_injects` if the binary crate name differs)
Expected: FAIL — injection assertions fail (feature does nothing yet).

- [ ] **Step 3: Implement the injection post-pass**

In `resolve_plugins`, after the collection loop (after the `for collection in ...` block, before the `let marketplaces = ...` emission), insert:

```rust
    // Built-in context-mode feature (#490): inject the canonical marketplace +
    // plugin when enabled, unless the user already declared it (user wins on
    // source). context-mode is a *plugin* (its hooks need ${CLAUDE_PLUGIN_ROOT}),
    // so it rides the normal plugin path — not ICM's remote-MCP mechanism.
    let cm_enabled = config
        .features
        .as_ref()
        .and_then(|f| f.context_mode.as_ref())
        .is_some_and(|c| c.enabled);
    let mut inject_builtin_marketplace = false;
    if cm_enabled {
        let key = (
            crate::config::CONTEXT_MODE_MARKETPLACE.to_string(),
            crate::config::CONTEXT_MODE_PLUGIN.to_string(),
        );
        if seen_plugin.insert(key) {
            referenced.insert(crate::config::CONTEXT_MODE_MARKETPLACE.to_string());
            plugins.push(ResolvedPlugin {
                marketplace: crate::config::CONTEXT_MODE_MARKETPLACE.to_string(),
                plugin: crate::config::CONTEXT_MODE_PLUGIN.to_string(),
                collection: "context_mode (built-in)".to_string(),
                install_path: None,
                git_commit_sha: None,
            });
        } else {
            // The user manually declared context-mode:context-mode in a
            // plugin-collection AND enabled features.context_mode. The built-in
            // already wires it — the manual entry is redundant. Warn so the user
            // can drop it (harmless, but confusing config drift otherwise).
            tracing::warn!(
                "features.context_mode is enabled and you also declared \
                 'context-mode:context-mode' in a plugin-collection — the \
                 built-in feature wires context-mode automatically, so the manual \
                 plugin-collection entry is redundant and can be removed."
            );
        }
        // The built-in marketplace is emitted from config.marketplace below only
        // if the user declared it. If they didn't, we must add it ourselves.
        inject_builtin_marketplace = !config
            .marketplace
            .iter()
            .any(|m| m.name == crate::config::CONTEXT_MODE_MARKETPLACE);
    }
```

Then, after the existing `let marketplaces = config.marketplace.iter()...collect();` (change `collect()` target to a mutable binding), append the synthetic marketplace:

```rust
    let mut marketplaces: Vec<ResolvedMarketplace> = config
        .marketplace
        .iter()
        .filter(|m| referenced.contains(&m.name))
        .map(|m| ResolvedMarketplace {
            name: m.name.clone(),
            source: m.source.clone(),
            install_location: None,
            head: None,
        })
        .collect();
    if inject_builtin_marketplace {
        marketplaces.push(ResolvedMarketplace {
            name: crate::config::CONTEXT_MODE_MARKETPLACE.to_string(),
            source: crate::config::CONTEXT_MODE_SOURCE.to_string(),
            install_location: None,
            head: None,
        });
    }
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p llmenv context_mode && cargo clippy --all-targets --all-features -- -D warnings`
Expected: PASS, zero warnings.

- [ ] **Step 5: Commit**

```bash
git add src/plugins/resolve.rs
git commit -m "feat: inject context-mode plugin when features.context_mode enabled (#490)"
```

---

### Task 3: Inject durable `CONTEXT_MODE_DATA_DIR` via StateTool

**Files:**

- Modify: `src/materialize/state.rs` (add `effective_state_config` helper + tests)
- Modify: `src/cli/mod.rs:873-880` (use the helper at the `state_env_vars`/`ensure_state_dirs` call site)

**Interfaces:**

- Consumes: `StateConfig`, `StateTool` (existing); `CONTEXT_MODE_DATA_ENV`, `CONTEXT_MODE_STATE_SUBDIR` from Task 1.
- Produces: `pub fn effective_state_config(cfg: &StateConfig, context_mode_enabled: bool) -> std::borrow::Cow<'_, StateConfig>` — returns the input unchanged when the feature is off or the env var is already declared; otherwise a clone with the synthetic tool appended.

- [ ] **Step 1: Write the failing test**

In `src/materialize/state.rs` test module:

```rust
#[test]
fn context_mode_injects_state_tool() {
    let base = StateConfig::default();
    let eff = effective_state_config(&base, true);
    assert!(eff.tools.iter().any(|t| t.env == "CONTEXT_MODE_DATA_DIR"
        && t.subdir == "context-mode"));
}

#[test]
fn context_mode_disabled_no_injection() {
    let base = StateConfig::default();
    let eff = effective_state_config(&base, false);
    assert!(eff.tools.is_empty());
}

#[test]
fn context_mode_dedups_user_state_tool() {
    let base = StateConfig {
        tools: vec![StateTool {
            env: "CONTEXT_MODE_DATA_DIR".into(),
            subdir: "my-custom-dir".into(),
        }],
    };
    let eff = effective_state_config(&base, true);
    let cm: Vec<_> = eff.tools.iter()
        .filter(|t| t.env == "CONTEXT_MODE_DATA_DIR").collect();
    assert_eq!(cm.len(), 1, "no duplicate");
    assert_eq!(cm[0].subdir, "my-custom-dir", "user entry preserved");
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p llmenv state::tests::context_mode`
Expected: FAIL — `effective_state_config` not found.

- [ ] **Step 3: Implement the helper**

In `src/materialize/state.rs`, add (import `Cow` at top: `use std::borrow::Cow;`):

```rust
/// Compute the effective state config, injecting context-mode's durable-dir
/// relocation (#490) when the built-in feature is enabled. Returns the input
/// borrowed unchanged when the feature is off or the user already declared a
/// `CONTEXT_MODE_DATA_DIR` tool (user wins); otherwise a clone with the synthetic
/// tool appended.
#[must_use]
pub fn effective_state_config(
    cfg: &StateConfig,
    context_mode_enabled: bool,
) -> Cow<'_, StateConfig> {
    use crate::config::{CONTEXT_MODE_DATA_ENV, CONTEXT_MODE_STATE_SUBDIR};
    if !context_mode_enabled
        || cfg.tools.iter().any(|t| t.env == CONTEXT_MODE_DATA_ENV)
    {
        return Cow::Borrowed(cfg);
    }
    let mut owned = cfg.clone();
    owned.tools.push(crate::config::StateTool {
        env: CONTEXT_MODE_DATA_ENV.to_string(),
        subdir: CONTEXT_MODE_STATE_SUBDIR.to_string(),
    });
    Cow::Owned(owned)
}
```

`StateConfig` already derives `Clone` (verify in schema.rs:644 — `#[derive(... Default ...)]`; it derives Clone). If not, add `Clone` to its derives.

- [ ] **Step 4: Wire it at the call site**

In `src/cli/mod.rs` around line 873-880, replace the two `&config.state` arguments. First compute the flag and effective config:

```rust
    let cm_enabled = config
        .features
        .as_ref()
        .and_then(|f| f.context_mode.as_ref())
        .is_some_and(|c| c.enabled);
    let state_cfg = crate::materialize::state::effective_state_config(&config.state, cm_enabled);
    let state_dir = crate::materialize::state::state_dir(&adapter_root);
    crate::materialize::state::ensure_state_dirs(&state_cfg, &state_dir)
        .context("creating durable state directories")?;
    env_vars.extend(crate::materialize::state::state_env_vars(
        &state_cfg,
        &state_dir,
    ));
```

(`state_env_vars`/`ensure_state_dirs` take `&StateConfig`; `&state_cfg` derefs from `Cow` via `&*` — write `&state_cfg` if `Cow` auto-derefs in the call, else `&*state_cfg`. Use `&state_cfg` and let deref coercion apply; if the compiler complains, use `state_cfg.as_ref()`.)

- [ ] **Step 5: Run tests + lints**

Run: `cargo test -p llmenv state::tests::context_mode && cargo clippy --all-targets --all-features -- -D warnings`
Expected: PASS, zero warnings.

- [ ] **Step 6: Commit**

```bash
git add src/materialize/state.rs src/cli/mod.rs
git commit -m "feat: relocate CONTEXT_MODE_DATA_DIR to durable state dir when enabled (#490)"
```

---

### Task 4: Remove `LLMENV_BASH_BAN`; add context-mode MCP permission grant

**Files:**

- Modify: `src/adapter/claude_code.rs:778-816` (delete bash-ban block); permission render area (~768-830) for the grant + its test module

**Interfaces:**

- Consumes: `CONTEXT_MODE_MCP_PREFIX` from Task 1; existing `manifest.plugins`, the `allow` Vec built near `claude_code.rs:768`.
- Produces: settings.json `permissions.allow` contains `mcp__plugin_context-mode_context-mode__*` iff the context-mode plugin is in `manifest.plugins`.

- [ ] **Step 1: Write the failing tests**

In `src/adapter/claude_code.rs` test module, find an existing test that builds a `MergedManifest` and renders settings (search for `fn render` or a settings-rendering test helper). Add:

```rust
#[test]
fn context_mode_plugin_grants_mcp_permission() {
    // Build a manifest whose resolved plugins include context-mode, render
    // settings.json, assert the MCP wildcard is in permissions.allow.
    let mut manifest = MergedManifest::default();
    manifest.plugins = vec![crate::plugins::resolve::ResolvedPlugin {
        marketplace: "context-mode".into(),
        plugin: "context-mode".into(),
        collection: "context_mode (built-in)".into(),
        install_path: None,
        git_commit_sha: None,
    }];
    let settings = render_settings_for_test(&manifest); // use the module's existing render helper
    let allow = settings["permissions"]["allow"].as_array().unwrap();
    assert!(allow.iter().any(|v| v == "mcp__plugin_context-mode_context-mode__*"));
}

#[test]
fn bash_ban_env_no_longer_adds_deny_rules() {
    // Regression guard (#490 / #464): LLMENV_BASH_BAN must be inert.
    // SAFETY: single-threaded test; set + remove around the render.
    unsafe { std::env::set_var("LLMENV_BASH_BAN", "cat,find,grep") };
    let manifest = MergedManifest::default();
    let settings = render_settings_for_test(&manifest);
    let deny = settings
        .get("permissions")
        .and_then(|p| p.get("deny"))
        .and_then(|d| d.as_array())
        .cloned()
        .unwrap_or_default();
    assert!(!deny.iter().any(|v| v.as_str().is_some_and(|s| s.contains("Bash(cat"))));
    unsafe { std::env::remove_var("LLMENV_BASH_BAN") };
}
```

Adapt `render_settings_for_test` to whatever the module's existing settings-render entry point is (the function containing lines 768-830 — likely `render_settings` or similar; match its real signature). If no test helper exists, call the real render function directly with a minimal manifest.

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p llmenv context_mode_plugin_grants_mcp_permission bash_ban_env_no_longer`
Expected: permission test FAILS (no grant yet); bash-ban test FAILS (deny rule still added).

- [ ] **Step 3: Delete the bash-ban block**

In `src/adapter/claude_code.rs`, delete lines 778-816 inclusive — the entire `// #464: Wire LLMENV_BASH_BAN ...` comment through the closing `}` of the `match std::env::var("LLMENV_BASH_BAN")` block. The `let mut deny = render_action(...)` above it stays; the `let has_perms = ...` below it stays.

- [ ] **Step 4: Add the MCP permission grant**

Immediately after the `let mut deny = render_action(...)` binding (where the deleted block was), insert:

```rust
    // Built-in context-mode (#490): grant the plugin's MCP tools when the
    // context-mode plugin is in the resolved set. Derived from manifest.plugins
    // (not config.features) so the grant only appears when the plugin loaded.
    let context_mode_active = manifest.plugins.iter().any(|p| {
        p.marketplace == crate::config::CONTEXT_MODE_MARKETPLACE
            && p.plugin == crate::config::CONTEXT_MODE_PLUGIN
    });
    if context_mode_active {
        allow.push(format!("{}*", crate::config::CONTEXT_MODE_MCP_PREFIX));
        dedup(&mut allow);
    }
```

(`allow` is the `let mut allow = render_action(...)` binding earlier in the function; `dedup` is the same helper the deleted block used — confirm it's still imported. If `allow` is not `mut`, make it `mut`.)

- [ ] **Step 5: Run tests + lints**

Run: `cargo test -p llmenv context_mode_plugin_grants_mcp_permission bash_ban_env_no_longer && cargo clippy --all-targets --all-features -- -D warnings`
Expected: PASS, zero warnings.

- [ ] **Step 6: Commit**

```bash
git add src/adapter/claude_code.rs
git commit -m "feat: grant context-mode MCP permission; remove LLMENV_BASH_BAN wiring (#490)"
```

---

### Task 5: doctor — report the built-in feature

**Files:**

- Modify: `src/cli/doctor.rs:62-68` (replace `config.mcp` scan)

**Interfaces:**

- Consumes: `config.features.context_mode.enabled` (Task 1); existing `pass`/`warn`/`info` strings + `super::doctor_info`.

- [ ] **Step 1: Replace the check**

In `src/cli/doctor.rs`, the function takes `info` from `super::doctor_info(use_color)` already (line 14). Replace lines 62-68:

```rust
    let cm_enabled = config
        .features
        .as_ref()
        .and_then(|f| f.context_mode.as_ref())
        .is_some_and(|c| c.enabled);
    if cm_enabled {
        eprintln!("{pass} context-mode built-in feature enabled (token-efficiency)");
    } else {
        eprintln!(
            "{info} context-mode not enabled \
             (set features.context_mode.enabled: true for built-in context saving)"
        );
    }
```

- [ ] **Step 2: Build to verify it compiles**

Run: `cargo build -p llmenv && cargo clippy --all-targets --all-features -- -D warnings`
Expected: compiles, zero warnings. (`info` is already in scope at line 14; if a "must use" or unused-var fires, that's the signal it wasn't — verify.)

- [ ] **Step 3: Add/adjust doctor test if one exists**

Check `tests/doctor_version_skew.rs` and any `doctor` test for token-efficiency assertions referencing the old "context-mode MCP not configured" string. If found, update the expected string to the new wording. If no such assertion exists, skip (no new test required — the rendering is a one-line branch already covered by manual run).

- [ ] **Step 4: Commit**

```bash
git add src/cli/doctor.rs tests/doctor_version_skew.rs
git commit -m "feat: doctor reports context-mode built-in feature status (#490)"
```

---

### Task 6: Regression test — self-registered cache-heal hook survives re-render

**Files:**

- Modify: `src/adapter/claude_code.rs` test module (test `reconcile_settings`)

**Interfaces:**

- Consumes: existing private `reconcile_settings(path, fresh) -> anyhow::Result<Value>` (claude_code.rs:1072).

- [ ] **Step 1: Write the test**

In `src/adapter/claude_code.rs` test module:

```rust
#[test]
fn reconcile_preserves_context_mode_self_registered_hook() {
    use serde_json::json;
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("settings.json");
    // Simulate a prior render where context-mode's start.mjs added a cache-heal
    // SessionStart hook into settings.json.
    let on_disk = json!({
        "hooks": {
            "SessionStart": [
                { "hooks": [ { "type": "command",
                  "command": "node /cfg/hooks/context-mode-cache-heal.mjs" } ] }
            ]
        },
        "enabledPlugins": { "context-mode@context-mode": true }
    });
    std::fs::write(&path, serde_json::to_vec(&on_disk).unwrap()).unwrap();

    // llmenv re-renders: its own hooks + authoritative enabledPlugins.
    let fresh = json!({
        "hooks": { "SessionStart": [
            { "hooks": [ { "type": "command", "command": "node /cfg/llmenv-own.mjs" } ] }
        ] },
        "enabledPlugins": { "context-mode@context-mode": true },
        "permissions": { "allow": [], "ask": [], "deny": [] }
    });

    let merged = reconcile_settings(&path, fresh).unwrap();
    let ss = merged["hooks"]["SessionStart"].as_array().unwrap();
    let commands: Vec<&str> = ss.iter()
        .flat_map(|e| e["hooks"].as_array().unwrap())
        .map(|h| h["command"].as_str().unwrap())
        .collect();
    assert!(commands.iter().any(|c| c.contains("context-mode-cache-heal")),
        "self-registered cache-heal hook must survive");
    assert!(commands.iter().any(|c| c.contains("llmenv-own")),
        "llmenv's own rendered hook must be present");
    assert_eq!(merged["enabledPlugins"]["context-mode@context-mode"], json!(true));
}
```

(Verify `tempfile` is a dev-dependency — it's used elsewhere in `state.rs` tests, so it is. `reconcile_settings` is module-private; this test lives in the same module so it has access.)

- [ ] **Step 2: Run test to verify it passes**

Run: `cargo test -p llmenv reconcile_preserves_context_mode`
Expected: PASS (this pins existing behavior — it should pass without source changes; if it fails, the merge behavior regressed and must be investigated before proceeding).

- [ ] **Step 3: Commit**

```bash
git add src/adapter/claude_code.rs
git commit -m "test: guard context-mode self-registered hook survives reconcile (#490)"
```

---

### Task 7: Docs, example config, changelog

**Files:**

- Modify: `docs/env-vars.md:52-61` (drop bash-ban example)
- Modify: `examples/config-llmenv-dir/config.yaml` (illustrate `features.context_mode`; docs-only, not product code)
- Modify: `CHANGELOG.md` (Unreleased)

- [ ] **Step 1: Update `docs/env-vars.md`**

Replace the example block at lines 52-61 (which shows `LLMENV_BASH_BAN`) with a version that drops the bash-ban line and notes the built-in feature. New block:

```markdown
Example:

​```yaml
# ✅ OK: no prefix for variables that are just bundle configuration
env:
  CBM_WARN_THRESHOLD: 50000
  CBM_AUTOINDEX: "true"
​```

> **Note:** Token-efficiency is now a built-in feature, not an env var. Enable
> it with `features.context_mode.enabled: true` (wires the context-mode plugin
> automatically). The former `LLMENV_BASH_BAN` env var was removed in #490.
```

(Strip the zero-width characters around the fences when editing — they're only here to escape the nested code block.)

- [ ] **Step 2: Illustrate in the example config**

In `examples/config-llmenv-dir/config.yaml`, in the `features:` block at the end (after `memory:`), add an illustrative, commented entry:

```yaml
  # Built-in context-saving (#490). Enabling this wires the context-mode plugin
  # automatically: marketplace + plugin + durable CONTEXT_MODE_DATA_DIR + the MCP
  # permission grant. Replaces the manual plugin-collection / state / permission
  # boilerplate. context-mode is loaded as a *plugin* (not an MCP) so its hooks
  # resolve ${CLAUDE_PLUGIN_ROOT}.
  context_mode:
    enabled: true
```

This is documentation only — `examples/` is illustrative config, never product code (per AGENTS.md). Do not remove the existing manual `context-mode` marketplace/plugin/state wiring elsewhere in the example; just show the toggle. Optionally add a one-line comment near the old manual wiring pointing at the new toggle.

- [ ] **Step 3: Update CHANGELOG**

Invoke the `keepachangelog` skill (or edit directly) to add under `## [Unreleased]`:

```markdown
### Added
- `features.context_mode` built-in feature: enabling `features.context_mode.enabled`
  auto-wires the context-mode plugin (marketplace, plugin, durable
  `CONTEXT_MODE_DATA_DIR`, and MCP permission) — the token-efficiency counterpart
  to the built-in ICM memory feature. (#490)

### Removed
- `LLMENV_BASH_BAN` env var and its deny-rule wiring. It was broken as shipped
  (read from llmenv's process env before bundle-declared values landed) and is
  superseded by the built-in context-mode feature. (#490, removes #464)
```

Reconcile against the older release line per AGENTS.md (check `git log --no-merges <last-tag>..HEAD` and the newest `release/X.x` branch's CHANGELOG for any forward-merged fix that must be re-listed). `LLMENV_BASH_BAN`/context-mode are new on `main`, so no back-reference is expected — confirm and note in the commit if nothing to reconcile.

- [ ] **Step 4: Verify docs build / no broken refs**

Run: `cargo build -p llmenv` (sanity) and manually re-read the three edited files.
Expected: edits read cleanly, no dangling `LLMENV_BASH_BAN` references remain (`git grep -n LLMENV_BASH_BAN` returns nothing outside CHANGELOG).

- [ ] **Step 5: Commit**

```bash
git add docs/env-vars.md examples/config-llmenv-dir/config.yaml CHANGELOG.md
git commit -m "docs: document context-mode built-in; remove LLMENV_BASH_BAN docs (#490)"
```

---

### Task 8: Full verification sweep

- [ ] **Step 1: Confirm no stray references**

Run: `git grep -n "LLMENV_BASH_BAN"`
Expected: matches only in `CHANGELOG.md` (the removal note). Any match in `src/`, `crates/`, `docs/env-vars.md` is a miss — fix it.

- [ ] **Step 2: Full test + lint + fmt**

Run:

```bash
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test
```

Expected: fmt clean, zero warnings, all tests pass.

- [ ] **Step 3: Manual smoke (optional but recommended)**

Build and run doctor against the example config:

```bash
cargo build -p llmenv
LLMENV_CONFIG_DIR=examples/config-llmenv-dir ./target/debug/llmenv doctor 2>&1 | grep -i context-mode
```

Expected: "context-mode built-in feature enabled (token-efficiency)".

- [ ] **Step 4: Hand off to ship-issue** for PR creation, pre-pr-review scans, CI watch, and merge (dev-sprint Phase 4 owns this).

---

## Self-Review

**Spec coverage:**

- Drop LLMENV_BASH_BAN → Task 4 (code) + Task 7 (docs) + Task 8 (sweep). ✓
- `features.context_mode` schema, no tag-scoping → Task 1. ✓
- Inject in resolve_plugins, track latest → Task 2. ✓
- Durable state dir via #175 StateTool → Task 3. ✓
- MCP permission grant (mirror icm_active) → Task 4. ✓
- doctor reports built-in (info not warn) → Task 5. ✓
- Self-registered cache-heal hook interaction → Task 6 regression test. ✓
- Dedup rules (user wins) → Tasks 2, 3 each have a dedup test. ✓
- Example config + changelog → Task 7. ✓

**Placeholder scan:** No "TBD"/"add error handling"/"similar to Task N". Each code step shows full code. Two spots say "verify against schema.rs / match the real helper name" — these are deliberate codebase-fit checks, not placeholders (exact code is given; the engineer confirms field names exist).

**Type consistency:** `ContextMode { enabled: bool }`, `Features.context_mode: Option<ContextMode>`, constants, `effective_state_config(&StateConfig, bool) -> Cow<StateConfig>`, `context_mode_active` from `manifest.plugins` — names consistent across Tasks 1-6.
