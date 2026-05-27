# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/),
and this project adheres to [Semantic Versioning](https://semver.org/).

## [Unreleased]

### Added

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
