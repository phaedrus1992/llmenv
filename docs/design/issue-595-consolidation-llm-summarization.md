# Issue #595 — Post-session consolidation: wire up LLM summarization

- **Issue:** https://github.com/phaedrus1992/llmenv/issues/595
- **Milestone:** Large Projects
- **Type:** Feature (fills an existing stub)
- **Difficulty:** Hard-ish — one genuine design decision (how to reach an
  LLM), plus error/timeout handling. Infrastructure is already done.

## Current state (verified)

PR #594 shipped everything except the LLM call itself:

- `src/consolidation/mod.rs` — `run(config, client)` finds the active
  `features.memory[*].consolidation` config (`enabled`,
  `max_rules_per_session`), logs intent, and returns a diagnostic string
  with an explicit `ponytail: LLM integration deferred to follow-up`
  marker. **This issue replaces that stub body**; the enable-gating and
  signature stay.
- Hook wiring, `ConsolidationConfig`, and the post-session dispatch exist.
- Spec: `docs/superpowers/specs/2026-06-02-memory-system-improvements.md`
  §R5 (lines ~249–281) — the prompt pattern, storage contract, and
  conservatism rules are decided there. Read it in full.

## Decision gate: how to call the LLM (resolve first)

**Option A (preferred — investigate first): delegate to ICM.** ICM's MCP
surface includes an `icm_memory_consolidate` tool. If ICM's consolidate
does the episodic→semantic distillation server-side (it owns the store and
an LLM backend), llmenv's job collapses to: recall-check preconditions →
call `icm_memory_consolidate` with tag/bundle scoping and
`max_rules_per_session` → report the result. No LLM SDK dependency in
llmenv, no API key handling, and it honors the `AGENTS.md` MCP-only rule
by construction.

*Verify by inspecting the resolved ICM server's tool list/schema
(`tools/list` via the MCP client — see how `src/hook_run/mcp_client.rs`
issues calls) and ICM's docs for that tool's parameters and behavior.*

**Option B (fallback): direct LLM call from llmenv.** Only if
`icm_memory_consolidate` doesn't exist on the deployed ICM or can't scope
to llmenv's tags. Then: call the Anthropic Messages API over plain HTTPS
using the HTTP client already in the dependency tree (**no new SDK crate**
— a consolidation call is one POST; check `Cargo.toml` for the existing
reqwest/hyper client and reuse it). Model comes from
`ConsolidationConfig.model` (spec R5), defaulting to the current cheapest
Haiku — look up the current model id, don't hardcode from memory without
checking. API key from environment only (never config files, never logged).

Whichever option lands, record the choice and why in a comment at the top
of `src/consolidation/mod.rs`.

## Pipeline (per spec R5 — applies to both options)

1. **Recall** recent episodic memories for the active tags/bundles via the
   existing MCP client (`icm_memory_recall`, keywords `llmenv-tag:<tag>` /
   `llmenv-bundle:<bundle>`, filter `type: episodic`).
2. **Precondition:** require **≥3 episodic records** or skip with an
   informative return string (spec's conservatism rule; also R3's
   don't-consolidate-empty-sessions dependency).
3. **Summarize** with the spec's exact ExpeL-inspired prompt (spec R5
   block), with `max_rules_per_session` substituted for the rule cap
   ("Extract 0–N standing rules… Output nothing if no new rules emerge").
4. **Store** each resulting rule via `icm_memory_store` with
   `type: semantic`, `importance: high`, under the same tag keyword.
   Enforce the cap client-side too: take at most `max_rules_per_session`
   bullets regardless of what the model returns. Empty output = store
   nothing, success.
5. **Return** a one-line human-readable summary ("consolidated N episodic
   memories into M rules for tags: …") — same channel the stub's
   diagnostic used.

## Robustness (issue requirement 4)

- **Timeout:** hard cap the LLM/consolidate call (default 60s,
  overridable via config only if a knob already exists — otherwise
  constant). On timeout: log warn, return gracefully.
- **No streaming needed** — a ≤3-bullet response doesn't justify streaming
  plumbing. Use a non-streaming request. (The issue lists "handle
  streaming"; handle it by not needing it — note this in the PR.)
- **Fail-soft everywhere:** consolidation runs post-session; a failure
  must never surface as a hook error to the engine. Catch, `tracing::warn`,
  return `Ok` with a diagnostic string — mirror the fail-soft policy in
  `tests/hook_run_failsoft.rs`.
- **Output hygiene:** parse only bullet lines from the model output;
  discard prose/preamble; reject rules over ~500 chars (false
  generalization guard).

## Tests

Mock at the boundary (MCP client / HTTP), never internal logic:

1. Disabled config → no calls, empty return (exists — keep passing).
2. <3 episodic records → no summarize call, skip message.
3. Happy path: N records → prompt contains them + the cap → M bullets
   stored as `semantic`/`high` (assert on mock's received store calls).
4. Model returns nothing / non-bullet prose → zero stores, success.
5. Model returns > cap bullets → exactly cap stored.
6. Timeout / HTTP 500 / malformed response → `Ok`, warn logged, no stores.

## Acceptance criteria

- [ ] Stub body replaced; enable-gating and ≥3-record precondition intact.
- [ ] Rules stored as `type: semantic`, `importance: high` via
      `icm_memory_store` (MCP only — never the `icm` CLI).
- [ ] `max_rules_per_session` enforced in prompt **and** client-side.
- [ ] All failure modes fail soft with a warn log.
- [ ] Option A/B decision documented in-module.
- [ ] If Option B adds no new crates: nothing to do for attribution. If
      any dependency changes: `scripts/gen-attribution.sh` + `cargo deny
      check` in the same change (`AGENTS.md`).
- [ ] CHANGELOG `[Unreleased]` entry (keepachangelog skill).
- [ ] Clippy/fmt clean; full suite green.

## Out of scope

- Consolidation scheduling/batching beyond the existing post-session hook.
- Contradiction detection / rule dedup against existing semantic memories
  (file a follow-up if the audit in #555 doesn't already cover it).
