# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/),
and this project adheres to [Semantic Versioning](https://semver.org/).

## [Unreleased]

### Fixed

- **Deep-merge non-idempotence** — `util::merge_json` / `merge_yaml` and
  `merge::capabilities::merge_native_feature` could keep duplicate
  sequence/array elements on the insert and scalar-overwrite paths that a later
  merge would dedup away, so `merge(merge(x)) != merge(x)`. All write paths now
  normalize (dedup at every depth) on insert, making the merge fully idempotent.
  Surfaced by new property-based tests covering the Claude Code adapter
  serialization/validation paths and the merge engine. (#107, #108, #109, #110,
  #111)

### Added

- **ICM-aware Claude Code adapter** — the adapter now resolves three previously
  open design questions automatically. (1) It auto-derives
  `enabledMcpjsonServers` in `mcp.json` from every server llmenv emits, so the
  agent never prompts to approve a server llmenv itself configured; a
  `native` override of the key replaces the derived list rather than unioning
  with it. (2) When the resolved manifest includes the `icm` MCP server, the
  adapter emits `autoMemoryEnabled: false` so ICM and Claude's native auto
  memory don't both write (a user `native` override still wins). (3) The adapter
  always registers a `SessionStart` hook running the new `llmenv check-stale`
  subcommand, which compares the booted config folder against the one llmenv
  would materialize now and warns the user to restart on drift. (#121, #122,
  #123, #124)

- **Per-feature `native` overrides + top-level passthrough** — completes the
  two-layer engine-capabilities model: every modeled feature now has both an
  engine-neutral generic form and an engine-specific `native` override emitted
  verbatim. New top-level `native_<feature>` sibling maps (`native_permissions`,
  `native_hooks`, `native_plugins`, `native_mcp`), each a per-engine fragment;
  the Claude Code adapter deep-merges each onto its rendered subtree
  (`native_hooks` → `hooks`, `native_plugins` → settings top level, `native_mcp`
  → `mcp.json`). The top-level `native.<engine>` catch-all (for keys no modeled
  feature owns, e.g. `alwaysThinkingEnabled`) threads through `merge()` and is
  overlaid onto `settings.json` last. A modeled-feature key (`permissions`,
  `hooks`) in the catch-all now hard-errors instead of silently clobbering the
  security-rendered output (which would bypass the deny-never-weakened
  invariant) — it belongs in the `native_<feature>` sibling. Adds
  `util::merge_json` for the adapter-side overlay. (#96, #97, #102)

- **Plugin + marketplace support** — `marketplace:` and `plugin-collection:` are
  now first-class top-level config blocks, selected onto a scope by tag
  intersection (same model as bundles and MCP servers). Plugins are written as
  `marketplace:plugin` refs. Git marketplace sources are cloned once into
  `<cache_dir>/marketplaces/<name>/` (shared across scopes, fast-forwarded by
  `llmenv plugin sync`); local-path sources are used in place. The resolved git
  HEAD is mixed into the materialized scope hash so a marketplace update
  re-renders. The Claude Code adapter renders `extraKnownMarketplaces` (as
  `directory` sources pointing at the local clone) and `enabledPlugins`
  (`plugin@marketplace`, all enabled) into `settings.json`. New CLI commands:
  `marketplace-ls`, `plugin-ls`, `plugin sync`; `doctor` flags orphan collections
  and unreferenced marketplaces. The internal resolved model is engine-agnostic so
  a future Codex adapter can reuse it. (#59)

- **`settings.json` permission rendering** — the Claude Code adapter now renders
  engine-neutral permission rules (`{tool, pattern}` / `{tool, paths}`) into
  Claude's `Tool(pattern)` string grammar, landing in flat
  `permissions.{allow,ask,deny}` arrays alongside verbatim
  `permissions.native.claude_code` rule strings. `default_mode` maps to
  `defaultMode`. Native suppression is directional: a native `deny` overrides a
  neutral `allow`/`ask` of the same string, but a native `allow` never weakens a
  neutral `deny` (deny is authoritative). The two-layer invariant — every major
  feature gets a generic form plus an engine-specific `native` override — is now
  documented in `docs/design/engine-capabilities.md`. (#34)

- **CLI color support** — `--color <auto|always|never>` mode with
  `should_use_color()` honoring `NO_COLOR` and `CLICOLOR_FORCE` env vars plus TTY
  detection. Color glyph helpers centralized in `src/cli/style.rs`. `tag-ls`,
  `scope-ls`, `bundle-ls`, `doctor`, and `status` emit colored markers (active
  `*`, `(inactive)`, `(orphan)`, doctor `✓`/`⚠`/`✗`); `export` output stays plain
  so its shell-eval'd stdout never carries escape codes. (#62)
- **`llmenv prune` command** — subcommand with `--all`, `--older-than
  <duration>`, and `--dry-run` flags (`--all`/`--older-than` mutually exclusive,
  durations parsed via `humantime`). `cache::prune()` performs the on-disk sweep:
  deletion is symlink-safe (links are unlinked, never followed), orphaned `*.tmp`
  staging dirs are always cleared, and `--older-than` only ages out
  current-version cache folders so the staleness and age axes stay orthogonal.
  (#63)
- **Doctor diagnostic command** (`llmenv doctor`) with full health checks
  - Validates configuration file parsing
  - Checks cache directory writability
  - Tests git remote connectivity
  - Optional garbage collection with `--gc` flag
  - Configurable cache retention via `cache_retention_hours` setting

- **User documentation**
  - `docs/getting-started.md` — Installation and quick start guide
  - `docs/configuration.md` — Complete configuration schema with examples
  - `docs/icm-topology.md` — MCP server integration guide
  - Updated README with feature overview and examples

- **Cache retention configuration**
  - New optional setting `cache_retention_hours` in `[settings]` section
  - Defaults to 168 hours (7 days)
  - Garbage collection removes stale cache entries and orphaned `.tmp` directories

- **Scope matching infrastructure**
  - Network scope matching via WiFi SSID
  - Host scope matching via hostname
  - User scope matching via OS user
  - Project scope matching via project markers (e.g., `.llmenvrc`)

- **Bundle system**
  - Environment variable bundles with tag-based activation
  - Multiple bundles can be active simultaneously
  - Tag filtering for selective bundle activation

- **Shell integration**
  - Automatic scope evaluation via shell hooks
  - Support for zsh and bash
  - Throttled configuration sync (respects sync interval)

- **Git sync**
  - Automatic commit and push of configuration changes
  - `llmenv sync` command for on-demand synchronization
  - Configurable sync interval

- **MCP server integration**
  - Scope-aware activation of Model Context Protocol server
  - Automatic process lifecycle management
  - Server binding configuration

### Changed

- **Config format is now YAML** — configuration lives at
  `~/.config/llmenv/config.yaml` (was `config.toml`), `llmenv init` emits YAML,
  and project marker files are parsed as YAML. The list-heavy scope/bundle schema
  reads far more compactly in YAML. Dropped the `toml` dependency and migrated off
  the deprecated `serde_yaml` crate to the maintained `serde_yaml_ng` fork. (#76)

### Fixed

- **Path traversal detection now parses path components** instead of substring
  matching, so traversal the old checks missed — e.g. a trailing `foo/..` with
  no slash — is rejected. New `paths::has_parent_component` helper backs both
  `cache_dir` (#65) and `project.path_prefix` validation. (#65)
- Improved error messages with context using `anyhow`
- Fixed shell variable name validation
- Added proper escaping for shell metacharacters in exported variables

### Security

- **Adapter-returned env var names are validated at the source** (in
  `build_and_materialize`) in addition to the final emission loop, so no future
  emission path can smuggle a name that breaks the `export NAME=...` shell
  contract. (#67)

### Hardening

- **Documented the cache-key invariant** in `materialize::cache::hash_manifest`:
  the key is a function of (relative path, file contents) only and deliberately
  excludes the absolute source path, so a bundle reached via a symlink or alias
  reuses the cache rather than missing it. Guarded by a new regression test.
  (#66)

### Documentation

- Complete API documentation for all public modules
- Configuration examples for common use cases
- Troubleshooting guide in MCP documentation
- Comprehensive security/performance/property-test audit
  (`docs/superpowers/specs/2026-05-26-comprehensive-audit.md`); follow-ups
  filed as #65–#73. (#56)
- **Documented `Config::load`'s path-expansion contract** — callers must expand
  `~` before calling; a `debug_assert` enforces this in debug builds. (#68)
