<!-- markdownlint-disable MD013 -->
# ICM Memoir Audit — Recommendation

**Issue:** [#555](https://github.com/phaedrus1992/llmenv/issues/555)
**Date:** 2026-07-11
**Deliverable:** Answers three questions re: using ICM memoirs (not just flat topics) from llmenv's context chunk and hooks.

---

## 1. Where would a memoir beat a flat topic?

**Answer: Nowhere in llmenv's current integration surface.** Memoirs (structured, linkable, curated concept graphs via `icm_memoir_create`/`add_concept`/`link`/`refine`) are architecturally more expressive than flat topics, but llmenv never writes durable cross-session knowledge — it writes session-scoped observations. A memoir's lifecycle (create → add concepts → link → maintain) costs more agent tokens per session than a single `icm_memory_store` call and provides no retrieval advantage at llmenv's granularity.

Concrete comparison for the three touchpoints where llmenv steers agents:

| Touchpoint | Today (flat topic) | Hypothethical memoir | Verdict |
| --- | --- | --- | --- |
| **Session memory store** (per-session context) | `icm_memory_store` with `topic: llmenv-scope-context` — one call, fire-and-forget | `icm_memoir_create("session-xxx")` → `add_concept` per observation → `link` them — 3-5× calls per session | **Flat topic wins.** Session context is ephemeral; a memoir's structure buys nothing for a one-shot write. |
| **Post-session consolidation** (#595) | `icm_memory_store` with `type: semantic, importance: high` — stores distilled rules | Same via memoir: create memoir per rule set, add concepts per rule — no retrieval benefit since consolidation already returns precisely the rules it stored | **Flat topic wins.** The consolidation pipeline writes its own output; memoir linking provides no signal here. |
| **Agent-initiated recall** | Agent calls `icm_memory_recall("query")` — BM25 search over all stored memories | Agent calls `icm_memoir_search_all("query")` — same BM25 surface, different namespace | **No difference.** Both are BM25 full-text search. A structured graph helps when navigating *relationships* between concepts; an agent looking for "how does auth work" gets the same answer from both. |

**Where memoirs WOULD win** (outside llmenv's scope):

- A human-authored project wiki / design-decision log that needs cross-referencing between decisions
- An agent that actively curates its own knowledge base (reads old memoirs, creates links to new ones)
- Multi-agent systems where one agent's memoir becomes another's context

None of these are what llmenv's hooks do. llmenv writes session context, reads it back, and consolidates it. Memoirs add ceremony without improving recall precision.

---

## 2. Should the auto-generated context chunk mention memoirs?

**Answer: No — stay topic-only by design.**

The context chunk tells agents what MCP tools are available and how to use them. Currently it documents `icm_memory_store` and `icm_memory_recall` with topic conventions. Adding `icm_memoir_*` tool documentation would:

1. **Burn ~200 tokens** in every session's context chunk for tools the agent almost certainly won't use (see Q1).
2. **Increase cognitive load** — agents that see more tools sometimes try them speculatively, making extra MCP calls per session.
3. **No measurable recall win** — the BM25 search surface is identical between memories and memoirs.

The topic-only convention (`llmenv-tag:<tag>`, `llmenv-bundle:<bundle>`) is simpler, cheaper, and sufficient. If a specific agent scenario later demands memoir curation, that agent can discover the tools on its own via `tools/list` — llmenv doesn't need to steer.

---

## 3. Is this "leave it to the agent's judgment"?

**Answer: Yes — and the current steering toward flat topics is correct, not harmful.**

Agents already have the full `icm_*` MCP toolset available (the ICM server exposes all memoir tools). Nothing in llmenv prevents an agent from calling `icm_memoir_create` if it wants to. The context chunk documents the memory tools but does not actively *prevent* memoir usage.

The concern in the issue is: "does steering toward flat topics actively prevent better usage?" The answer is **no** — the steering is documentation, not enforcement. The context chunk says "here is how to tag memories for scoped recall." An agent that decides a memoir would be more appropriate for a specific task can still create one. The flat-topic documentation is the path of least resistance, which is correct: 99% of use cases want flat recall, not curated graphs.

---

## Recommendation

**Status quo — no code changes.** Keep the current topic-only conventions. Do not:

- Add memoir calls to the context chunk or hooks
- Create memoirs during session start/end
- Add memoir-specific config to the schema
- Add `icm_memoir_*` examples to the auto-generated context

**What to do instead:**

1. Close #555 as "won't do" with this document as the rationale.
2. If a future use case arises where an agent demonstrably needs cross-referenced structured knowledge (e.g., a multi-agent codebase where one agent's design decisions are another agent's context), that's when to add memoir steering — but as a new issue, not a reactivation of #555.

---

## Tokens & Cost Appendix

| Operation | Approx MCP calls | Token cost (input) | Notes |
| --- | --- | --- | --- |
| `icm_memory_store` (topic) | 1 | ~100 | One-shot, no setup |
| Memoir: create + N concepts + N links | 2N + 1 | ~150 + N×50 | Each concept/link call burns tokens for both the request and response |
| Memoir: search | 1 | ~80 | Same BM25 as `icm_memory_recall` |

Memoir setup costs 2-5× what a flat store costs, with no corresponding recall-precision gain at llmenv's granularity. Not justified.

---

## Current ICM touchpoints (audit sweep)

Per the issue's acceptance criteria, here is every ICM touchpoint in the repo identified by `rg`:

| File | Usage | Would memoir help? |
| --- | --- | --- |
| `src/icm.rs` — `generate_context_chunk()` | Writes the context chunk documenting `icm_memory_store`/`recall` with topic keywords | No (Q2) |
| `src/icm.rs` — `store_tag_memory()` | Stores tag/bundle context via `icm_memory_store` | No (Q1) |
| `src/adapter/claude_code.rs` | SessionStart injects context chunk; SessionEnd triggers store+consolidation | No |
| `src/hook_run/action.rs` | `Action::Store` and `Action::Recall*` dispatch to `icm_memory_store`/`icm_memory_recall` | No |
| `tests/icm_tag_mapping.rs` | Integration test for tag→keyword mapping | N/A (test only) |
| `src/hook_run/detached_store.rs` | Background store of session context | No |

No touchpoint is a good candidate for memoir replacement.
