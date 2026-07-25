<!-- markdownlint-disable MD013 -- entries are one dense bullet per change, not wrapped prose -->

# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/),
and this project adheres to [Semantic Versioning](https://semver.org/).

<!-- 4.0 next-header -->

## [Unreleased] - ReleaseDate

## [3.6.1] - 2026-07-24

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
- `llmenv task ls` human output now groups tasks by session (current-project sessions first), indents subtasks under their parent, prefixes each row with a state glyph + label, and annotates blocked tasks with their `blocked_on` refs; new `--state <open|wip|waiting|done>` (repeatable) and `--hide-done`/`--active` filters compose with `--session` and apply to `--format json` too. See [`task`](https://phaedrus1992.github.io/llmenv/docs/commands) (#926)
- Feature-enabled MCPs (`features.context_mode`, `features.memory`) now take a `mcp_permissions` override to customize the read-only/mutation/destructive tier→action policy per feature. See [`mcp_permissions`](https://phaedrus1992.github.io/llmenv/docs/configuration#featuresmcp_permissions) (#946)

### Changed

- **Breaking:** Remove the deprecated boolean `session_log` shape
  (`file: bool`, `transcript: bool`, `verbose: bool`). Configs
  using the old format must migrate to the per-sink mapping blocks
  introduced in 3.3.0 ([#744](https://github.com/phaedrus1992/llmenv/issues/744))
- The bundled `llmenv` skill's task rules now guide agents to link tasks liberally with `--parent` (ordered decomposition) and `block --on` (real dependencies) and to record milestones, design rationale, and failures with `task note`. See [`task`](https://phaedrus1992.github.io/llmenv/docs/commands) (#932)

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
- The task-tracker Stop hook no longer re-injects the `waiting`-task FYI every turn; `waiting` tasks are now silent on Stop and surface only in the SessionStart reminder. See [`task`](https://phaedrus1992.github.io/llmenv/docs/commands) (#933)
- `llmenv task add` no longer warns "you have N task(s) already in progress" for `waiting` tasks — only genuinely `wip` tasks count, since starting new work alongside a task paused on external input is legitimate (#933)
- The statusline `{pr}` widget no longer renders empty under engines (like Claude Code) that don't send a `pr` field — it now self-resolves via `gh pr view` for the current branch, cached briefly so it doesn't shell out on every render. See [`statusline:`](https://phaedrus1992.github.io/llmenv/docs/configuration) (#950)
- The task-tracker Stop hook's `wip` reminder and SessionStart's `waiting` reminder no longer leak across projects sharing the same task store — a `wip`/`waiting` task from one project no longer nags a hook running in another. See [`task`](https://phaedrus1992.github.io/llmenv/docs/commands) (#949)
- Feature-enabled MCP permissions (context-mode, ICM) no longer conflict between a wildcard allow and per-tool tier rules; Claude Code's `deny > ask > allow` precedence was silently shadowing the wildcard, so mutation tools prompted on every call and destructive tools were blocked outright even with the feature enabled. Default policy now allows read-only and mutation tools without prompting, and asks before destructive ones. An explicit `native_permissions` rule on a built-in MCP tool now takes precedence over the tier default for that tool (`deny > ask > allow`), rather than emitting a competing entry. See [`mcp_permissions`](https://phaedrus1992.github.io/llmenv/docs/configuration#featuresmcp_permissions) (#946, #972)
- The statusline `branch` widget's PR hyperlink no longer stays inert under engines (like Claude Code) that don't send a `pr` field — the branch text now links to the current branch's PR via the same self-resolving `gh pr view` lookup the `{pr}` widget uses (#950), sharing its short-lived cache. See [`statusline:`](https://phaedrus1992.github.io/llmenv/docs/configuration) (#973)
- Rendered `settings.json` no longer lists each hook twice for the same event on a first or strict render — the freshly generated hooks doc is now deduped at generation time (the same strip-nulls-then-dedup pass `reconcile` already applied when a prior file existed), so each guard fires once per event instead of launching two (or, for dual-interpreter guards, four) processes per tool call (#977)
- The ICM memory injection no longer adds a `No memories found` block or a "consider saving" nag to the context on every prompt when the store is empty — advisory-line stripping is now case-insensitive to server wording, and a recall left with only advisory/blank lines injects nothing (#978)
- With the task tracker enabled, Claude Code's built-in `TaskCreate`/`TaskList`/`TaskUpdate` tools are now redirected into the `llmenv task` tracker instead of Claude's ephemeral task state — `TaskCreate` records a real task (auto-starting a session when none is open), `TaskList` returns the tracker's view, and `TaskUpdate` maps status to start/done/delete. Previously the agent's built-in task tools bypassed the tracker, so it sat mostly unused. See [`task`](https://phaedrus1992.github.io/llmenv/docs/commands) (#985)
- Features set at the root of `config.yaml` (`features:`) are no longer silently dropped from the generated engine config. `build_manifest` only fed `merge()` the `capabilities:` block, so a root-level `task_tracker`, `slippage`, or `context_mode` (incl. its `mcp_permissions` override) never reached the manifest that renderers gate on — the task-tracker hooks, slippage guardrails, built-in skill reference docs, and MCP-permission overrides could all silently go missing. Root `features:` now folds into the merged manifest (root wins over bundle-contributed values) (#987)
- A hook removed from your config no longer lingers in the generated `settings.json`. `reconcile` unions rendered hooks with what's already on disk (to preserve hooks a plugin self-registers at runtime), which meant a hook llmenv *used to* render but no longer does was kept forever. llmenv now records the hooks it renders and, on the next render, purges its own dropped hooks while still preserving genuinely-foreign ones (#991)

<!-- next-url -->
[Unreleased]: https://github.com/phaedrus1992/llmenv/compare/v3.6.1...HEAD
[3.6.1]: https://github.com/phaedrus1992/llmenv/compare/v3.2.0...v3.6.1
