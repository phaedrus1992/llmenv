# Issue #270 — TTL-based memory retention policy and `llmenv memory prune`

- **Issue:** https://github.com/phaedrus1992/llmenv/issues/270
- **Milestone:** Large Projects
- **Type:** Feature
- **Difficulty:** Moderate. The issue body embeds a near-complete
  implementation plan, dependencies have landed, and the CLI surface is
  already stubbed.

## Authoritative spec

Two documents, in order of precedence:

1. The **implementation plan embedded in the issue body itself** (Steps
   1–N with literal code) — read the full issue, not just this doc.
2. `docs/superpowers/specs/2026-06-02-memory-system-improvements.md`,
   section **R4**.

## Current state (verified in code — the issue slightly predates it)

- Dependencies **#267** (memory type tagging) and **#268** (memory
  subcommand) are **closed/landed**. `MemoryType` exists at
  `crates/llmenv-config/src/schema.rs:672`; per-type TTL is possible.
- `llmenv memory prune [--dry-run]` is **already wired** through clap:
  `MemoryCommand::Prune { dry_run }` → `crate::memory::prune(dry_run)`
  (`src/cli/mod.rs:469`), which is a placeholder printing "not yet
  implemented" (`src/memory/mod.rs:118–120`). This issue **fills in that
  placeholder** — no new CLI plumbing needed.
- Path correction: the schema lives in `crates/llmenv-config/src/schema.rs`
  (the issue's "Files" list says `src/config/schema.rs`; ignore that line,
  the issue's own Step 1 has the right path).
- Sibling memory ops (`stats`/`list`/`diff` in `src/memory/mod.rs`) show
  how the module already talks to ICM — mirror their client setup.

## Scope summary

1. **`RetentionConfig`** in `crates/llmenv-config/src/schema.rs` per the
   issue's Step 1: per-type duration strings (`episodic` default `"30d"`,
   `semantic` default `None` = never, `procedural` default `"365d"`),
   parsed with `humantime::parse_duration` (already a dependency — verify
   before assuming; if absent, justify adding or hand-roll `Nd` parsing).
   Plus `retention: Option<RetentionConfig>` and `auto_prune: bool`
   (default false) on the memory feature config. Top-level default:
   `retention: None` → pruning fully off.
   Validate duration strings in `validate()` (reject unparseable).
2. **Prune logic** in `src/memory/prune.rs` (new) per the issue's Steps
   2–3: query ICM for records under active `llmenv-tag:<tag>` /
   `llmenv-bundle:<bundle>` keywords, parse type + ISO-8601 timestamp,
   filter by age against the per-type retention, forget expired ids.
   Return `PruneResult { total_records, pruned, skipped_semantic }`.
3. **Dry-run:** print each candidate (id, keyword, type, age) and counts;
   make **zero** forget calls.
4. **`auto_prune: true`:** run the prune pass during `llmenv materialize`
   (`src/materialize/mod.rs`) — fail-soft: a prune error must warn, not
   fail materialization.

## Hard constraint (from `AGENTS.md`)

All ICM interaction goes through the **ICM MCP** (`icm_memory_recall` /
`icm_memory_forget` etc. via the resolved endpoint — see
`src/hook_run/mcp_client.rs` / `src/mcp/resolve.rs`), **never** the `icm`
CLI. The local CLI writes the wrong store on non-host machines.

## Tests (per the issue's checklist)

1. Records within TTL not pruned; records past TTL pruned (mock the MCP
   boundary — the repo's testing rule is mock boundaries only).
2. `semantic` (retention `None`) never pruned regardless of age.
3. Dry-run makes no forget MCP calls (assert against the mock).
4. Config: round-trip, defaults, invalid duration rejected.
5. `auto_prune` invokes prune from materialize; prune failure doesn't fail
   materialize.

## Acceptance criteria

Issue's own checklist, plus:

- [ ] The `src/memory/mod.rs` placeholder is replaced (no dead stub left).
- [ ] `tests/prune_command.rs` note: that file covers the unrelated
      `llmenv prune` command (#54) — don't confuse the two; memory-prune
      tests go in a new file or `src/memory/prune.rs` unit tests.
- [ ] CHANGELOG `[Unreleased]` entry (keepachangelog skill) with
      forward-merge reconciliation per `AGENTS.md`.
- [ ] Clippy/fmt clean; full suite green.

## Out of scope

- Relevance/decay-scored forgetting (age+type only, per spec R4).
- Pruning memoirs (flat memories only; see #555's audit).
