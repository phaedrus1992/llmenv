# Issue #555 — Audit: use ICM memories/memoirs, not just flat topics

- **Issue:** https://github.com/phaedrus1992/llmenv/issues/555
- **Milestone:** Small Projects
- **Type:** Audit / recommendation — **no implementation in this issue**;
  implementation work becomes follow-up issues.
- **Difficulty:** Easy-moderate. Reading + analysis + one written deliverable.

## Problem

llmenv's ICM integration only ever steers agents toward a flat
topic-keyword convention: `llmenv-tag:<tag>` and `llmenv-bundle:<bundle>`
topics via `icm_memory_store`/`icm_memory_recall`. Nothing in the repo
references the `icm_memoir_*` surface (structured, linkable,
concept-taggable, longer-lived memory units). Flat topics are coarse —
everything under `llmenv-tag:rust` is one undifferentiated bucket across
projects and sessions.

## Deliverable

A new doc `docs/design/icm-memoir-recommendation.md` (or an issue comment
closing #555, if the recommendation is "no change") that answers three
questions with evidence, plus follow-up issues for any implementation.

### The three questions

1. **Where would a memoir beat a flat topic?** E.g. one memoir per bundle
   or per project capturing durable design decisions, with discrete
   memories linked in — versus today's single topic string per tag/bundle.
2. **Should the auto-generated context chunk mention memoirs at all**, or
   stay topic-only by design (simplicity for the agent)?
3. **Is this "leave it to the agent's judgment"** — i.e. agents already
   have the full `icm_*` MCP toolset available; does llmenv need to steer,
   or does steering toward flat topics actively *prevent* better usage?

## Audit steps

1. Read the current wiring end to end:
   - `src/icm.rs` — `generate_context_chunk`, `store_tag_memory`, the
     `icm.json` state file handling.
   - `src/adapter/claude_code.rs` — the SessionStart/SessionEnd hook
     dispatcher that injects the context chunk and auto-recall instruction.
   - `rg -n 'llmenv-tag|llmenv-bundle|icm_memory|icm_memoir' src/ docs/ tests/`
     for every remaining touchpoint (including `tests/icm_tag_mapping.rs`).
2. Enumerate the full `icm_*` MCP surface from ICM's own docs/repo
   (`icm_memory_*`, `icm_memoir_create/add_concept/link/refine/search/
   search_all/show/list/inspect/export`). Document what memoirs offer that
   flat topics don't (structure, linking, curation, lifespan).
3. For each llmenv touchpoint, assess: would a memoir change what the agent
   stores/recalls here, and is that change an improvement for retrieval
   precision? Be concrete — write the hypothetical instruction text the
   context chunk would carry in a memoir world and compare it to today's.
4. Weigh costs: memoirs require curation calls (create/link/refine) that
   burn agent tokens each session; flat topics are one fire-and-forget
   store. The recommendation must account for token cost, not just
   structure elegance.
5. Write the recommendation. **Pick one position** (topic-only / memoirs
   for specific touchpoints / agent's judgment) with justification. If
   implementation is warranted, file one follow-up issue per touchpoint
   change with enough context for offline implementation.

## Constraints

- **All ICM interaction goes through the ICM MCP** (`icm_*` tool calls),
  never the `icm` CLI — see `AGENTS.md` (the CLI writes a local store that
  diverges on non-host machines). The audit's recommendations must preserve
  this.
- New feature code, if any results, goes in llmenv core (`src/icm.rs` and
  the adapter hook wiring), not `examples/`.

## Acceptance criteria

- [ ] Every current ICM touchpoint in the repo is listed in the deliverable
      (verified by the `rg` sweep above).
- [ ] Each of the three questions gets an explicit answer with reasoning.
- [ ] One recommendation chosen; token-cost tradeoff addressed.
- [ ] Follow-up issues filed (or an explicit "no change" close-out).
- [ ] No production code changed by this issue itself.
