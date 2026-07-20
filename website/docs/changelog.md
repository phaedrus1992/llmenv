---
id: changelog
title: Changelog
slug: /changelog
sidebar_label: Changelog
---

{/* GENERATED FILE — do not edit by hand. Regenerate with `scripts/sync-changelog-doc.sh`. */}

# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/),
and this project adheres to [Semantic Versioning](https://semver.org/).

## Version 4.x

## [Unreleased] - ReleaseDate

### Added
- Add Opencode engine adapter (`src/adapter/opencode.rs`) — full
  feature parity with the Claude Code adapter: renders
  `opencode.json` (MCP, LSP, permissions, env vars), `AGENTS.md`
  with frontmatter translation, rules, and a JS hook bridge shim
  that maps Opencode plugin events to llmenv hook subprocess calls
  with Claude-shaped stdin payloads. Plugin content (skills,
  commands, agents, MCP) from Claude Code bundles is translated
  into Opencode-native forms ([#657](https://github.com/phaedrus1992/llmenv/issues/657))
- Add JSON Schema generation for materialized configs —
  `llmenv materialize` now emits a `schema.json` sidecar alongside
  the rendered engine config, describing the full type shape of the
  output for validation and tooling ([#660](https://github.com/phaedrus1992/llmenv/issues/660))
- Add model provider configuration rendering to Claude Code and
  Crush adapters — `capabilities.model_providers` and
  `capabilities.default_models` are now rendered into engine-native
  config forms ([#682](https://github.com/phaedrus1992/llmenv/issues/682))
- Add stale MCP server pruning to the Claude Code adapter — servers
  previously owned by llmenv but absent from the resolved set are
  removed from `.claude.json`, preserving user-added servers
  ([#739](https://github.com/phaedrus1992/llmenv/issues/739))
- Add tiered MCP permission rules for built-in servers (ICM,
  context-mode) — read-only tools are auto-allowed, mutation tools
  prompt the user, and destructive tools are denied, matching the
  sensitivity tier of each tool
  ([#694](https://github.com/phaedrus1992/llmenv/issues/694))

### Changed
- **Breaking:** Remove the deprecated boolean `session_log` shape
  (`file: bool`, `transcript: bool`, `verbose: bool`). Configs
  using the old format must migrate to the per-sink mapping blocks
  introduced in 3.3.0 ([#744](https://github.com/phaedrus1992/llmenv/issues/744))

### Fixed
- Fix opencode hook shim generating misleading warning when bundle
  path resolution fails — diagnostic now correctly describes stale
  or restructured bundles ([#769](https://github.com/phaedrus1992/llmenv/issues/769))
- Fix `split_frontmatter` crash on empty/single-delimiter input in
  the opencode adapter ([#769](https://github.com/phaedrus1992/llmenv/issues/769))
- Fix silent `remove_file` error discard in claude_code companion
  file cleanup — now emits `tracing::warn!` on failure
- Add `tracing::warn!` diagnostics to `read_owned_servers` I/O and
  parse error paths

## Version 3.x

## [Unreleased] - ReleaseDate

### Added
- Add an in-engine task tracker: `llmenv task add|start|done|ls|show|note|block` manages a durable, file-based task store so agents can track "what am I working on" across `/clear`, `/compact`, and new sessions instead of relying on ephemeral in-session TODOs. Off by default (`features.task_tracker.enabled: true` to opt in) — when enabled, a CLAUDE.md fragment steers the agent to use it, and SessionStart/Stop hook reminders nudge the agent to resume or close `wip` tasks (#231)
- First-class `llmenv statusline` subcommand: reads engine session JSON from stdin, config from the new `statusline:` section of `config.yaml`, and llmenv's own stats (active scopes, plugin/MCP counts, ICM memory stats, throttle state, cache health, config staleness, session log activity) from a materialized `llmenv-status.json` data file. Supports 18 widget types — 10 engine-sourced (`model`, `folder`, `branch`, `pr`, `progress_bar`, `tokens`, `context_pct`, `budget`, `duration`, `cache_pct`) and 8 llmenv-sourced (`scopes`, `plugins`, `mcps`, `icm`, `cache`, `config_stale`, `throttle`, `session_log`) — with per-widget `format`/`style`/`max_len` overrides, configurable row templates, and a configurable icon set (`auto`/`nerd`/`simple`/`none`). The Claude Code adapter seeds `llmenv statusline` as the default `statusLine` hook in `settings.json` automatically, without overwriting an existing user customization. Crush has no statusline hook concept yet (#855 tracks adding it) (#836)
- Opt-in per-phase hook-run timing via `LLMENV_TRACE_TIMING` env var — emits a single `llmenv-trace {json}` stderr line with config-load/scope-eval/prep/mcp phase durations in microseconds; off by default, stdout unaffected
- `llmenv doctor` flags `hook.matcher` values shaped like file-extension globs (e.g. `*.rs`, `.py`) — Claude Code matches `hook.matcher` against tool name only, never file path, so these silently never fire; warning points at `scope.content` to gate a bundle by file type instead (#837)
- Add `features.codebase_memory` — first-class integration for [codebase-memory-mcp](https://github.com/DeusData/codebase-memory-mcp), a local code-intelligence MCP server. Tag-activated entries (`when`, optional `index_path`) materialize as a local stdio MCP server per matching project scope — unlike the `memory:` (ICM) backend, codebase-memory-mcp has no remote-serve mode, so there's no `server_host`/`port` to configure, and multiple entries can be active at once. llmenv always computes `CBM_CACHE_DIR` (index storage) and `CBM_ALLOWED_ROOT` (restricts indexing to the project root) for the launched process. `SessionStart` fires a fire-and-forget `index_repository` call that registers the project with the server's own background auto-watch, so the index stays current without llmenv re-implementing reindex scheduling. `llmenv doctor` checks the binary is on `PATH` and flags entries whose tags no scope emits; `llmenv status mcps` reports activation. Fully independent of `features.memory` — both can be active simultaneously (#365)
- Backport the opencode adapter: `opencode` is now a third supported engine alongside `claude_code` and `crush`. PATH-gated like Crush (skipped silently when `opencode` is not on `PATH`), it materializes `opencode.json`, `AGENTS.md`, rule files, skills (`SKILL.md`), plugin-translated `command/` + `agent/` files, and a generated `plugin/llmenv.js` hook-bridge shim into the llmenv cache dir — discovered via the exported `OPENCODE_CONFIG_DIR`. It reaches near-parity with the Claude Code adapter: permissions rendered as per-tool `pattern → action` maps with native `allow`/`ask`/`deny` (a bare tool emits a plain action string), six hook events (`SessionStart`, `SessionEnd`, `UserPromptSubmit`, `PreToolUse`, `PostToolUse`, `Stop`) bridged through the JS shim, MCP servers (local with `${HOME}` expansion plus remote `http`/`sse`), LSP servers, first-class and plugin-projected skills, and custom agents/commands. Unsupported hook events and `mcp_tool`-kind handlers are dropped with an actionable warning rather than a hard error. A `native.opencode` escape hatch deep-merges catch-all keys into `opencode.json` while rejecting the modeled keys (`instructions`, `mcp`, `lsp`, `permission`) that must go through the `native_permissions`/`native_hooks`/`native_mcp.opencode` siblings. `llmenv setup` and `llmenv doctor` probe for `opencode`, and `disabled_engines` accepts `opencode` (#876)

### Changed
- Evaluate all `scope.content` matchers in a single directory walk instead of one walk per matcher — N active content scopes previously meant N full tree walks on every hook fire and every export (#703)
- Resolve the hostname via the `uname(2)` syscall instead of spawning the `hostname` binary on every hook-run — the fork/exec dominated hook-run scope evaluation (~15ms/event, ~35% of hook-run CPU); each hook is a fresh process so the process-static env cache never helped this path
- Deduplicate byte-identical memory blocks across a TurnStart's recalls before injecting them into agent context — a memory stored under several tag/bundle keywords came back from multiple recalls and was injected 2–3× (~60% of the TurnStart context payload in the common case); only exact-duplicate blocks are dropped, order preserved, so no unique recall is lost
- Skip gateway-MAC detection (`route`+`arp` subprocess forks) on hook-triggered paths — the synchronous hook-run and the detached memory-store, consolidation, and session-log children it spawns — when no `network` scope is configured; nothing can match the gateway MAC then, and each is a fresh process so the env cache never covered it, so the two forks were pure waste dominating the remaining hook-run scope-evaluation cost
- Skip a redundant second `config.yaml` parse on the hook-run path — `main()` already loads it once to resolve session-log settings before the tracing subscriber is set up; the loaded config is now cached and reused instead of parsing the file again inside `hook_run::run`
- Skip loading `config.yaml` entirely for `--version`/`-V` — the version flag never touches config or any hook, so the load was pure overhead on an otherwise config-free startup path
- Reuse the config `main()` already loaded in `llmenv export`, `llmenv regenerate`, and `llmenv statusline` instead of re-parsing `config.yaml` a second time in the same process

### Fixed
- Bundle- and user-declared hooks no longer emit null-valued `tool`/`command` keys into the generated engine config — the Claude Code adapter rendered `"tool": null` for a `command`-type handler (and `"command": null` for an `mcp_tool` handler), and the Crush adapter rendered `"command": null` when a command hook had no command; absent fields are now omitted in both adapters (#720)
- Skill frontmatter `name`/`description` values containing control characters (e.g. a stray vertical tab) no longer produce invalid YAML when auto-quoted — control characters are now escaped instead of passed through literally (#859)
- `features.read_once` no longer silently drops Debug-level session-log capture for `PreToolUse` events — enabling it previously short-circuited before session logging ran whenever a Debug-level session-log sink was also configured; both now fire (#864)
- A computed `read_once` deny/advisory result is no longer silently discarded if an unrelated hook-run pipeline error (e.g. invalid tag/bundle config, memory URL resolution failure) occurs afterward — it's now still returned instead of being lost when the pipeline errors out (#867)
- `SessionEnd` session-log capture is no longer silently skipped when the redundant-store dedup check fires — previously any configured session-log sink missed `SessionEnd` events whenever the context chunk was unchanged since the last store; only the redundant store is skipped now, not the log (#866)
- Skill frontmatter `name`/`description` values containing Unicode noncharacters (e.g. U+FFFE) no longer produce invalid YAML when auto-quoted — these are now escaped like control characters, and code points above U+FFFF use the correct 8-digit `\U` escape instead of a truncated 4-digit `\u` one that corrupted the scalar (#873)
- opencode adapter: a `native_permissions.opencode` `allow` rule no longer silently overrides a structured `permissions.deny` rule for the same tool+pattern — permission-map insertion is now interleaved by action tier (all `allow`, then all `ask`, then all `deny`, structured before native within each tier) so `deny` always wins regardless of source, instead of native rules always being inserted last (#877)
- opencode adapter: a malformed `native_permissions.opencode` rule string (missing/unbalanced parentheses, an empty tool name, an empty pattern, or an empty rule) no longer silently falls back to a wildcard-allow-all pattern for the tool — e.g. a typo like `Bash(` previously granted blanket Bash permission with no feedback; it's now a clear error naming the offending rule and which action list (`allow`/`ask`/`deny`) it came from (#882)
- A hook whose handler `type` didn't match its populated field — a `command`-type handler with no (or an empty) `command`, or an `mcp_tool`-type handler with no (or an empty) `tool` — no longer silently loads as a no-op hook; `config.yaml` now fails to load with an error naming the offending hook's event (#851)
- A computed `read_once` deny result could be silently overridden by other hook output — the invariant guarding it was only enforced via `debug_assert!`, which compiles to nothing outside debug builds, leaving the sentinel unenforced in release builds; it's now an always-on guard, so a deny always wins regardless of build profile (#868)
- `config.yaml` now rejects a duplicate `scope.content` id, matching the existing check for `network`/`host`/`user` scopes — previously two content scopes sharing an id could both silently activate even when only one's glob matched (#843)
- Claude Code adapter: a `Write` permission rule (neutral `{tool: Write, ...}` or a verbatim `native_permissions.claude_code` string) is now rewritten to `Edit` before it reaches `settings.json` — Claude Code deprecated `Write(<path>)` in favor of `Edit(<path>)`, so the stale form previously only produced a "Fix:" warning on every session instead of matching anything (#888)

## [3.5.1] - 2026-07-15

### Fixed
- `remote_sync` no longer blocks manual `llmenv sync` and `llmenv plugin-sync` commands — it only gates the non-interactive throttled pull during `llmenv export` (#835)

## [3.5.0] - 2026-07-15

### Added
- Configurable session-log retention: `session_log.transcript.retention_days` — best-effort deletion of stale session-log files before each SessionStart; validated >= 1 (#812)
- Add `cache.remote_sync` config option (default `true`) to disable remote git operations — prevents shell freezes when 1Password's SSH agent is locked and an SSH askpass prompt hangs terminal-based git ops (#833)

### Changed
- Build manifest once per export/regenerate instead of once per adapter, reducing repeated work in multi-engine setups (#708)
- Hot-path optimizations for hook-run pipeline: cache Env::detect() results (30s TTL), cache bundle merge by config mtime, reuse Tokio runtime and MCP HTTP client via OnceLock (#813)

### Fixed
- Remove dead process-static CONFIG_CACHE from hook_run that never saved a parse (each hook event is a fresh process); poisoned-cache log no longer fires on cold-start misses (#706)
- Add eprintln! diagnostic when fs::canonicalize() fails in read-once, so operators can detect non-canonicalized cache keys (#728)
- Add eprintln! diagnostic when deprecated PascalCase 'filePath' key is used in read-once, surfacing format drift (#729)
- Preserve MCP server sub-keys (runtime auth tokens) across re-materialization in `merge_mcp_into_claude_json` — fixes silent auth loss on every materialize in Loose/Normal mode (#814)
- Fail fast on manifest build error with preserved error chain instead of silently falling back to stale manifest (#708)
- Gate git marketplace and external plugin sync behind `cache.remote_sync` to prevent hangs when remote sync is disabled
- Distinguish local-only commits from pushed commits — prints "Committed locally (remote sync disabled — push skipped)" instead of misleading "Synced config to GitHub" when remote_sync is off
- Add `## Version X.x` headers to the generated website changelog for correct section hierarchy across major versions

## [3.4.0] - 2026-07-14

This release tightens error diagnostic coverage across two dozen silent-fallthrough
sites, adds PermissionMode variants for granular permission control, hardens cache
GC edge cases, and normalizes JSON/YAML merge null-strip behavior.

### Fixed

- Fold `strip_json_nulls` into `normalize_json` so every merge path (not just
  `reconcile_settings`) benefits from null-tolerant merge dedup (#718)
- Add null-stripping to `normalize_yaml` and insert-path null guard to
  `merge_yaml` for YAML merge parity with JSON (#718)
- Session log transcript correlation (`session_log::state`) no longer
  silently fails when `state_dir()` is unavailable — falls back to CWD with
  a `tracing::warn!` instead of returning `None`/`Err` (#737)
- Add `tracing::warn!` diagnostics to 7 additional silent-error swallowing
  sites in file_sink, event serialization, read-once canonicalize, throttle
  error body, consolidation error body, and MCP client error body reads (#773)
- Enrich pre-subscriber diagnostics — promote event serialization failures
  to `error!`, add URL context to throttle/consolidation error messages,
  and log fallback path in `state_path()` warnings (#784)
- Surface silent error swallowing in read-once hook — `state_dir()`
  resolution failures are now logged as warnings before returning empty
  strings (#760)
- Surface silent error swallowing in doctor version skew check —
  `read_dir` failures on adapter cache directories are now logged as
  warnings instead of being silently skipped (#764)
- Surface silent error swallowing in login auth status update —
  `CacheManifest::read` failures are now logged as warnings instead of
  being silently skipped (#765)
- Surface silent error swallowing in auth, throttle, hook-run, and
  reconcile_settings — read/parse failures are now logged as warnings instead
  of being silently discarded (#749)
- Fix transcript session id parsing — ICM returns the session id as a JSON
  object, not a bare ULID, so every transcript record call was passing a JSON
  blob instead of a real id and records went nowhere (#755)
- Add diagnostics for walkdir entry errors in scope matcher — I/O errors
  during directory traversal are now logged as warnings instead of silently
  skipped (#752)
- Add diagnostics for project marker file read errors — read failures on
  `.llmenv.yaml` are now logged as warnings before returning defaults (#753)
- Add diagnostics for config-context stdin JSON parse failures — parse
  errors are now logged as warnings before falling back to SessionStart (#754)
- Surface silent error swallowing in settings.json parse — parse failures
  in `apply_seeded_settings` are now logged as warnings instead of silently
  returning defaults (#762)
- Surface silent error swallowing in version comparison — malformed version
  strings in `compare_versions` are now logged as warnings instead of silently
  returning `Equal` (#766)
- Surface silent error swallowing in session log path resolution — path
  resolution failures are now logged to stderr instead of silently falling
  back to CWD before the tracing subscriber is initialized (#763)
- Upgrade `debug_assert!` to `tracing::warn!` in scope matcher — walkdir
  entries outside the workspace root are now surfaced as warnings instead
  of only being checked in debug builds (#761)
- Remove angle brackets from bare URLs in changelog and release docs —
  `<url>` is interpreted as JSX by Docusaurus, breaking the `docs.yml`
  CI build against `website/docs/changelog.md` and `website/docs/release.md`
  (#811)

### Added

- Add `auto`, `dontAsk`, and `manual` PermissionMode variants alongside
  existing boolean/string forms — `auto` is only honored from user-scope
  settings, `dontAsk` skips the permission prompt, and `manual` matches
  the default deny-mode behavior (#748)
- Migrate ephemeral state (`projects/`) across hash changes in Strict
  mode materialization (#746, #797)

### Fixed

- GC in Normal mode now age-checks each shape individually instead of
  treating the entire version generation as one unit (#738, #797)
- Clock-skew handling in GC — entries with future mtimes are now
  treated as expired with a logged warning instead of silently skipped
  (#797)
- Edge-case hardening in cache lifecycle — log I/O errors in ephemeral
  migration, attempt older siblings on copy failure, clean up `.tmp`
  staging directories in GC, and log unexpected entries (#797)

## [3.3.0] - 2026-07-13

### Deprecated

- The old boolean `session_log` shape (`file: bool`, `transcript: bool`,
  `verbose: bool`) is deprecated. It still parses in 3.x but will be
  removed in 4.0. Migrate to the new per-sink mapping blocks. ([#744](https://github.com/phaedrus1992/llmenv/issues/744))

### Removed

- Remove dead `diff` field from `ReadOnce` config schema — the
  planned phase-2 delta mode was never implemented (#725)

### Changed

- `session_log.verbose` replaced with per-sink `level` (info/debug/trace).
  `session_log.file` and `session_log.transcript` are now mapping blocks with
  `enabled` + `level` fields. Old boolean shape still parses. ([#740](https://github.com/phaedrus1992/llmenv/issues/740))

### Fixed

- Early-exit hook-run before scope evaluation for events that
  produce no memory actions — saves ~3.5ms per PreToolUse
  dispatch on a loaded config (#702)
- Thread `--engine` flag through to adapter selection so
  hook-runs targeting non-default engines (e.g. opencode)
  actually use the correct adapter instead of always env-sniffing
  (#704)
- Fix WebSearch auto-store labelling "URL: unknown" instead of
  the actual search query — read `tool_input.query` for WebSearch
  and label as `Query:` (#707)
- Strip ICM advisory lines ("Consider saving", "No memories found.")
  from hook-run recall output — ~1KB/turn of noise in agent
  conversations (#692)
- Fix doctor false-flagging marketplaces pinned to annotated
  tags as broken — `git rev-parse <tag>` returns the tag
  object SHA, not the commit SHA; use `^{commit}` peeling for
  commit-vs-commit comparison (#695)
- Fix project-scoped tags from `.llmenv.yaml` leaking into
  host-level plugin collection, MCP server, and throttle
  resolution — introduce `non_project_tags()` to exclude
  project-scoped tags from host config generation (#696)

- Fix read-once hook using PascalCase `filePath` when Claude Code
  sends snake_case `file_path` — production read-once was a
  complete no-op against any Read call (#724)
- Move `prune_stale_sessions` from `SessionCache::load()` (runs
  on every Read) to `save()` — eliminates redundant readdir +
  stat per Read call (#726)
- Surface silent error swallowing in config load, session-log
  correlation, and setup detection — add `inspect_err`
  diagnostics before `.ok()`/`.ok()?`/`unwrap_or_default()` that
  silently discarded errors (#731, #710, #712, #713)

### Added

- Add `llmenv upgrade` subcommand for self-upgrade from
  GitHub releases (`--check`, `--track beta|release`,
  `features.upgrade.track` config option) (#686)
- Add model provider configuration
  (`capabilities.model_providers`) with schema types,
  validation, merge rules, and CrushAdapter rendering
  (#526, #527, #528)
- Add default model selection
  (`capabilities.default_models`) for role-keyed model
  resolution across providers (#530)
- Add content-based scope matching with file glob
  patterns (`scope.content`) — auto-activates tags when
  matching files exist in the working directory, without
  requiring `.llmenv.yaml` markers (#278)
- Cache hashing now supports `version: major` granularity — set
  `hashing: { normal: { version: major } }` in config.yaml to key
  cache folders on major version only (e.g. `1/` instead of `1.2/`).
  Default remains `minor` for full backward compatibility. (#651)
- opencode engine support — new `opencode` adapter with full parity
  vs the claude-code adapter: AGENTS.md, rules, skills, MCP
  (local/remote), LSP, permissions, hook bridging via a generated JS
  shim plugin, and Claude-plugin content translation (#656, #657)
- JSON Schema generation for materialized configs — adapters that
  derive `JsonSchema` on their output structs now emit a
  `{adapter}.schema.json` sidecar alongside the native config file,
  enabling IDE validation and editor autocompletion for materialized
  opencode.json files. (#660)
- Add read-once file deduplication hook — tracks files
  read via the Read tool within a session and skips
  re-reading unchanged files within a configurable TTL
  (`features.read_once`). Includes deny-mode envelope to
  block writes to never-read files (#318)
- Add slippage control bundle — effort-level injection
  and compaction-survival rules to improve agent behavior
  consistency across long sessions
  (`features.slippage`) (#317)
- Add TTL-based memory retention pruning
  (`llmenv memory prune`, `memories.retention` config with
  per-type durations, `memories.auto_prune` flag during
  materialize) (#270)
- Add post-session LLM consolidation — after SessionEnd,
  distills recent memories into permanent semantic rules
  via direct Anthropic API call, reducing context drift
  across sessions (#595)

### Fixed
- opencode adapter not activating when `OPENCODE_CONFIG_DIR` is unset
  (now falls back to checking if `opencode` is on PATH) (#657)

## [3.2.0] - 2026-07-11

### Changed

- Move WebFetch/WebSearch ICM storage and PostSession consolidation to background
  detached child processes, reducing hook latency for common events (#670)
- Cache parsed config by file mtime in hook-run to avoid redundant YAML parsing on each event (#670)

### Added

- `llmenv doctor` checks that config-dependent executables (`icm`,
  `mcp-proxy`/`uvx`, `claude`, `crush`) are available on `PATH`,
  respecting each tool's config conditions (memory entries, disabled
  engines, optional status). (#655)
- Add Discord community link to README and getting-started guide

### Fixed

- `capabilities.permissions` and `native_permissions` rules
  (top-level or bundle-contributed) whose `pattern`/`paths` have
  unbalanced parentheses — e.g. a process-substitution deny pattern like
  `bash <(curl *` — are now rejected at config-load time with a fix hint,
  instead of rendering into a `Tool(pattern)` string that Claude Code/Crush
  silently drop at settings-load time. This previously left `deny` rules
  silently non-functional with no warning from `llmenv doctor` or config
  validation. (#664)
- Validate skill-file paths with CommonMark-aware parsing (`pulldown-cmark`)
  instead of fragile heuristics. Fenced/indented code blocks and inline code
  spans containing `~/.claude` no longer falsely trigger configuration-path
  validation errors. (#659)
- Fix root-level `lsp:` and `skills:` declarations in `config.yaml` not
  being materialized into the rendered manifest. These were parsed,
  validated, and documented but silently never reached the output. (#661)
- Fix false `"marketplace.json broken"` warning from `llmenv doctor` when
  the context-mode marketplace clone is properly synced but lacks a
  standalone `marketplace.json` — the marketplace is managed internally
  and the check was a false positive
- Fix loopback address detection in the ICM MCP SSRF guard to cover the
  full `127.0.0.0/8` range, unspecified addresses (`::`, `::0`, `0.0.0.0`),
  and provide a safer fallback when `needs_proxy` cannot be determined
- Fix background PostSession consolidation child process inheriting stdin,
  which could cause hangs; add trace logging for CONFIG_CACHE poison
  detection

## [3.1.0] - 2026-07-10

### Added

- Auto-activate OS tag in scope resolution — bundles with OS-specific `when:` tags
  (e.g. `linux`, `macos`, `windows`) now activate automatically without requiring
  manual scope configuration (#638)
- Create plugin cache directory automatically on export (`CLAUDE_CODE_PLUGIN_CACHE_DIR`),
  and add `llmenv prune --plugin-cache` flag for explicit shared plugin cache cleanup (#643)

### Fixed

- Build static Linux binaries with musl (`*-linux-musl`) instead of glibc
  (`*-linux-gnu`) so the pre-built Homebrew-tap binaries work on any Linux
  distro regardless of system glibc version (#647)
- Fix typos in `llmenv prune` output text

## [3.0.0] - 2026-07-10

### Major changes since v2.4.1

This release introduces a multi-engine architecture (Crush alongside Claude
Code), a built-in persistent memory system via ICM, automatic context-mode
integration, and a new interactive setup wizard. Full granular changeset in
the rc.1 and rc.2 sections below.

- **Multi-engine support** — llmenv now drives Crush as a second agent engine
  alongside Claude Code. `export`/`hook`/`regenerate` iterate all installed
  adapters. The CrushAdapter renders hooks, MCP servers (stdio/SSE/HTTP), LSP,
  permissions, and skills against Crush's actual schema.
- **ICM Memory System** — Built-in persistent memory with session logging
  (transcript + JSONL file), CLI observability (`llmenv memory stats|list|diff|prune`),
  importance/type annotations, consolidation groundwork, and `SessionStart`/
  `SessionEnd` lifecycle hooks that actually wire memory wake-up and store.
- **Context-mode integration** — Enabling `features.context_mode` auto-wires
  the context-mode plugin: marketplace clone, MCP server, durable data dir,
  and permissions. Supersedes the removed `LLMENV_BASH_BAN`.
- **`llmenv setup` wizard** — Interactive command that scans existing tool
  configs (`~/.claude`, `~/.cursor`), prompts for preferences, and generates a
  validated `config.yaml` with starter `AGENTS.md`.
- **First-class LSP & Skills** — Declare language servers (`name`, `command`,
  `filetypes`, `init_options`, etc.) and skills directly in config or bundles,
  tag-scoped and independent of the plugin model.
- **MCP field parity** — `headers`, `disabled`, `disabled_tools`, and `timeout`
  on MCP server entries.
- **Config validation & observability** — `llmenv doctor` warns on dangling
  bundle dirs, unused marketplace entries, and orphaned `native_permissions`.
  `disabled_engines` skips rendering for named engines. Token-efficiency checks
  in `doctor`, `--compress` export flag.
- **BREAKING:** `session_log` is now a mapping (`{ file, transcript, verbose,
  path, max_content_bytes }`) instead of a path string. The old string form is
  rejected with a migration hint.
- **Removed:** `LLMENV_BASH_BAN` env var; superseded by context-mode.

### Changes since v3.0.0-rc.2

- Forward-merged from 2.4.0: per-hash `CLAUDE_CODE_TMPDIR` temp isolation and
  `CLAUDE_CODE_PLUGIN_CACHE_DIR` durable plugin cache (#630, #632)
- Forward-merged from 2.4.0: `CONTEXT_MODE_DATA_DIR` and other state-directory
  env vars now emit forward-slash paths on all platforms (#497)
- `llmenv doctor` structural validation: dangling bundle directories, unused
  marketplace entries, orphaned `native_permissions` keys (#604)
- CI: trusted publishing to crates.io via OpenID Connect

## [3.0.0-rc.2] - 2026-07-09

### Added

- `llmenv setup` interactive wizard: scans existing tool configurations
  (`~/.claude`, `~/.cursor`), prompts for GitHub repo and bundle organization,
  and generates a validated `config.yaml` with starter `AGENTS.md`. (#561, #575)
- `llmenv setup --rescan`: re-read existing tool configs and refresh the
  enumeration JSON without overwriting config.yaml, AGENTS.md, or bundle
  contents. Composes with `--no-launch` and `--path`. (#576)
- The Claude Code adapter now renders `capabilities.lsp`: entries with an
  `extension_to_language` map (new field, e.g. `{".rs": "rust"}`) render into a
  synthetic skills-directory plugin (`skills/llmenv-lsp/.claude-plugin/plugin.json`),
  which Claude Code auto-loads with no marketplace or install step — its only LSP
  surface is a plugin's `lspServers` manifest key. Entries without the map are
  skipped (with a warning) rather than rendered incorrectly, since the existing
  `filetypes` field (language ids) doesn't reliably convert to Claude's required
  extension-to-language form. (#556)
- `CrushAdapter` hardening: incompatible hook events, `mcp_tool` hooks, and
  non-skill plugin content (`agents/`, `commands/`, `hooks/`) now warn and skip
  instead of hard-erroring the entire render — one unsupported piece no longer
  blocks Crush output altogether. (#543)
- `llmenv doctor` now reports, by name, every hook event that a `PATH`-detected
  adapter can't materialize (e.g. Crush skipping a `PostToolUse` hook), and its
  token-efficiency checks now count a var as set if it's declared in
  `native.claude_code.env`, not only in the live process environment. (#543)
- Top-level `disabled_engines` config list: skip rendering for named engines
  (e.g. `claude_code`, `crush`) even when their binary is on `PATH`. An entry
  that doesn't match any registered engine prints a warning on every
  `export`/`regenerate`/`doctor` run (not just `llmenv validate`). Matching is
  case-insensitive, so `Claude_Code` or `CRUSH` disable the same engines as
  their lowercase form, and the `--engine` flag's own unknown-engine check
  now matches case-insensitively too. (#562, #564)
- Add optional `<!-- llmenv-type: episodic|semantic|procedural -->` HTML-comment marker in
  context chunks to classify stored memories by type. Types persist as ICM memory metadata and
  can be filtered in recall. Configurable default via `default_type` on memory server entries. (#267)
- Add `llmenv memory stats|list|diff|prune` CLI subcommand for ICM store observability. `stats`
  shows record counts, `list` dumps memories for the active scope, `diff` highlights changes
  since the last session snapshot. (#268)
- Add optional `<!-- llmenv-importance: low|medium|high|critical -->` marker to tag memory
  importance at write time. Configurable per-type defaults via `type_importance` map on memory
  server entries. SessionEnd writes now skip duplicate chunks when unchanged. (#269)
- Add `consolidation` config section with `enabled` and `max_rules_per_session` fields.
  Wires a diagnostic consolidation hook into the SessionEnd lifecycle; LLM integration
  deferred. (#271, #595)
- Add three structural validation checks to `llmenv doctor`: warn on dangling bundle
  directories (declared but missing on disk), unused marketplace entries (defined but
  unreferenced), and orphaned `native_permissions` keys (no matching MCP server or
  engine adapter) (#604)

### Changed

- Replace stale Claude Code env var table in `docs/env-vars.md` with a link to the
  [upstream docs](https://code.claude.com/docs/en/env-vars)

### Fixed

- Fix `export`/`regenerate` never actually materializing Crush output: the internal
  materialization step ignored which adapter was passed in and always rendered Claude
  Code's layout, so `crush.json` and `CRUSH_GLOBAL_CONFIG`/`CRUSH_GLOBAL_DATA` were never
  produced even with `crush` on `PATH`. `regenerate` also gained the same per-adapter
  `PATH`-gated loop `export` already had. (#543)
- Fix `CrushAdapter` hard-erroring the *entire* render over a single incompatible hook
  event, `mcp_tool` hook, or plugin with `agents/`/`commands/`/`hooks/` content — one
  unsupported bundle previously blocked Crush output altogether. Incompatible pieces
  are now skipped with a warning naming them; everything Crush can support still
  materializes. (#543)
- Fix `LLMENV_STATE_DIR` (and other configured tool-state relocation vars) getting
  silently overwritten with the wrong adapter's state directory once more than one
  adapter materializes in the same `export`/`regenerate` run — the durable-state
  feature is scoped to tools writing into `CLAUDE_CONFIG_DIR`, so it now only runs
  for the Claude Code adapter instead of once per adapter. (#543)
- Fix unbounded, non-timeout-bounded DNS resolution in the ICM MCP client's SSRF
  guard: `validate_url_production` resolved domain hosts via a plain blocking
  `to_socket_addrs()` call before the 2s `HOOK_TIMEOUT` was ever applied, so a slow
  or failing DNS resolver could hang `llmenv hook-run` — including the per-prompt
  `turn_start` hook — for minutes instead of seconds. Resolution is now bounded by
  the same timeout via a dedicated helper. (#547)
- Fix `CrushAdapter` exporting `CRUSH_GLOBAL_CONFIG` pointing directly at the rendered
  `crush.json` file instead of the directory containing it. Crush's own config loader
  joins `crush.json` onto `CRUSH_GLOBAL_CONFIG` itself, so the file-path value made it
  look for `crush.json/crush.json` and fail to load — `crush` couldn't start with any
  llmenv-managed config. `CRUSH_GLOBAL_CONFIG` now points at the cache directory, matching
  the original design intent. (#551)
- Fix `CrushAdapter` rendering hooks in Claude Code's nested `{matcher, hooks:
  [{type, command, tool}]}` shape instead of Crush's flat `HookConfig` (`{matcher?,
  command}`) — Crush read an empty `command` off the wrapper object and rejected the
  whole config with `hook PreToolUse[0]: command is required`, so no hook (or any
  other capability sharing the render) ever reached Crush. Also ports Claude Code's
  bundle-relative hook-script path resolution (a bare `hooks/foo.sh` in a hook
  `command` resolves against the bundle's directory) into the shared adapter helper
  so Crush benefits from it too — it previously only ran for Claude Code, leaving a
  bundle-authored relative script path broken under Crush. (#551)
- Fix `CrushAdapter` rendering MCP servers, LSP `init_options`, and permissions in
  Claude Code's shapes instead of Crush's actual schema
  (`https://charm.land/crush.json`), found by auditing the adapter against it: every
  MCP server previously failed to initialize because Crush's required `type` field
  (`stdio`/`sse`/`http`) was either missing (stdio entries) or set to the
  nonexistent value `"remote"` (remote entries) — Crush's MCP client hits an
  `unsupported mcp type` error for anything else. LSP `init_options` was written
  under Claude Code's `initializationOptions` key, so Crush's plain
  `json.Unmarshal` silently dropped it. `permissions.denied_tools`/`default_mode`
  were also dropped — Crush's `PermissionsConfig` has only `allowed_tools`; not a
  security regression (Crush already denies-by-default outside the allow-list),
  but dead output. The full rendered config (all three MCP transports, hooks, LSP,
  permissions) now validates against the real schema with zero violations. (#554)
- Fix the ICM memory backend (`session_start`/`turn_start`/`session_end`) being
  completely non-functional whenever it resolved to loopback or a private-network
  address — the documented common topology (AGENTS.md: "the resolved icm MCP
  endpoint can be a remote `icm serve`"). Four bugs stacked, each masking the next:
  the SSRF guard rejected loopback/private/ULA outright (now split into
  `SsrfPolicy::PublicOnly` vs. `AllowPrivateNetwork`, the latter used by the ICM
  client); the client never sent the `Accept` header MCP's Streamable HTTP
  transport requires (406); the client never performed the MCP `initialize`
  session handshake the transport requires (400 missing session ID); and the
  `SessionEnd` store action never sent the tool's required `topic` field. All four
  fixed together; verified end-to-end against a live ICM server. (#548)
- Fix remaining hardcoded ClaudeCodeAdapter call sites: thread the actual adapter identity through
  `build_and_materialize`, `run_export`, `run_regenerate`, `run_prune`, `run_doctor`,
  `run_throttle_inner`, and `hook_run` instead of assuming Claude Code (#544)
- Fix skill materialization rejecting a `SKILL.md` whose `description` contains a colon (e.g.
  "Triggers on: ..."); `name`/`description` values are now auto-quoted before the strict YAML
  parse so a single malformed-looking skill no longer takes down the whole adapter (#568)
- Fix bundle hook paths in generated `settings.json` referencing the source directory instead
  of the materialized cache directory. Hook paths now resolve against the cache copy via
  two-pass resolution — direct join for clean relative paths, suffix-match against the
  materialized manifest for shell-variable/absolute prefixes — with longest-suffix matching
  and path-boundary checks to prevent ambiguous matches. (#162)
- Fix memory deduplication snapshot being written before the MCP store call completed.
  A transient store failure left the snapshot ahead of reality, causing the next
  `SessionEnd` to skip the store and permanently lose the memory chunk.
- Fix unknown keys under `features:` silently degrading instead of producing a clear
  error; `Features` now rejects unknown fields at parse time. (#602)
- Fix skills with the same name from different bundles colliding in materialization
  after tag filtering; skills are now deduplicated by name, keeping the first
  occurrence. (#600)
- Fix `llmenv doctor` not verifying the context-mode marketplace clone exists when
  `features.context_mode.enabled` is true; now warns if the marketplace hasn't been
  synced yet. (#601)
- Fix example bundle hook matchers using glob patterns (`*.rs`, `*.py`, `*.ps1`)
  instead of valid tool-name regexes; corrected to `^(Edit|Write|MultiEdit)$`. (#605)
- Fix example bundle commands containing unsubstituted template placeholders and
  incorrect ICM CLI usage instead of ICM MCP calls. (#606)
- Fix example `fyi` app: race-condition in `mkdir` lock in `refresh.sh`, missing
  `TypeError` in toggle handler, missing `Origin` check on POST endpoints, and
  phantom `topFocus` in `SPEC.md`. (#607)
- Fix example plugin augmentation: pinned slop-scan wrapper and cryptic dangling
  bullet in `general.md`. (#608)

## [3.0.0-rc.1] - 2026-07-01

### Added

- `features.context_mode` built-in feature: enabling `features.context_mode.enabled`
  auto-wires the context-mode plugin (marketplace, plugin, durable
  `CONTEXT_MODE_DATA_DIR`, and MCP permission) — the token-efficiency counterpart
  to the built-in ICM memory feature. Warns when the plugin is also declared manually
  in a plugin-collection. (#490)
- ICM-transcript session logging: llmenv records scope + lifecycle (and, with
  `session_log.verbose`, prompts and tool use) into ICM's transcript store via
  the ICM MCP, discoverable by `llmenv-tag:` / `llmenv-bundle:` tokens and
  project. A local JSONL `file` sink mirrors the same stream, independent of
  ICM reachability. (#382)
- The Claude Code adapter now auto-registers `SessionStart`/`SessionEnd` hooks
  running `llmenv hook-run`, fixing a gap where the ICM memory wake-up/store
  dispatcher existed but was never wired into generated `settings.json` —
  memory wake-up/store now actually fires. Continuous per-prompt recall
  (`turn_start`) is still unwired; tracked in #499. (#382)
- Multi-engine foundation for a second agent engine (Crush): `export`, `hook`,
  and `regenerate` now iterate a registry of engine adapters, materializing each
  into its own per-engine cache subtree and skipping any whose binary isn't on
  `PATH`. Claude-only users see no behavior change. Groundwork for the Crush
  adapter (#506); no Crush support ships yet. (#502)
- Add first-class `lsp:` capability: declare language servers (`name`, `when`,
  `command`, `args`, `env`, `disabled`, `filetypes`, `root_markers`,
  `init_options`, `timeout`) at the top level or inside a bundle, tag-scoped like
  `mcp`. Engines with no LSP concept (Claude Code) silently ignore them. (#503)
- Add first-class `skills:` capability, decoupled from plugins: declare a skill
  (`name`, `path`, `when`) directly in config or a bundle, tag-scoped, validated
  with the same frontmatter and path checks as plugin-bundled skills. (#504)
- Add MCP server field parity: `headers`, `disabled`, `disabled_tools`, and
  `timeout` on MCP server entries. All optional — existing configs parse
  unchanged. (#505)
- `CrushAdapter`: Crush is now a supported engine. `export`/`hook`/`regenerate`
  render `crush.json` when `crush` is on `PATH`. What maps: permissions →
  `allowed_tools`/`denied_tools` (lossy, fail-closed — `ask` rules collapse to
  `denied_tools`, never silently allowed; Crush has no ask concept); hooks →
  `PreToolUse` only (`mcp_tool`-kind hooks and unsupported hook events hard-error
  with an actionable message); MCP servers (including `headers`, `disabled_tools`,
  `timeout`); LSP servers → `lsp.<name>`; first-class skills and plugin-projected
  skills → `options.skills_paths`. Non-skill plugin content (`agents/`, `commands/`)
  hard-errors naming the offending plugin. `native.crush` / `native_permissions.crush`
  / `native_hooks.crush` / `native_mcp.crush` merge verbatim — provider/model config
  lives here until first-class provider config ships (#508). Docs in #507. (#506)

### Changed

- **Behavior change (dual-engine export):** `export`, `hook`, and `regenerate`
  now iterate all registered engine adapters. If `crush` is on `PATH`, a new
  `crush/` cache subtree is materialized and `CRUSH_GLOBAL_CONFIG` /
  `CRUSH_GLOBAL_DATA` are exported alongside the existing Claude Code env vars.
  Claude-only users (no `crush` binary on PATH) see no change. (#502, #506)
- **BREAKING:** `session_log` is now a mapping (`{ file, transcript, verbose,
  path, max_content_bytes }`), not a path string. ICM transcript logging is on
  by default. The pre-3.0 `session_log: "<path>"` form is rejected with a
  migration hint. (#382)

### Removed

- `LLMENV_BASH_BAN` env var and its deny-rule wiring. It was broken as shipped
  (read from llmenv's process env before bundle-declared values landed) and is
  superseded by the built-in context-mode feature. (#490, removes #464)

### Fixed

- Fix marketplace and plugin-payload sync returning a broken clone with unstable cache key when
  git HEAD cannot be resolved. Now detects and errors on broken clones (after clone or pull),
  cleans up the corrupted directory, and forces a fresh clone on retry (#537)

## Version 2.x

## [2.4.1] - 2026-07-10

- CI updates to support trusted publishing to crates.io

## [2.4.0] - 2026-07-10

### Added

- Add per-hash temp directory isolation for Claude Code subprocesses: `CLAUDE_CODE_TMPDIR`,
  `TMPDIR`, `TMP`, and `TEMP` env vars now point to `<cache_dir>/<hash>/tmp/`, scoping
  temporary files to the current content hash (#630)
- Add durable plugin cache directory: `CLAUDE_CODE_PLUGIN_CACHE_DIR` now points to
  `<state_dir>/plugins/` so plugins are not re-downloaded on every scope change (#632)

### Fixed

- Fix hook context emission including `additionalContext` content in store-only events
  (SessionStart, SessionEnd), which Claude Code's hook schema rejects — store-only events now
  emit empty output instead of triggering a validation error at the end of every session (#558)
- Fix `CONTEXT_MODE_DATA_DIR` and other state-directory env vars (from
  `materialize::state::state_env_vars`) emitting platform-native path separators
  (`\` on Windows) instead of forward slashes, breaking cross-platform compatibility
  for consumers that parse paths in these env vars. Normalization consolidated into
  the existing `normalize_rel` helper. (#497)

## [2.3.0] - 2026-06-30

### Added

- Add `features.throttle`: keep an LLM backend within its rate limits by polling usage and
  inserting a capped, adaptive delay as the request budget runs low, instead of hitting a hard
  429. Tag-scoped like `features.memory`; currently supports the `umans` backend (#487)

## [2.2.1] - 2026-06-24

### Fixed

- Fix `llmenv export` aborting with "variable value contains forbidden control character" for
  `LLMENV_ICM_CONTEXT` and other legitimately multiline values; value validation now rejects only
  NUL, since every emission path single-quotes the value and newlines are inert there (#469)

## [2.2.0] - 2026-06-23

### Added

- Add built-in `token-efficiency` example bundle with env vars (`LLMENV_BASH_BAN`, `CBM_WARN_THRESHOLD`,
  `CBM_AUTOINDEX`), SessionEnd auto-handoff hook, SessionStart context-mode reminder hook,
  PostToolUse reject-scanner scaffold, and minimal `native_permissions` limiting Bash to
  state-mutation operations (git, mkdir, curl, trash). Include per-stack rule files (`bash.md`,
  `rust.md`, `typescript.md`, `skill-gates.md`) documenting the skill-gate pattern for conditional
  skill activation by language tag, prerequisite, or indexed content (#218, #219, #220, #222, #223)
- Add `--compress` flag to `llmenv export`: strips trailing whitespace and collapses excessive blank
  lines for token-efficient AGENTS.md output (#226)
- Wire `LLMENV_BASH_BAN` env var into the Claude Code adapter permission layer: when set, denies
  Bash tool invocations whose commands match any comma-separated prefix pattern before execution
  (#464)

### Fixed

- Fix `token-efficiency` example bundle declaring `BASH_BAN` instead of `LLMENV_BASH_BAN`; the
  Bash deny feature silently failed for any user of the example config (#466)
- Fix `token-efficiency` example bundle placing env vars under `features.env` instead of the
  top-level `env` key and using snake_case hook event names instead of PascalCase (e.g.
  `session_end` → `SessionEnd`); env vars were not exported and hooks never fired
- Fix `LLMENV_BASH_BAN` accepting patterns containing `)`, `(`, and newlines that produced
  malformed deny rules; invalid pattern characters are now rejected at startup (#465)
- Fix `LLMENV_BASH_BAN` treating a non-unicode env var value the same as the variable being
  unset; non-unicode values now return an error instead of silently disabling enforcement (#465)
- Fix `llmenv export --compress` not preserving the final newline, producing non-POSIX output
  (#465)


## [2.1.0] - 2026-06-23

### Added

- Add `session_log` config field: opt-in JSONL tracing of all llmenv log events to
  a file for diagnosing hooks and materialization without reading stderr (#382)
- Add SSH auth negotiation timeout (`ssh -o ConnectTimeout`) and HTTP pack-transfer
  stall detection (`http.lowSpeedTime`/`http.lowSpeedLimit`) to all git subprocesses,
  preventing indefinite hangs on slow or unresponsive servers (#453)
- Add annotated `examples/my-llmenv/` reference config: a fully commented example
  covering `config.yaml`, five bundles, hooks, skills, rules, and scripts
- Detect volta, fnm, Linux pnpm (`~/.local/share/pnpm/`), and macOS pnpm
  (`~/Library/pnpm/`) install paths when seeding `installMethod` in Claude Code
  settings; previously these were classified as `native`

### Fixed

- Fix `GIT_SSH_COMMAND` being overwritten by llmenv's SSH timeout injection;
  user SSH identity files, `ProxyJump`, and other existing SSH customizations
  are now preserved
- Fix `seed_install_method` overwriting a user-customized `installMethod` value
  in Claude Code `settings.json`; the field is now only written when absent
- Fix `seed_install_method` silently swallowing I/O errors (e.g. permission
  denied) when reading `settings.json`; non-NotFound errors now propagate
- Fix long interactive session pause when GitHub remote is unreachable: all git
  subprocesses now apply a TCP connection timeout (`GIT_CONNECT_TIMEOUT` — 10 s
  for background fetch/pull, 30 s for explicit plugin clone/fetch)
- Fix malformed `marketplace.json` entries (missing or invalid `source` field)
  being silently dropped; these now emit a `warn` log with the entry details (#361)

### Security

- Reject NUL, newline, and carriage-return characters in env var values at config
  load time; these were previously accepted silently and could interfere with shell
  export (#356)
- Reject `file://` transport in external plugin source URLs; only `https://` and
  SSH remotes are permitted (#360)
- Remove `StrictHostKeyChecking=accept-new` from llmenv's SSH options for git
  operations; this option weakened host-key verification (MITM/DNS-hijacking
  exposure) and was unrelated to the timeout feature it was grouped with


## [2.0.5] - 2026-06-18

### Added

- Fold six `*-ls` listing commands into `status` subcommands: `status bundles`,
  `status tags`, `status scopes`, `status mcps`, `status marketplaces`,
  `status plugins`. The top-level `*-ls` forms are retained as hidden deprecated
  shims and will be removed in 2.1.
- Add `context --bundle <name>` to narrow the context view to a single bundle,
  showing its env vars, hooks, MCPs, plugins, and skills
- Add `context --why` to show activation tracing — which scope triggered each
  active tag and which tags fired each bundle
- Add `export --explain` to annotate each exported variable with its source
  (adapter or llmenv introspection)
- Add `sync --dry-run` to preview pending config changes without committing
- Add `check-stale --auto-fix` to automatically re-materialize config on drift
  rather than only printing a warning
- Add `validate` command to check config for structural issues (duplicate bundle
  names, bundles with no activation tags)
- Add `edit [bundle-name]` command to open `config.yaml` or a named bundle file
  directly in `$EDITOR`
- Add `completions <shell>` command to generate shell completion scripts for
  bash, zsh, and fish
- Document `regenerate`, `login`, `config-context`, and `config-guard` commands
  in `commands.md`; add `regenerate` and `login` to the `getting-started.md`
  quick-reference table
- Expand `doctor` entry in `commands.md` to list the token-efficiency settings
  it checks

### Fixed

- Fix `edit` command allowing paths outside the config root via `..` traversal;
  the target path is now canonicalized and validated before opening
- Fix `edit` command ignoring arguments in `$EDITOR` (e.g. `code --wait`);
  the editor value is now split on whitespace before invoking
- Fix `validate` not checking `enable_bundles` references in project-scoped
  config; unknown bundle names now report an error regardless of scope type
- Fix `plugin-sync` silently succeeding when a configured plugin is absent from
  the marketplace manifest after sync; it now prints a user-visible error and
  exits non-zero
- Fix `status` listing commands and `doctor --all` incorrectly classifying MCPs,
  bundles, and plugins as orphaned when their `when:` tags are emitted only by
  project scopes; the emitted-tag set now includes project-scope active tags

## [2.0.4] - 2026-06-16

### Added

- Provide prebuilt `linux/aarch64` (ARM64) release binaries

### Fixed

- Fix `hookEventName` being emitted at the top level of hook JSON instead of
  inside `hookSpecificOutput`; it is now nested per the Claude Code hook schema,
  so hooks that read the event name from context find it in the right place (#419)
- Fix `llmenv plugin-sync` silently dropping all externally-sourced plugins
  (e.g. `slack`, `superpowers`) whose `marketplace.json` entry uses the
  `{"source": "git", "url": "..."}` object form; only bare-string sources were
  parsed, so every object-form entry was lost. Malformed object-form entries now
  emit a warning, and the related messages correctly direct users to
  `llmenv plugin-sync` instead of `llmenv sync`
- Fix hooks crashing with a broken-pipe error when the agent truncates their
  stdout early; hooks are fail-soft and now exit 0 on `SIGPIPE` (#422)
- Fix bundle and tag memory recall errors being silently discarded; all MCP
  action failures (recall, tag recall, bundle recall, store) now emit a
  `tracing::warn!` with structured context so misconfigured or unreachable
  recall is diagnosable without source-level debugging (#421)

## [2.0.3] - 2026-06-15

### Fixed

- Fix `SessionStart` (and other hook) output missing the required `hookEventName` field,
  causing Claude Code to reject hook JSON with "hookSpecificOutput is missing required
  field 'hookEventName'" on startup

## [2.0.2] - 2026-06-14

### Fixed

- Fix `cargo release --workspace` not bumping sub-crates: add explicit
  `shared-version = true` to each sub-crate `release.toml` so cargo-release
  treats them as part of the workspace version group
- Fix CI publish step silently timing out when sub-crate versions don't match
  the release tag: add upfront version validation that fails fast with a clear
  error message
- Fix `pre-release-hook = []` panic in cargo-release 1.1.2: remove empty hook
  arrays from sub-crate configs and update workspace hook to use `${WORKSPACE_ROOT}`
  so it resolves correctly from any sub-crate working directory

## [2.0.1] - 2026-06-14

### Fixed

- Fix multi-crate crates.io publishing: enable sub-crates (`llmenv-util`, `llmenv-paths`,
  `llmenv-git`, `llmenv-config`) for publishing with required metadata, bump all to 2.0.0
  to match root, and publish in dependency order with crates.io indexing polls in CI

## [2.0.0] - 2026-06-14

### Added

- Add token-efficiency checks to `llmenv doctor`: warns when
  `CLAUDE_AUTOCOMPACT_PCT_OVERRIDE`, `BASH_MAX_OUTPUT_LENGTH`, `MAX_MCP_OUTPUT_TOKENS`,
  or `ENABLE_PROMPT_CACHING_1H` are not set (or misconfigured); informs when
  `CLAUDE_CODE_SUBAGENT_MODEL` is unset; warns when no `context-mode` MCP server is
  configured
- Add `config::template::generate_template()` function; `llmenv init` now derives the
  config template from a single source rather than a hardcoded string, making it easier
  to keep the template in sync as the schema evolves
- Add `llmenv config-context` subcommand, auto-registered as a `SessionStart` hook
  by the Claude Code adapter; emits source config file and bundles directory paths
  as `hookSpecificOutput.additionalContext` so the agent always knows where to edit
  llmenv config rather than touching managed cache files
- Add `llmenv config-guard` subcommand, auto-registered as a `PreToolUse` hook
  (matcher: `Write`, `Edit`, `MultiEdit`) by the Claude Code adapter; warns when the
  agent writes to a path inside the managed cache directory and redirects to the
  source config; always exits 0 (fail-soft, never blocks the write)
- Add stable authentication cache: `oauthAccount` credentials are now stored in
  `state/auth/<uuid>.json` outside the content-hashed config dir and automatically
  re-injected on every new materialization; Claude Code no longer requires
  re-authentication after a version bump, project switch, or directory change (#172)
- Add `llmenv login [--global]` subcommand: captures credentials via `claude auth login`,
  saves them to the stable auth cache, and optionally persists them globally (#172)
- Add `init.seeded_settings` to `config.yaml`: user-selected keys from
  `~/.claude/settings.json` are seeded into `settings.json` on first materialization of
  a new config folder, carrying over preferences without overwriting managed settings;
  `llmenv init` now prompts to log in, import from `~/.claude`, or skip (#172)
- Add per-bundle `features.memory` overrides: bundles can declare a `features:` block in
  `bundle.yaml` to use a different memory daemon `server_host` per scope, enabling
  different daemons on different machines or networks without a global config change (#335)

### Changed

- Replace ASCII pipeline and precedence diagrams in the concepts and philosophy
  documentation pages with Mermaid flowcharts; the diagrams now render as proper
  graphs on the Docusaurus docs site

### Removed

- **Breaking:** Remove `env` (and its deprecated alias `vars`) from the top-level
  `bundle:` config field. Bundle-level environment variables must now be declared
  in `bundle.yaml` under `capabilities.env`. (#352)

### Fixed

- Fix `config-guard` path-prefix check accepting `..`-based traversal paths (e.g.
  `~/.cache/llmenv/../../../etc/shadow` matched as inside the cache); paths are now
  normalized lexically before the prefix check
- Fix `config-guard` silently swallowing JSON parse failures when the hook payload was
  malformed; non-empty non-JSON stdin now logs a warning to stderr
- Fix `config-guard` not logging when `CLAUDE_CONFIG_DIR` is set but has no recognizable
  `claude-code` ancestor directory; the fallback is now visible to operators
- Fix `config-context` silently substituting a wrong default path when config path
  resolution fails; it now emits a warning to stderr and returns a degraded-state context
  message rather than feeding the agent incorrect file paths
- Fix missing bundle directories being silently ignored; `llmenv` now logs a warning
  when a configured bundle name has no corresponding directory, making typos and
  deleted directories detectable
- Fix `mcp[].env` keys not being validated for the `LLMENV_` prefix or reserved state
  vars (`CLAUDE_CONFIG_DIR`, `LLMENV_STATE_DIR`); these were accepted silently where
  `capabilities.env` already rejected them, creating an inconsistent validation gap
- Fix git fetch spawn errors logged at `debug` level in the background sync path;
  a spawn error (git binary missing or misconfigured) is unexpected and is now logged
  at `warn` so operators can see it
- Fix git reset errors during explicit plugin sync silently logged at `debug` level;
  errors are now logged at `warn` so sync failures surface in production logs (#376)
- Fix clock skew silently bypassing the pull throttle check; when the stored sync
  timestamp is in the future, `llmenv` now logs a `warn` with the skew magnitude
  (`skew_secs`) and proceeds with the pull rather than silently skipping it (#377)
- Fix missing `plugin.json` after a plugin sync being silently ignored; `llmenv` now
  logs a `warn` when the plugin manifest is absent after materializing the plugin,
  making broken plugin installs diagnosable (#379)

## Version 1.x

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

- Add `effort_level` and `advisor_size` as first-class capability fields; rendered
  into `settings.json` as `effortLevel` and `advisorSize` for engine adapters to
  consume (`advisor_size` uses generic sizes `"small"`, `"medium"`, `"large"` so
  adapters map to engine-specific models via `native` overrides)
- Add `env` field to `NetworkScope`, `HostScope`, and `UserScope`; environment
  variables declared on a scope are injected when that scope matches, extending
  the existing bundle-level env-var pattern to all scope types
- Add GitHub Actions workflow to auto-close issues when PRs merge to `release/*`
  branches; GitHub's native auto-close only works on the default branch, so this
  workflow parses merged PR bodies for closing keywords and closes referenced
  issues via the API
- Add GitHub Actions workflow to forward-merge `release/*` branches through the
  release chain into `main`; a fix pushed to an older release line cascades
  forward through newer lines automatically, opening a labeled PR (and halting)
  on the first conflict or protected branch instead of being dropped

### Changed

- Rename `bundle.vars` to `bundle.env`; the old key `vars` is still accepted as
  a backward-compatible alias so existing configs continue to work

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
