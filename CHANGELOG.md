# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/),
and this project adheres to [Semantic Versioning](https://semver.org/).

<!-- next-header -->

## [Unreleased] - ReleaseDate

## [1.0.11] - 2026-06-15

## [1.0.10] - 2026-06-11

### Added

- `llmenv plugin-sync` now fetches externally-sourced plugins — those whose
  `source` in `marketplace.json` is a git URL rather than a relative path
  within the marketplace clone. Payloads are cloned to a stable path outside
  the hash-keyed config dir so they survive config changes without requiring
  a manual `/plugin install` or re-authentication (#353)

### Fixed

- Fix `env:` declared in a bundle's `bundle.yaml` being silently dropped;
  bundle-level env vars are now merged and exported alongside `Bundle.vars`
  (#351)
- Reject reserved env var names (`CLAUDE_CONFIG_DIR`, `LLMENV_STATE_DIR`) and
  the `LLMENV_*` prefix in `capabilities.env` at validation time; silently
  setting these would shadow adapter-emitted vars and produce conflicts that
  are impossible to diagnose at runtime (#354)
- Detect same-precedence conflicts in `capabilities.env` key merging and error
  with the contributor names and values, matching the existing `default_mode`
  conflict behaviour; previously one of the conflicting values would silently
  win (#355)

## [1.0.9] - 2026-06-10

### Fixed

- Fix `memory.listen_host` unspecified-address warning emitting on every shell
  prompt; the warning now only appears when the ICM proxy actually starts or
  restarts (#347)

## [1.0.8] - 2026-06-09

### Added

- Memory server now supports a `listen_host` option under `features.memory`
  (default `"127.0.0.1"`). Set to `"0.0.0.0"` to accept connections on all
  interfaces, or to a specific IP to bind to one interface. Fixes #337.

### Fixed

- Fix shell hook functions (`__llmenv_precmd`, `__llmenv_prompt`) triggering a
  full environment render inside non-interactive subshells (e.g. Claude Code's
  Bash tool); add early-return guards for both `$-` interactivity and
  `$LLMENV_STATE_DIR` already-active checks (#338)
- Fix empty directories left in rendered output when a bundle contributes no
  files to a subdirectory; `create_dir_all` is now followed by a bottom-up
  prune pass that removes empty dirs without touching the output root (#336)

## [1.0.7] - 2026-06-05

### Added

- Add `mcp:` support in `bundle.yaml`; declare MCP servers inside a bundle using
  the same format as `config.yaml`; tagless entries are active whenever the bundle
  is selected, tagged entries are further filtered by active scope tags (#329)
- `llmenv init` now generates a `README.md` orientation file in the config
  directory on first run; the write is skipped if a `README.md` already exists
  (#325)

### Fixed

- Fix bundle `mcp:` entries accepting names with characters outside
  `[a-zA-Z0-9_-]`; invalid names are now rejected with a clear error (#329)
- Fix missing collision detection between `config.mcp` and bundle `mcp:` entries;
  a name declared in both sources now errors at startup instead of silently
  producing duplicate servers (#329)
- Fix `mcp-ls` omitting bundle-declared MCP servers; bundle MCPs are now listed
  with a `(bundle)` annotation and correct active/orphan status (#329)
- Fix bundle `mcp:` entries accepting the reserved name `icm`; the guard now
  matches the one already present for top-level `config.mcp` (#329)
- Fix `llmenv init` emitting a config.yaml template with a nested `transport:`
  block for MCP servers; the correct flat schema (`type`/`command`/`args` at the
  top level) is now emitted (#325)
- Fix `llmenv init` silently replacing non-UTF-8 path bytes with `?`; non-UTF-8
  paths now fail with a clear error (#325)

## [1.0.6] - 2026-06-05

### Added

- Add GitHub Actions workflow to auto-close issues when PRs merge to `release/*`
  branches; GitHub's native auto-close only works on the default branch, so this
  workflow parses merged PR bodies for closing keywords and closes referenced
  issues via the API
- Add GitHub Actions workflow to forward-merge `release/*` branches through the
  release chain into `main`; a fix pushed to an older release line cascades
  forward through newer lines automatically, opening a labeled PR (and halting)
  on the first conflict or protected branch instead of being dropped

### Fixed

- Fix `mcp-proxy` spawned during `llmenv export` inheriting the calling shell's
  stdio; when the export was sourced over SSH via `source <(llmenv export)` the
  proxy wrote its logs into the process-substitution pipe, flooding the terminal
  with `command not found: INFO:` lines. The proxy now redirects stdio to
  `/dev/null` and starts in its own process group so terminal job-control
  signals no longer reach it
- Fix `llmenv sync` silently reporting success when `git push` failed; a
  rejected or failed push is now surfaced as an error with git's own message
- Fix git operations potentially hanging on a credential prompt when run with a
  non-interactive stdin (CI, or a sourced `llmenv export`); all git subprocesses
  now detach stdin so they fail fast instead of blocking
- Fix materialized skills failing silently when they referenced bundled scripts
  via hardcoded `~/.claude` paths; such paths resolve against the default config
  dir, not the materialized folder llmenv actually boots. Materialization now
  rejects skills (and rules/CLAUDE.md) carrying `~/.claude` or `$HOME/.claude`
  paths, naming the offending file
- Fix marketplace `git clone`/`fetch` failures hiding git's diagnostic output;
  the underlying stderr is now surfaced (auth failure, bad URL, disk full are
  distinguishable) with any embedded credentials scrubbed from the message
- Fix `llmenv` config auto-pull silently swallowing a failed fast-forward
  (diverged history, conflict, network); a one-line nudge now points at
  `llmenv sync` instead of failing invisibly on every shell prompt

## [1.0.5] - 2026-06-03

### Changed

- GitHub release notes now include inline SHA256 checksums and the changelog
  section for the released version; checksums no longer require downloading a
  separate `checksums.txt` attachment to verify

### Fixed

- Fix documentation referencing `mcp.json` for MCP server configuration;
  servers have been written to `mcpServers` in `.claude.json` since v1.0.0
- Fix `state:` key and `features.memory:` subsection missing from
  configuration reference
- Fix `hook-run` command and command aliases (`scopes`, `tags`, `bundles`,
  `mcps`, `marketplaces`, `plugins`) missing from commands reference
- Add SLSA provenance verification instructions to release documentation;
  SLSA artifacts have been published since v1.0.0 but were undocumented

## [1.0.4] - 2026-06-03

Aborted release. CI pipeline issue.

## [1.0.3] - 2026-06-03

### Fixed

- Fix `reconcile_settings` silently dropping native passthrough keys (e.g.
  `statusLine`, `cleanupPeriodDays`) on re-renders when `settings.json` already
  exists; non-owned keys from `fresh` are now written through on every render

## [1.0.2] - 2026-06-02

### Fixed

- Fix marketplace sync failure silently dropping `CLAUDE_CONFIG_DIR` on export;
  missing local clone now warns and continues rather than propagating an error
  that exited 0 without emitting the env var (#281)
- Fix `run_export` allowing `build_and_materialize` failures to exit 0 without
  emitting `CLAUDE_CONFIG_DIR`; build failures now exit non-zero (#281)
- Fix materialize creating empty cache directories when source bundles are
  deleted or moved (#285)
- Fix `doctor` falsely reporting marker-enabled bundles (e.g. `rust-dev`,
  `python-dev`) as orphans when no project marker is currently active (#284)
- Fix `doctor` suppressing legitimate orphan warnings due to overly-broad
  marker-driven heuristics matching non-marker bundles and tags
- Add remediation hint (`llmenv plugin-sync`) to marketplace unavailability
  warning during export

## [1.0.1] - 2026-06-02

### Added

- Add changelog to Docusaurus documentation site (#258)

### Fixed

- Fix documentation links in README; correct missing `/docs/` path segment in
  several links (#265, #266)

## [1.0.0] - 2026-06-01

### Added

- Add `llmenv doctor` diagnostic command with config, cache, and git health
  checks; `--gc` flag for garbage collection; `cache_retention_hours` setting
  (default 168 hours)
- Add `llmenv prune` command with `--all`, `--older-than <duration>`, and
  `--dry-run` flags; symlink-safe deletion, orphaned `*.tmp` staging dirs always
  cleared (#63)
- Add `llmenv sync` command for on-demand configuration synchronization with
  configurable sync interval
- Add `hook-run` command for engine-neutral lifecycle event dispatching
  (`session_start`, `turn_start`, `session_end`); hooks degrade gracefully on
  failure so they never block the agent (#171)
- Add ICM-aware Claude Code adapter: auto-merges MCP servers into `.claude.json`,
  suppresses native auto-memory when ICM is active, and registers
  `check-stale` `SessionStart` hook for drift detection (#121, #122, #123, #124)
- Add per-feature `native` override maps (`native_permissions`, `native_hooks`,
  `native_plugins`, `native_mcp`) for engine-specific config passthrough; catch-all
  `native.<engine>` block for unmodeled keys; modeled-feature keys in the catch-all
  are a hard error (#96, #97, #102)
- Add first-class plugin and marketplace support with git and local sources;
  Claude Code adapter renders `extraKnownMarketplaces` and `enabledPlugins` into
  `settings.json`; new `marketplace-ls`, `plugin-ls`, and `plugin sync` CLI
  commands (#59)
- Add engine-neutral permission rule rendering into Claude Code `settings.json`
  with native suppression (deny is authoritative over allow/ask) (#34)
- Add cross-project tag-scoped memory recall via `turn_start` hook; tags
  validated before expansion to prevent metacharacter injection (#197)
- Add `--color <auto|always|never>` flag with `NO_COLOR` and `CLICOLOR_FORCE`
  support; colored markers in `tag-ls`, `scope-ls`, `bundle-ls`, `doctor`, and
  `status` (#62)
- Add scope matching via WiFi SSID, hostname, OS user, and project markers
  (e.g. `.llmenvrc`)
- Add bundle system for tag-activated environment variable groups; multiple
  bundles can be active simultaneously
- Add zsh and bash shell integration with throttled configuration sync via
  shell hooks
- Add scope-aware MCP server integration with automatic process lifecycle
  management and server binding configuration
- Add MIT and Apache-2.0 license texts with per-dependency attribution via
  `cargo-about`; `cargo deny` gates license policy in CI and on pre-push (#253)
- Add user documentation: getting-started guide, configuration schema reference,
  ICM topology/MCP integration guide, and updated README

### Changed

- **BREAKING**: Replace two-knob `cache.hashing: strict|version` +
  `cache.version_fidelity` config with single `cache.hashing: loose|normal|strict`
  (default `normal`); `normal` → `<adapter>/<version_mm>/<shape>/`, `loose` →
  `<adapter>/<shape>/`, `strict` → `<adapter>/<VERSION_TAG>-<content_hash>/`;
  existing configs using the old keys must migrate (#246)
- **BREAKING**: Write MCP servers to `mcpServers` object in `.claude.json`
  instead of standalone `mcp.json`; foreign keys are preserved on
  read-modify-write merge; remote servers now carry an explicit `"type"` field;
  `enabledMcpjsonServers` is no longer emitted (#244)
- Change config format from TOML to YAML (`~/.config/llmenv/config.yaml`
  replaces `config.toml`); `llmenv init` emits YAML; migrated from deprecated
  `serde_yaml` to `serde_yaml_ng` (#76)
- Change `hook-run` from multi-threaded to current-thread tokio runtime,
  reducing startup overhead on the agent hot path; fail-soft contract locked by
  integration tests (#186, #187, #189)

### Fixed

- Fix `llmenv prune` counting symlinks as removed when unlink failed; failures
  are non-fatal but now logged and reported under a separate `failed` list (#255)
- Fix corrupt `.llmenv-manifest.json` being discarded silently; parse failure
  now emits a `tracing::warn!` (#247)
- Fix deep-merge producing duplicate sequence entries, making
  `merge(merge(x)) != merge(x)`; all write paths normalize on insert (#107,
  #108, #109, #110, #111)
- Fix path traversal detection to parse path components instead of substring
  matching; catches trailing `foo/..` patterns the old checks missed (#65)
- Fix shell variable name validation
- Fix shell metacharacter escaping in exported variables
- Improve error messages with operation context and actionable guidance

### Security

- Validate env var names at source in `build_and_materialize` in addition to
  the final emission loop, preventing injection in the `export NAME=...` shell
  contract (#67)

<!-- next-url -->
[Unreleased]: https://github.com/phaedrus1992/llmenv/compare/v1.0.11...HEAD
[1.0.11]: https://github.com/phaedrus1992/llmenv/compare/v1.0.10...v1.0.11
[1.0.10]: https://github.com/phaedrus1992/llmenv/compare/v1.0.9...v1.0.10
[1.0.9]: https://github.com/phaedrus1992/llmenv/compare/v1.0.8...v1.0.9
[1.0.8]: https://github.com/phaedrus1992/llmenv/compare/v1.0.7...v1.0.8
[1.0.7]: https://github.com/phaedrus1992/llmenv/compare/v1.0.6...v1.0.7
[1.0.6]: https://github.com/phaedrus1992/llmenv/compare/v1.0.5...v1.0.6
[1.0.5]: https://github.com/phaedrus1992/llmenv/compare/v1.0.4...v1.0.5
[1.0.4]: https://github.com/phaedrus1992/llmenv/compare/v1.0.3...v1.0.4
[1.0.3]: https://github.com/phaedrus1992/llmenv/compare/v1.0.2...v1.0.3
[1.0.2]: https://github.com/phaedrus1992/llmenv/compare/v1.0.1...v1.0.2
[1.0.1]: https://github.com/phaedrus1992/llmenv/compare/v1.0.0...v1.0.1
[1.0.0]: https://github.com/phaedrus1992/llmenv/releases/tag/v1.0.0
