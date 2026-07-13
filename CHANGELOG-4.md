# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/),
and this project adheres to [Semantic Versioning](https://semver.org/).

<!-- 4.0 next-header -->

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

<!-- next-url -->
[Unreleased]: https://github.com/phaedrus1992/llmenv/compare/v3.2.0...HEAD
