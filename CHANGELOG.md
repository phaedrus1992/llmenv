# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/),
and this project adheres to [Semantic Versioning](https://semver.org/).

<!-- next-header -->

## [Unreleased] - ReleaseDate

## [1.0.1] - 2026-06-02

### Added

- **CHANGELOG on docs site** — the project changelog is now browseable at the
  Docusaurus documentation site alongside the rest of the docs. (#258)

### Fixed

- **README doc links** — all relative documentation links in the README
  replaced with absolute Docusaurus URLs; a missing `/docs/` path segment in
  several links corrected. (#265, #266)

### Changed

- **CI path filters** — workflow jobs now run only when files relevant to that
  job change (source, docs, CI configs), avoiding spurious runs on unrelated
  commits.

## [1.0.0] - 2026-06-01

### Changed

- **Cache hashing strictness dial (BREAKING)** — replaced the two-knob
  `cache.hashing: strict|version` + `cache.version_fidelity: major_minor|...`
  config with a single `cache.hashing: loose|normal|strict` (default `normal`).
  Folder layout follows the mode: `loose` → `<adapter>/<shape>/`, `normal` →
  `<adapter>/<version_mm>/<shape>/`, `strict` →
  `<adapter>/<VERSION_TAG>-<content_hash>/`, where `shape` is a 12-hex SHA-256
  over the active tags ∪ enabled bundles. The plaintext selection set
  (`active_tags`, `enabled_bundles`) is now recorded in
  `.llmenv-manifest.json`, and pruning is mode-aware (`state/` is never
  touched). Existing configs using `hashing: version`/`strict` or
  `version_fidelity` must migrate to the new single key. (#246)

- **MCP servers written to `.claude.json` (BREAKING)** — the Claude Code adapter
  now merges resolved MCP servers into the top-level `mcpServers` object of
  `.claude.json` (the surface Claude Code actually reads) instead of writing a
  standalone `mcp.json` that Claude never ingested. The merge is read-modify-write:
  llmenv servers are upserted by name and every foreign key (`oauthAccount`,
  `projects`, `numStartups`, hooks, …) is preserved; a corrupt or non-object
  `.claude.json` is a hard error rather than being overwritten. Remote servers
  now carry an explicit `"type"` (`http`/`sse`). `mcp.json` is no longer written,
  and `enabledMcpjsonServers` (a project `.mcp.json` approval gate, irrelevant to
  auto-trusted user-scoped servers) is no longer emitted. (#244)

### Fixed

- **`prune` no longer reports un-removed symlinks as removed** — a symlink whose
  unlink failed was still counted in the `removed` list, so `llmenv prune`
  claimed deletions it never made. The failed unlink stays non-fatal (pruning
  continues) but is now logged and reported under a separate `failed` list, and
  the CLI surfaces "failed to remove" lines and a count. (#255)

- **Corrupt cache manifest now logged** — a `.llmenv-manifest.json` that fails
  to parse is still treated as "no prior knowledge" (non-fatal, the documented
  behavior), but the discard now emits a `tracing::warn!` instead of being
  swallowed silently, so the degraded re-render is observable. (#247)

- **Deep-merge non-idempotence** — `util::merge_json` / `merge_yaml` and
  `merge::capabilities::merge_native_feature` could keep duplicate
  sequence/array elements on the insert and scalar-overwrite paths that a later
  merge would dedup away, so `merge(merge(x)) != merge(x)`. All write paths now
  normalize (dedup at every depth) on insert, making the merge fully idempotent.
  Surfaced by new property-based tests covering the Claude Code adapter
  serialization/validation paths and the merge engine. (#107, #108, #109, #110,
  #111)

### Added

- **Third-party license attribution + dual-license texts** — added the
  `LICENSE-MIT` and `LICENSE-APACHE` texts for the project's `MIT OR Apache-2.0`
  license, and generate per-dependency attribution with cargo-about into two
  outputs: `THIRD-PARTY-LICENSES.md` (ships with the binary/source dist) and
  `website/docs/third-party-licenses.md` (browseable on the docs site).
  `cargo deny check` now gates the license policy in CI and on pre-push, and the
  allowlist was audited and extended (`MIT-0`, `BSD-3-Clause`, `ISC`,
  `CDLA-Permissive-2.0`) — all permissive or weak/file-scoped copyleft, mutually
  compatible for the dual-licensed binary. (#253)

- **Cross-project tag-scoped memory recall** — the `turn_start` hook now issues,
  in addition to the existing project-scoped recall, one **project-unfiltered**
  recall per active tag keyed on that tag's `llmenv-tag:<tag>` keyword. Memory
  stored under a tag in one project surfaces when the same tag activates in
  another, completing the cross-project promise of the write side (#81). Tags are
  validated before expansion so a malformed scope can't inject recall
  metacharacters. Bundle-scoped recall (`llmenv-bundle:<bundle>`) remains
  documented-but-unimplemented (#215). (#197)

- **Lifecycle memory hooks** — new `hook-run` command provides engine-neutral
  lifecycle event dispatching for ICM memory integration. Three events (`session_start`,
  `turn_start`, `session_end`) trigger corresponding ICM actions (`icm_wake_up`,
  `icm_memory_recall`, `icm_memory_store`). Hooks auto-activate when a memory
  backend is configured; failures degrade gracefully with warnings (exit 0) so
  hooks never block the agent. (#171)

- **ICM-aware Claude Code adapter** — the adapter now resolves three previously
  open design questions automatically. (1) It merges every resolved MCP server
  into the top-level `mcpServers` of `.claude.json` — the surface Claude actually
  reads — so user-scoped servers are auto-trusted and never prompt (the earlier
  `enabledMcpjsonServers` approach targeted the dead `mcp.json` and was removed;
  see the #244 entry below). (2) When the resolved manifest includes the `icm` MCP server, the
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

- **Lifecycle hooks run on a current-thread tokio runtime** — `hook-run` fires on
  the agent's hot path (session start and every prompt turn) and only does one
  `block_on` over a short sequential chain of HTTP round-trips, so the
  multi-threaded runtime's worker pool was pure startup overhead. Swapped to
  `Builder::new_current_thread().enable_all().build()`; the 2s timeout and
  fail-soft behavior are unchanged. Added integration tests driving the real
  `hook-run` binary to lock in the fail-soft contract (exit 0 + stderr warning on
  unknown event, no backend, malformed/SSRF-rejected URL, unreachable backend).
  (#186, #187, #189)

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

<!-- next-url -->
[Unreleased]: https://github.com/phaedrus1992/llmenv/compare/{{tag_name}}...HEAD
[1.0.1]: https://github.com/phaedrus1992/llmenv/compare/v1.0.0...{{tag_name}}
[1.0.0]: https://github.com/phaedrus1992/llmenv/releases/tag/v1.0.0
