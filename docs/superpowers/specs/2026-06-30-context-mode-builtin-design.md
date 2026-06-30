# Design: context-mode as a built-in feature; drop LLMENV_BASH_BAN

**Issue:** #490
**Milestone:** Large Features
**Date:** 2026-06-30

## Summary

Two coupled changes:

1. **Remove `LLMENV_BASH_BAN` (#464) entirely.** Banning `cat`/`grep`/`find` is a
   crude way to save context, and the feature is broken as shipped: the adapter
   reads `LLMENV_BASH_BAN` from llmenv's *own* process env at materialize time
   (`src/adapter/claude_code.rs:780`), but bundles declare it in
   `capabilities.env`, which only lands in the session env *after* llmenv has
   already run — so a bundle-declared ban produces zero deny rules.

2. **Make context-mode a first-class built-in feature**, the way ICM is llmenv's
   built-in memory. Today users wire context-mode by hand: a `plugin-collection`
   entry, a `marketplace` entry, a `state:` tool for `CONTEXT_MODE_DATA_DIR`, and
   a `native_permissions` grant — all copy-pasted. A `features.context_mode`
   toggle replaces that boilerplate with one switch.

## Background: why context-mode is wired as a plugin, not an MCP

context-mode (`github.com/mksglu/context-mode`) is a **Claude Code plugin**, not a
standalone MCP server. Its `plugin.json` declares the MCP server *and* it ships
7 hooks (`hooks/hooks.json`):

- `PreToolUse` (Bash/WebFetch/Read/Grep/Agent/mcp__ routing)
- `PostToolUse` (session capture)
- `UserPromptSubmit`, `PreCompact` (snapshot), `SessionStart` (restore), `Stop`

Every hook command is `node "${CLAUDE_PLUGIN_ROOT}/hooks/*.mjs"`. The
`${CLAUDE_PLUGIN_ROOT}` variable **only resolves when Claude Code loads the plugin
through its plugin system.** Copying those hooks into top-level `settings.json`
(llmenv's native-hook path) breaks them — they resolve to nothing and produce a
SessionStart hook error (this is already documented in the example config at
`examples/config-llmenv-dir/config.yaml:186-191`).

**Consequence:** "built-in like ICM" cannot mean ICM's *mechanism*. ICM is a
remote MCP (`icm serve` + `mcp-proxy`, resolved by `resolve_mcps`). context-mode
must be loaded **as a plugin** so its hooks work. The built-in feature therefore
auto-injects the marketplace + plugin + durable state dir + MCP permission that a
user assembles manually today — and the hooks ride along inside the plugin for
free.

This is a deliberate divergence from ICM and must be called out in code comments
so a future reader doesn't try to "unify" the two feature mechanisms.

## Scope

In scope:
- Delete `LLMENV_BASH_BAN` deny-wiring and its documentation example.
- Add `features.context_mode` schema (`enabled: bool`, no tag-scoping).
- Auto-inject marketplace + plugin + state-dir env var + MCP permission when enabled.
- Update `doctor` token-efficiency check to report the built-in feature.

Out of scope:
- Tag-scoping context-mode (decided: enable/disable only).
- Pinning context-mode to a version (decided: track latest via normal marketplace sync).
- Fixing context-mode's own self-heal hook that writes into the config dir ignoring
  its env var — that is the tool's upstream bug (per ICM decision on #175), not
  llmenv's to fix from outside. The `state:` relocation is best-effort.

## Decisions

- **No tag-scoping.** `features.context_mode` is a simple enable/disable, unlike
  `features.memory` (which is a tag-scoped list because different hosts run
  different daemons). context-mode's store is a local FTS5 db; there is no
  topology to scope.
- **Injection point: `resolve_plugins`.** When the feature is enabled, the resolver
  appends the canonical context-mode marketplace + plugin to its output, deduped
  against any user-declared `context-mode:context-mode`. Single chokepoint: plugin
  payload sync, marketplace clone, hook loading, and `plugin ls` provenance all
  flow through the existing path unchanged.
- **Source: track latest.** The injected marketplace source is hardcoded to
  `https://github.com/mksglu/context-mode`; llmenv's normal marketplace
  fast-forward sync pulls updates, identical to every other plugin. No version
  pin, no bump mechanism.

## Architecture

### Component 1 — Schema (`crates/llmenv-config/src/schema.rs`)

Add to `Features`:

```rust
/// context-mode built-in: llmenv's built-in context-saving feature, the
/// counterpart to ICM (built-in memory). When enabled, llmenv auto-wires the
/// context-mode plugin (marketplace + plugin + durable state dir + MCP
/// permission). Unlike `memory`, this is a simple toggle, not a tag-scoped
/// list — context-mode is a local FTS5 store with no host topology.
#[serde(default)]
pub context_mode: Option<ContextMode>,
```

New struct:

```rust
/// context-mode built-in feature toggle. Loaded as a Claude Code *plugin*
/// (not an MCP) because its hooks reference ${CLAUDE_PLUGIN_ROOT}, which only
/// resolves inside the plugin system — see design doc.
#[derive(Debug, Clone, Deserialize, Serialize, Default, PartialEq, Eq)]
pub struct ContextMode {
    /// Whether the built-in context-mode plugin is wired up.
    #[serde(default)]
    pub enabled: bool,
}
```

Canonical source constant (in `llmenv-config`, alongside `MEMORY_MCP_NAME`):

```rust
/// Marketplace registration name for the built-in context-mode plugin.
pub const CONTEXT_MODE_MARKETPLACE: &str = "context-mode";
/// Canonical git source for the built-in context-mode plugin.
pub const CONTEXT_MODE_SOURCE: &str = "https://github.com/mksglu/context-mode";
/// Plugin name inside the context-mode marketplace.
pub const CONTEXT_MODE_PLUGIN: &str = "context-mode";
/// MCP tool-name prefix Claude Code assigns the plugin's MCP server.
pub const CONTEXT_MODE_MCP_PREFIX: &str = "mcp__plugin_context-mode_context-mode__";
/// Env var context-mode honors to relocate its FTS5 store (#175 durable dir).
pub const CONTEXT_MODE_DATA_ENV: &str = "CONTEXT_MODE_DATA_DIR";
/// Durable-state subdir name for context-mode's store.
pub const CONTEXT_MODE_STATE_SUBDIR: &str = "context-mode";
```

Re-export the struct + constants through `src/config/mod.rs`.

### Component 2 — Plugin injection (`src/plugins/resolve.rs`)

`resolve_plugins` gains a post-pass: when `config.features.context_mode.enabled`,
inject the marketplace + plugin if not already present.

- After the collection loop, if the feature is on and
  `(CONTEXT_MODE_MARKETPLACE, CONTEXT_MODE_PLUGIN)` is not already in
  `seen_plugin`, push a `ResolvedPlugin` with `collection: "context_mode (built-in)"`
  for provenance, and add `CONTEXT_MODE_MARKETPLACE` to `referenced`.
- The marketplace-emission step already filters declared marketplaces by
  `referenced`. The built-in marketplace is *not* in `config.marketplace`, so
  also synthesize a `ResolvedMarketplace { name: CONTEXT_MODE_MARKETPLACE, source:
  CONTEXT_MODE_SOURCE, .. }` and append it (deduped if the user also declared a
  `context-mode` marketplace — user declaration wins on source).
- Dedup rule: if a user already declared the plugin/marketplace (e.g. existing
  `core` collection), the built-in injection is a no-op for that entry — no
  duplicate, and the user's marketplace source is preserved.

Validation (`crates/llmenv-config/src/validate.rs`): no new rule strictly
required — the injected refs are well-formed by construction. Add a defensive
check only if a user-declared `context-mode` marketplace points at a different
source while the feature is on: warn (don't reject), since a fork is legitimate.

### Component 3 — Durable state dir

When the feature is enabled, llmenv must emit
`CONTEXT_MODE_DATA_DIR=<state>/context-mode`. This reuses the #175 `StateTool`
machinery (`src/materialize/state.rs`).

Implementation: at the point where `state_env_vars` is computed during
materialization, if `features.context_mode.enabled` and the user has not already
declared a `state.tools` entry for `CONTEXT_MODE_DATA_ENV`, inject a synthetic
`StateTool { env: CONTEXT_MODE_DATA_ENV, subdir: CONTEXT_MODE_STATE_SUBDIR }`
into the effective `StateConfig` before `state_env_vars` / `ensure_state_dirs`
run. Dedup against any user-declared entry (user wins).

### Component 4 — MCP permission grant (`src/adapter/claude_code.rs`)

The plugin's MCP tools need an allow grant (`mcp__plugin_context-mode_context-mode__*`),
matching what the example config grants by hand. Inject into the rendered `allow`
array when the feature is active — mirroring the `icm_active` pattern at
`claude_code.rs:838`. Use a `context_mode_active` boolean derived from the
manifest's resolved plugins (presence of the context-mode plugin) so the grant
only appears when the plugin actually loaded.

This is the same array the deleted `LLMENV_BASH_BAN` block touched (`allow`/`deny`
render near `claude_code.rs:768-816`), so the removal and this addition are local
to the same function.

### Component 5 — Remove LLMENV_BASH_BAN

- Delete `src/adapter/claude_code.rs:778-816` (the `match std::env::var(...)`
  deny-wiring block).
- `docs/env-vars.md:52-55`: remove the `LLMENV_BASH_BAN` example; replace the
  "Bundle-Provided Variables" example with a neutral one (e.g. the `CBM_*`
  example already present below it) and add a one-line note that context-mode is
  now the supported context-saving path via `features.context_mode`.
- `LLMENV_BASH_BAN` is not in `LLMENV_OWNED_SETTINGS_KEYS` and has no tests, so no
  other code changes.

### Component 6 — doctor (`src/cli/doctor.rs:62-68`)

Replace the `config.mcp` scan:

```rust
let cm_enabled = config.features.as_ref()
    .and_then(|f| f.context_mode.as_ref())
    .is_some_and(|c| c.enabled);
if cm_enabled {
    eprintln!("{pass} context-mode built-in feature enabled (token-efficiency)");
} else {
    eprintln!("{info} context-mode not enabled \
        (set features.context_mode.enabled: true for built-in context saving)");
}
```

(`info`, not `warn` — disabled is a valid choice, not a misconfiguration.)

## Data flow

```
config.yaml: features.context_mode.enabled: true
        │
        ├─ resolve_plugins ──► inject context-mode marketplace + plugin
        │                        (deduped vs user-declared)
        │                              │
        │                        sync_marketplaces / sync_plugin_payloads
        │                              │
        │                        settings.json: extraKnownMarketplaces + enabledPlugins
        │                        (plugin carries its own hooks via ${CLAUDE_PLUGIN_ROOT})
        │
        ├─ materialize state ─► inject StateTool{CONTEXT_MODE_DATA_DIR, context-mode}
        │                        env: CONTEXT_MODE_DATA_DIR=<state>/context-mode
        │
        └─ adapter render ────► settings.json permissions.allow +=
                                 mcp__plugin_context-mode_context-mode__*
```

## Interaction: context-mode's self-registered cache-heal hook

context-mode's MCP entrypoint (`start.mjs`) self-heals on every boot by writing
into llmenv's materialized `$CLAUDE_CONFIG_DIR`:

1. Registers a **cache-heal `SessionStart` hook** into `settings.json`
   (`start.mjs:387-413`) — an *additional* hook llmenv never rendered.
2. Drops `hooks/context-mode-cache-heal.mjs` (the script that hook runs).
3. Heals `settings.json.enabledPlugins` so the plugin loader re-enables it.

llmenv's `reconcile_settings` (#175/#196) already handles all three cleanly, and
the built-in feature *improves* the situation:

- **`hooks` is the single owned key that is merged, not replaced**
  (`src/adapter/claude_code.rs:1102-1114`): per-event arrays concat + dedup. The
  cache-heal `SessionStart` entry context-mode self-registers **survives every
  re-render** alongside llmenv's rendered hooks, with no duplication.
- **`hooks/context-mode-cache-heal.mjs` is a foreign (non-owned) file** —
  reconciliation only deletes `previous_owned − current_owned`, so it is
  preserved in `version` mode and re-deployed on every boot in `strict` mode.
- **`enabledPlugins` is authoritatively replaced**, and because this feature
  makes llmenv render `context-mode@context-mode: true` itself, context-mode's
  `healSettingsEnabledPlugins` becomes a redundant no-op instead of fighting
  llmenv. Today a manually-wired plugin can drift from llmenv's render; with the
  built-in feature they agree by construction.

Known rough edge (not a regression, no fix needed): in `strict` hashing mode each
config change spawns a fresh content-hashed folder, so the cache-heal hook is
absent until context-mode's next MCP boot re-registers it. It is self-correcting,
and the default (`version` + `major_minor`, stable folder) preserves it across
renders. This matches the existing #175 decision that context-mode's
config-dir-writing self-heal is the tool's behavior, best-effort honored, not
something llmenv guarantees from outside.

**The feature depends on the `reconcile_settings` hooks-merge staying intact** —
a regression test (below) guards it.

## Error handling

- Feature absent / `enabled: false` → zero injections, no behavior change (fail-safe default).
- User already declares the plugin/marketplace/state-tool → injection deduped, user declaration wins. No error.
- Marketplace not yet cloned at export time → existing `sync_marketplaces` warning path applies unchanged (#282).
- User-declared `context-mode` marketplace with a divergent source → warn, proceed (fork is legitimate).

## Testing

Behavior-level (test the observable outcome, not internals):

1. **Schema round-trip** (`crates/llmenv-config`): `features.context_mode.enabled: true`
   parses; absent → `None`; `false` → present but disabled.
2. **Plugin injection** (`src/plugins/resolve.rs` tests): feature on → resolved
   plugins contain `context-mode:context-mode` + its marketplace; feature off →
   absent; user already declared it → exactly one entry (no dup), user source wins.
3. **State dir** (`src/materialize/state.rs` tests): feature on → `state_env_vars`
   includes `CONTEXT_MODE_DATA_DIR=<state>/context-mode`; user-declared entry not
   duplicated.
4. **Permission grant** (`src/adapter/claude_code.rs` tests): feature on →
   settings.json `permissions.allow` contains the MCP wildcard; off → absent.
5. **Bash-ban removal**: a test asserting that setting `LLMENV_BASH_BAN` env var
   no longer adds any deny rule (proves the wiring is gone, guards against
   reintroduction). Edge: set the var, render, assert deny array unaffected.
6. **doctor**: feature on → "enabled" line; off → info line. (Covered by existing
   doctor test harness if present; otherwise a focused unit test.)

7. **Self-registered hook survival (regression guard).** Render settings.json
   with the feature on → simulate context-mode's boot by adding a foreign
   `SessionStart` cache-heal hook entry to the on-disk settings.json → re-render
   → assert via `reconcile_settings` that the foreign cache-heal entry **survives**
   and llmenv's own rendered hooks are still present with no duplication. This
   pins the `hooks`-merge behavior the feature relies on (see Interaction section).
   Also assert `enabledPlugins` contains `context-mode@context-mode` after render
   (proves llmenv now authoritatively owns it, so context-mode's heal is a no-op).

Edge/error cases to cover: empty active tag set, feature on with no other plugins
(injection still works standalone), feature on alongside an existing `core`
collection that already lists context-mode (dedup).

## Files touched

- `crates/llmenv-config/src/schema.rs` — `ContextMode` struct, `Features.context_mode`, constants
- `crates/llmenv-config/src/lib.rs` — re-export constants
- `crates/llmenv-config/src/validate.rs` — defensive divergent-source warning (optional)
- `src/config/mod.rs` — re-export `ContextMode` + constants
- `src/plugins/resolve.rs` — built-in injection post-pass
- `src/materialize/state.rs` (or its caller in `src/cli/mod.rs`) — synthetic StateTool injection
- `src/adapter/claude_code.rs` — delete bash-ban block; add MCP permission grant
- `src/cli/doctor.rs` — built-in feature check
- `docs/env-vars.md` — drop bash-ban example
- `CHANGELOG.md` — Unreleased entry (added: context-mode built-in; removed: LLMENV_BASH_BAN)
- `examples/config-llmenv-dir/config.yaml` — illustrative: show `features.context_mode`
  replacing the manual wiring (docs-only; example is not product code)
```
