# Issue #233 — Codex adapter: feature parity with Claude Code

- **Issue:** https://github.com/phaedrus1992/llmenv/issues/233
- **Milestone:** Large Projects
- **Type:** Feature (largest open item — a whole new engine adapter)
- **Difficulty:** Hard. Not because any one piece is novel — three
  adapters already exist to copy from — but because parity is wide and
  Codex's hook surface is the big unknown.

## Goal

`llmenv export` for Codex gives the same experience as for Claude Code:
scopes, bundles, MCP, permissions, rules/AGENTS.md, ICM recall, doctor —
against Codex's config format and lifecycle.

## Ground rules

- **Template:** `src/adapter/opencode.rs` and `src/adapter/crush.rs` are
  the newest full adapters — copy their structure (trait impl shape,
  capability probes, rendering helpers, test layout). The `AgentAdapter`
  trait lives in `src/adapter/mod.rs`; every `supports_*()` probe must get
  an explicit Codex answer.
- **Parity means honest probes, not forced features.** Where Codex has no
  equivalent surface, the probe returns `false` and llmenv silently skips
  that capability (the established pattern — see how `supports_lsp()`
  engines ignore `lsp:` entries). Do not emulate missing features.
- **Coverage enforcement:** `docs/engine-capabilities.md` +
  `tests/engine_capabilities_coverage.rs` track per-engine capability
  claims — the new engine must be added there, and that test will tell
  you what's unaccounted for. Run it early and often.
- Dependency notes: #163 (bash-pattern research) informs permission
  translation — if it hasn't landed, do its Codex column as part of Phase
  0 here and feed it back into that doc.

## Phase 0 — capability research (gates everything; write it down)

Produce a mapping table (commit it as a comment block in
`src/adapter/codex.rs` or an appendix in `docs/engine-capabilities.md`)
from current, cited Codex docs/source — **not from memory**; Codex's
config surface changes fast. For each row of the issue's parity table,
record: Codex config file + format (TOML?), key name, semantics, and
"none" where absent. Specifically pin down:

1. Config file location/format and how per-project vs global config merge.
2. MCP server declaration shape.
3. Instructions/rules file (AGENTS.md handling) and merge order.
4. Permissions/approval model — and how (or whether) Claude-style
   `Bash(cmd:*)` allow rules translate. This is where #163's findings
   plug in; divergences surface as doctor warnings, not silent drops.
5. **Hooks/lifecycle:** does Codex expose anything equivalent to
   SessionStart / UserPromptSubmit / PreToolUse? This decides whether ICM
   recall (issue AC) is implementable. If Codex has no hook surface, ICM
   recall parity is **not achievable** — file the gap explicitly (issue
   comment + follow-up issue), set the probes false, and descope those
   ACs with the finding as evidence. Don't fake it via wrappers.
6. Env-var injection and state-dir override (for `StateConfig`, #175).

## Phase 1 — adapter skeleton + registry

`src/adapter/codex.rs`: `CodexAdapter` implementing `AgentAdapter` with
all probes answered per Phase 0; register it wherever the existing three
adapters are enumerated (registry + CLI engine selection + the
`registry_adapter_trait_probes` test). `materialize()` initially renders
only the config skeleton. Add the engine to
`engine_capabilities_coverage` and the docs table.

## Phase 2 — core rendering

In parity-table order, smallest first, one PR-sized commit each:

1. **Rules/instructions merge** → Codex's AGENTS.md-equivalent (llmenv
   already assembles merged rules for claude-code — reuse the merged
   product, only the write-out differs).
2. **MCP injection** from `capabilities.mcp` + `features.memory` (ICM
   server entry — the ICM MCP *server* wiring is independent of hooks and
   should work even if Phase 0 kills hook parity).
3. **Permissions** with the #163 translation rules; untranslatable
   patterns → materialize-time warning listing the dropped/adjusted rules.
4. **Env/state** (`StateConfig` relocation, env var injection).
5. **Native passthrough**: `native.codex` keys merged verbatim (the
   `Config.native` escape hatch — mirror `tests/native_passthrough.rs`).

Each step: snapshot-style adapter tests mirroring the corresponding
`tests/claude_code_adapter.rs` cases (that file is the parity checklist —
walk it test by test and port or explicitly skip each with a reason).

## Phase 3 — hooks + ICM recall (only if Phase 0 found a surface)

Wire `llmenv hook-run --engine codex <event>` dispatch: add the engine's
event-name mapping alongside the claude_code table
(`src/adapter/claude_code.rs:45–90` pattern), map Codex's equivalents of
session-start/prompt-submit to the existing `HookEvent`s, and register
the hooks in Codex's config. The `hook_run` handlers themselves are
engine-agnostic already — this phase is mapping + registration only.

## Phase 4 — doctor + export + docs

- `llmenv doctor`: Codex checks mirroring the claude-code set
  (`src/cli/doctor.rs`): binary present, config parse, version skew,
  stale materialization (see `tests/doctor_version_skew.rs`,
  `tests/check_stale.rs` for what's enforced).
- `llmenv export`/`setup` recognize Codex as a target engine end-to-end.
- Docs: engine page alongside the existing engines' docs;
  `docs/engine-capabilities.md` rows finalized; CHANGELOG `[Unreleased]`
  entry (keepachangelog skill).

## Acceptance criteria

Issue's four ACs, amended by Phase 0 reality:

- [ ] `llmenv export` produces valid Codex config activating the active
      scope's bundles, MCPs, permissions, and rules.
- [ ] ICM recall fires on Codex's lifecycle equivalents **or** a
      documented gap issue exists with the probe set false.
- [ ] Doctor surfaces Codex-specific issues (missing binary, invalid
      config, stale materialization).
- [ ] Every `tests/claude_code_adapter.rs` case has a Codex port or an
      explicit skip-with-reason; `engine_capabilities_coverage` green.
- [ ] Phase 0 table committed; no Codex behavior claimed without a cited
      source.
- [ ] Clippy/fmt clean; full suite green per phase.

## Out of scope (per issue)

- Codex-only features with no Claude Code equivalent.
- Other engines (Gemini CLI etc.).
