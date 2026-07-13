# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/),
and this project adheres to [Semantic Versioning](https://semver.org/).

<!-- 3.0 next-header -->

## [Unreleased] - ReleaseDate

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

## [3.2.0] - 2026-07-11

### Changed
- Move WebFetch/WebSearch ICM storage and PostSession consolidation to background detached child processes, reducing hook latency for common events (#670)
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
  (https://charm.land/crush.json), found by auditing the adapter against it: every
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

<!-- next-url -->
[Unreleased]: https://github.com/phaedrus1992/llmenv/compare/v3.2.0...HEAD
[3.2.0]: https://github.com/phaedrus1992/llmenv/compare/v3.1.0...v3.2.0
[3.1.0]: https://github.com/phaedrus1992/llmenv/compare/v3.0.0...v3.1.0
[3.0.0]: https://github.com/phaedrus1992/llmenv/compare/v3.0.0-rc.2...v3.0.0
[3.0.0-rc.2]: https://github.com/phaedrus1992/llmenv/compare/v3.0.0-rc.1...v3.0.0-rc.2
[3.0.0-rc.1]: https://github.com/phaedrus1992/llmenv/compare/v2.3.0...v3.0.0-rc.1
