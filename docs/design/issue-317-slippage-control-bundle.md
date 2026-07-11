# Issue #317 — context & behavioral slippage management bundle

- **Issue:** https://github.com/phaedrus1992/llmenv/issues/317
- **Milestone:** Large Projects
- **Type:** Feature (large meta-feature, multi-layer)
- **Difficulty:** Hard. Many small mechanisms; ship in phases, each layer
  independently useful and toggle-able.

## Problem

Long sessions degrade: context rot, effort decay, rule forgetting after
compaction, anchor bias, act-before-answer slippage. Prompt-only fixes are
unreliable; mechanical enforcement (hooks that inject, block, and track)
is durable. Implement the domain-agnostic subset of Claude-Control's
layers as an llmenv built-in feature. Sibling of #318 (read-once), which
shares the same hook plumbing — **read
`docs/design/issue-318-read-once-hook.md` first**; this doc reuses its
infrastructure findings rather than restating them.

## Architecture (one decision up front)

Ship as a **feature, not a literal bundle**: `features.slippage` on the
`Features` struct (`crates/llmenv-config/src/schema.rs`), following the
`context_mode` sibling pattern — because most layers need llmenv-core hook
handlers (`llmenv hook-run` dispatch in `src/hook_run/`), not just config
fragments, and features are how core-backed behavior is toggled. The
issue's "built-in bundle" phrasing describes the UX (one switch, layer
toggles), which this shape satisfies:

```yaml
features:
  slippage:
    enabled: true            # master switch, default false (opt-in)
    effort_level: xhigh      # or absent = don't touch settings
    rule_reinjection: true   # SessionStart + UserPromptSubmit
    read_before_edit: true   # PreToolUse
    self_critique: true      # Stop hook
    metrics: true            # PostToolUse read:edit ratio → ICM
    compact_survival: true   # CLAUDE.md fragment
    diagnose_command: true   # /diagnose skill
    explain_before_act: false  # phase 2, default off (transcript-scan)
    answer_before_act: false   # phase 2, default off (transcript-scan)
```

Every layer is individually toggle-able (issue AC). Defaults above.

## Phases (each lands green and shippable)

### Phase 1 — config-fragment layers (no new hook logic)

1. **`effort_level`:** rendered into generated claude-code settings by the
   adapter (find where `settings.json` keys are emitted in
   `src/adapter/claude_code.rs`; `native:` passthrough handling shows the
   merge point). Validate the enum against Claude Code's accepted values —
   check current docs, don't assume.
2. **`compact_survival`:** a short static rules fragment merged into the
   generated CLAUDE.md the same way existing metadata fragments are (find
   the CLAUDE.md assembly in the materialize path). Content: ~10 lines of
   "re-read your rules after compaction" survival rules — port the spirit
   of upstream Claude-Control's fragment, don't copy text verbatim
   (license/attribution: if any text is copied, `AGENTS.md`'s attribution
   rule applies — prefer original wording).
3. **`diagnose_command`:** materialize a `/diagnose` skill (structured
   evidence-first checklist: symptoms → evidence gathered → hypotheses →
   test per hypothesis → only then act). llmenv already materializes
   skills (`src/adapter/skills.rs`) — add a built-in skill source the
   feature injects.

### Phase 2 — stateless hook layers

4. **`rule_reinjection`:** SessionStart + UserPromptSubmit handlers in
   `src/hook_run/` emitting a compact (≤300 token) rules digest as
   additional context. Source of digest: a fixed template + the active
   tags/bundles (the ICM context chunk generator in `src/icm.rs` is the
   pattern for building injected context).
5. **`read_before_edit`:** PreToolUse handler: track Read paths per
   session (share the session cache design from #318 — same state-dir
   layout, same session-id parsing); on Edit/Write of a path never read
   this session → deny with "read it first" (deny envelope per the
   existing write-guard hook, `src/adapter/claude_code.rs:29`). Fail-soft
   on any cache error. Note: Claude Code itself enforces read-before-edit
   for Edit; this layer's value is Write and post-compaction sessions —
   keep it cheap.
6. **`metrics`:** PostToolUse handler counting tool invocations by name in
   the session state file; on SessionEnd, store a one-line summary
   (read:edit ratio, total calls) to ICM via `icm_memory_store` (MCP
   only — `AGENTS.md`) under the active tag topics, `type: episodic`.
7. **`self_critique`:** Stop-hook handler injecting a short checklist
   ("tests run? anomalies explained? scope finished?") as context —
   advisory, never blocking (a blocking Stop hook can trap the agent in a
   loop; upstream's block mode is deliberately not ported).

### Phase 3 — transcript-scan layers (default off, highest risk)

8. **`explain_before_act` / `answer_before_act`:** PreToolUse handlers
   that scan the transcript tail (hook stdin payload includes a transcript
   path — verify against current Claude Code hook docs) for a user
   question or a modifying command without preceding explanation, and
   deny-with-reason. Heuristics will misfire; that's why they're off by
   default. If, during implementation, the transcript format proves
   unstable, split phase 3 into a follow-up issue rather than blocking
   phases 1–2.

## Tests

- Schema round-trip + defaults + master-switch gating (mirrors
  `ContextMode` tests).
- Per-layer handler unit tests with synthetic hook payloads: injection
  content, deny envelopes, fail-soft on corrupt state (extend the
  `tests/hook_run_failsoft.rs` pattern).
- Adapter tests: `effort_level` appears in generated settings only when
  enabled; CLAUDE.md fragment present/absent; `/diagnose` skill
  materialized only when enabled.
- Metrics: two synthetic PostToolUse events → correct ratio in the ICM
  store call (mock the MCP boundary).

## Acceptance criteria

The issue's checklist, mapped: phases 1–2 cover every AC except the two
transcript-scan hooks (phase 3 or explicit follow-up issue). Plus:

- [ ] Every layer toggles independently; master `enabled: false` is a
      complete no-op (byte-identical materialized output).
- [ ] User-facing docs for the feature and each layer.
- [ ] CHANGELOG `[Unreleased]` entry (keepachangelog skill).
- [ ] Clippy/fmt clean; full suite green after each phase.

## Out of scope

- Homelab/Proxmox/OS-destructive layers (per issue).
- Porting Claude-Control's remaining layers (13 upstream; only the 9
  domain-agnostic ones listed are in play).
- read-once itself (#318 — separate issue, shared plumbing).
