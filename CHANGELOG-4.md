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

### Changed
- **Breaking:** Remove the deprecated boolean `session_log` shape
  (`file: bool`, `transcript: bool`, `verbose: bool`). Configs
  using the old format must migrate to the per-sink mapping blocks
  introduced in 3.3.0 ([#744](https://github.com/phaedrus1992/llmenv/issues/744))

<!-- next-url -->
[Unreleased]: https://github.com/phaedrus1992/llmenv/compare/v3.2.0...HEAD
