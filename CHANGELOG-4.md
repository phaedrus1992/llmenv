<!-- markdownlint-disable MD013 -- entries are one dense bullet per change, not wrapped prose -->

# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/),
and this project adheres to [Semantic Versioning](https://semver.org/).

<!-- 4.0 next-header -->

## [Unreleased] - ReleaseDate

4.0 is a small major so far. There is one breaking change — `session_log`'s boolean shape (`file: true`, `verbose: true`) is no longer translated to the per-sink mapping behind the scenes, and is now a parse error that names its replacement (#744). Alongside it, one restriction is loosened: `native.claude_code.permissions` is accepted rather than rejected, layering additively so the catch-all can carry Claude-Code-only permission keys llmenv doesn't model — without giving a `native:` block a way to weaken what the renderer produced (#750).

Everything shipping on the 3.x line is inherited; those entries live in `CHANGELOG-3.md` (Version 3.x on the docs site).

### Removed

- **Breaking:** the boolean `session_log` shape is gone. `session_log: { file: true, transcript: false, verbose: true }` parsed until now by being translated to the per-sink form behind the scenes; it is rejected outright in 4.0. Each sink is a mapping — `file: { enabled, level }`, `transcript: { enabled, level }` — and `verbose: true` becomes `level: debug` on whichever sink should capture prompts and tool calls, which means the two sinks can now differ. The parse error names the replacement; it does not rewrite your config. See [Configuration](https://phaedrus1992.github.io/llmenv/docs/configuration#session_log) (#744)

### Added

- `native.claude_code.permissions` is accepted instead of rejected, making the catch-all the escape hatch for Claude-Code-only permission keys llmenv doesn't model (`additionalDirectories`, `disableBypassPermissionsMode`, and whatever ships next) without waiting on a neutral-schema field. It's safe to accept because the merge is additive: `allow`/`ask`/`deny` append to what was rendered rather than replacing it, `deny > ask > allow` authority is re-applied afterwards, rule strings get the same `Write` → `Edit` normalization as `native_permissions`, and every other key overwrites. `defaultMode` is rejected here — it's modeled as `capabilities.permissions.default_mode`, and allowing it would let anything that can author a `native:` block set `bypassPermissions` and switch the permission system off. A fragment can tighten permissions or add unmodeled keys; it cannot loosen what the renderer produced — an omitted or `null` `deny` leaves the rendered one intact. `native.claude_code.hooks` still hard-errors, since an array of matcher groups has no unambiguous additive merge. See [Engines](https://phaedrus1992.github.io/llmenv/docs/engines#nativeclaude_codepermissions) (#750)

<!-- next-url -->
[Unreleased]: https://github.com/phaedrus1992/llmenv/compare/v3.11.0...HEAD
