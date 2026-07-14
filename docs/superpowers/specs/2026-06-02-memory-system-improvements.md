# Memory System Improvements

**Source:** "Memory for Autonomous LLM Agents: Mechanisms, Evaluation, and Emerging Frontiers"
(Du, arXiv:2603.07670, Mar 2026)

**Purpose:** Gap analysis between the paper's framework and llmenv's current ICM integration.
Recommendations are scoped to the envelope layer (llmenv), not ICM internals.

---

## Paper Framework Summary

The paper formalizes agent memory as a **write–manage–read loop** in a POMDP framework.

**3D taxonomy:**

- *Temporal scope*: working / episodic / semantic / procedural
- *Representational substrate*: context-resident, external vector/DB, parametric weights
- *Control policy*: heuristic, prompted, policy-learned (RL)

**5 mechanism families:** context-resident compression, retrieval-augmented stores,
reflective self-improvement, hierarchical virtual context, policy-learned management.

**Key empirical data:**

- Removing reflection → agents degenerate from coherent multi-day planning to repetitive
  context-free responses within 48 simulated hours (Generative Agents ablation)
- Memory-augmented vs. no-memory gap often exceeds different-model gap
- Models near-perfect on passive recall benchmarks drop to 40–60% on multi-session active
  tasks (MemoryArena) — passive recall ≠ active memory

**Pattern recommendation (§7.6):** Start with Pattern B (context window + external retrieval
store), instrument thoroughly, graduate to Pattern C (tiered + learned control) only when
empirical data justifies it.

llmenv is Pattern B. This is the correct call.

---

## llmenv Current Memory System

Three lifecycle hooks wired via `hook-run`:

| Hook | Event | ICM tool | What it does |
| ------ | ------- | ---------- | -------------- |
| `session_start` | `SessionStart` | `icm_wake_up` | Injects critical memories by importance + recency |
| `turn_start` | `TurnStart` | `icm_memory_recall` | Project-scoped recall + per-tag + per-bundle recalls |
| `session_end` | `SessionEnd` | `icm_memory_store` | Stores active scope context chunk |

Cross-project recall via keyword conventions: `llmenv-tag:<tag>` and `llmenv-bundle:<bundle>`,
both project-unfiltered. Correct design — matches paper §4.2 (RAG with structured keys).

`LLMENV_ICM_CONTEXT` env var carries a markdown chunk with active tags/bundles, giving agents
explicit instructions for scoped storage.

**What llmenv delegates entirely to ICM with no signal:**

- Importance scoring
- Memory eviction / expiry
- Retrieval ranking
- Deduplication

---

## Gaps

### G1: Write-path quality (paper §7.1) — HIGH IMPACT

SessionEnd stores whatever context is active with no filtering, no priority signal, no type
annotation. Paper §7.1: *"storing every interaction verbatim is tempting and almost always
wrong — noise degrades retrieval precision."*

Well-designed write paths include: filtering (reject low-signal records), canonicalization,
deduplication, priority scoring, and metadata tagging (timestamp, source, type, confidence).
llmenv passes none of these signals to ICM.

### G2: No memory type taxonomy (paper §3) — HIGH IMPACT for software agents

All stored memories are undifferentiated keyword-tagged blobs. Paper §3.1 identifies four
types with different retention and retrieval profiles. For software engineering agents
specifically (paper §6.2):

- **Procedural** — verified code patterns, architecture decisions → never expire, high importance
- **Semantic** — project rules, user preferences, conventions → slow update cycle
- **Episodic** — session events, specific interactions → should expire (30–90 days)
- **Working** — current context window → ephemeral (not stored)

Without type distinction, llmenv cannot apply different TTL, retrieval priority, or
consolidation strategies per type.

### G3: No reflective consolidation (paper §4.3) — HIGH IMPACT

No mechanism to distill episodic memories into semantic rules across sessions. Paper ablation:
removing reflection caused agents to degenerate from coherent multi-day planning to repetitive,
context-free responses within 48 simulated hours.

SessionEnd stores raw context chunks. There is no step that extracts standing rules from what
was learned in a session. The consolidation gap — where episodes become semantic knowledge —
is the paper's most consistently cited weakness across deployed systems.

### G4: No selective forgetting (paper §9.4) — MEDIUM-HIGH IMPACT

No TTL, no stale record retirement. Paper: *"nobody evaluates forgetting well"* — identified as
the single biggest gap across all evaluated benchmarks. Raw context chunks accumulate → stale
records surface in TurnStart recall → retrieval precision degrades over time.

Paper §9.4: *"inability to discard outdated information gradually poisons retrieval precision."*

### G5: No memory observability (paper §7.7) — MEDIUM IMPACT

No visibility into the memory store. No `llmenv memory` commands for stats, listing, or diff.
Paper: *"observability infrastructure... its absence is one of the primary reasons that
impressive demo-stage memory systems fail to make the transition to reliable production
deployments."*

Currently impossible to answer: which tags have stale memories? What was written last session?
Why did TurnStart surface the wrong context?

### G6: No contradiction/staleness detection (paper §7.3) — MEDIUM IMPACT

When SessionEnd stores new context for a tag that already has records, nothing checks for
conflicts. Paper: *"robust systems need temporal versioning (prefer newest), source attribution,
contradiction detection, and periodic consolidation."* Current behavior silently accumulates
potentially contradictory records.

### G7: Retrieval quality ceiling (paper §5.4) — DEPENDS ON ICM

TurnStart uses keyword-only recall. Paper: *"primary bottleneck is no longer storage — it is
retrieval quality."* Hybrid retrieval (semantic similarity + temporal ordering) would require
ICM backend changes, not llmenv changes. Flagged for completeness; not actionable at the
envelope layer.

---

## Recommendations

Ordered by impact × feasibility for llmenv (envelope layer only):

---

### R1: Memory type tagging at write time

**Impact: HIGH / Effort: LOW**
**Depends on:** nothing
**Enables:** R4 (TTL), R3 (per-type filtering thresholds), future retrieval routing

Add a `type` field to ICM store calls at SessionEnd. Convention: store keyword annotation
`llmenv-type:episodic` (or `semantic`, `procedural`) alongside existing tag keyword.

Agent specifies type via structured marker in the context chunk:

```html
<!-- llmenv-type: semantic -->
```

llmenv reads the marker and passes it as metadata to `icm_memory_store`. If absent, defaults
to `episodic`.

**Files:**

- `src/hook_run/action.rs` — `Action::Store` gains `memory_type: Option<MemoryType>`
- `src/icm.rs` — `generate_context_chunk()` documents the marker convention;
  `store_tag_memory()` reads it from the chunk before the MCP call
- `src/config/schema.rs` — `features.memory.default_type: episodic`

---

### R2: `llmenv memory` observability subcommand

**Impact: HIGH / Effort: LOW**
**Depends on:** ICM supporting list/search queries (already available via `icm_memory_search`)

New CLI subcommand surfacing memory store state:

```text
llmenv memory stats              # record count by tag/bundle/type, last-written dates
llmenv memory list [--tag <t>]   # show stored memories for active scope
llmenv memory diff               # what changed since last session
llmenv memory prune --dry-run    # preview stale candidates (see R4)
```

Output format mirrors `llmenv doctor` — scannable, actionable.

`diff` compares current store state against a snapshot saved at SessionStart in the state dir
(`state_dir/memory_snapshot.json`). SessionStart already runs before any writes, so snapshotting
there gives a clean before/after.

**Files:**

- `src/cli/mod.rs` — new `Subcommand::Memory` with sub-subcommands
- `src/memory/` — new module: `stats.rs`, `list.rs`, `diff.rs`
- `src/hook_run/mcp_client.rs` — expose query methods already used by hook-run

---

### R3: Write-path filtering and importance annotation

**Impact: HIGH / Effort: MEDIUM**
**Depends on:** R1 (type tagging makes per-type thresholds meaningful)

Two parts:

**Part A — Skip empty/low-signal stores:** Before `icm_memory_store` at SessionEnd, skip if
the context chunk is structurally identical to the last stored chunk (compare hash against
`state_dir/icm.json`). Also skip if chunk falls below a minimum content threshold
(configurable line count or token estimate).

**Part B — Importance passthrough:** Agent annotates chunk with importance level:

```html
<!-- llmenv-importance: critical -->
```

llmenv reads the marker, passes `importance` to the ICM store call. Currently ICM decides
importance with no external signal. With this change, llmenv can also apply config-driven
defaults per type (e.g., procedural memories always stored as `high`).

**Files:**

- `src/icm.rs` — dedup check against state file; parse importance marker
- `src/hook_run/action.rs` — `Action::Store` gains `importance: Option<Importance>`
- `src/config/schema.rs` — `features.memory.default_importance` + per-type overrides

---

### R4: TTL-based selective forgetting + `llmenv memory prune`

**Impact: MEDIUM-HIGH / Effort: MEDIUM**
**Depends on:** R1 (type tags needed for per-type TTL; degrades to age-only without it)

Per-type retention policy in config:

```yaml
features:
  memory:
    retention:
      episodic: 30d
      semantic: never
      procedural: 365d
```

`llmenv memory prune [--dry-run]` queries ICM for records by active tags, filters by age and
type against retention config, calls `icm_memory_forget` for expired records.

Optionally wire to `llmenv materialize` via `features.memory.auto_prune: false` (default off,
opt-in).

**Files:**

- `src/config/schema.rs` — `RetentionConfig` struct under `features.memory`
- `src/memory/prune.rs` — new; query + filter + forget logic
- `src/materialize/mod.rs` — optional prune call if `auto_prune: true`

---

### R5: Reflective consolidation hook (`post_session`)

**Impact: HIGH / Effort: HIGH**
**Depends on:** R1 (type tagging so consolidation output is stored as `semantic`),
R3 Part A (avoid consolidating empty sessions)

New lifecycle hook after SessionEnd. Calls a configured LLM to summarize recent episodic
memories into semantic rules, stores them as `type: semantic` under active tags.

Prompt pattern (ExpeL-inspired, paper §4.3):

```text
Given these recent session memories for tag <tag>:
<recent episodic memories>

Extract 0–3 standing rules (architecture decisions, patterns to follow or avoid,
project conventions learned). Output bullet points only. Output nothing if no new
rules emerge — do not invent rules not evidenced by the memories.
```

Result stored via `icm_memory_store` with `type: semantic`, `importance: high`.

Gated by `features.memory.consolidation.enabled: false` — opt-in, off by default.

**Files:**

- `src/hook_run/mod.rs` — new `HookEvent::PostSession`
- `src/hook_run/action.rs` — new `Action::Consolidate`
- `src/consolidation/` — new module: prompt construction, LLM call, result store
- `src/config/schema.rs` — `ConsolidationConfig { enabled, model, max_rules_per_session }`

Note: introduces an LLM API dependency (Anthropic SDK or compatible). Model should default
to a fast/cheap option (Haiku). Prompt quality is the main risk — bad prompts extract false
generalizations. Start conservative: require ≥3 episodic records before consolidating.

---

### Not recommended (out of llmenv scope)

- **Causal metadata layer** (paper §9.2) — requires ICM vector DB changes
- **Hybrid semantic+temporal retrieval** (paper §7.2) — ICM backend concern
- **Policy-learned management / AgeMem RL** (paper §4.5) — requires training infrastructure,
  overkill for Pattern B

---

## Implementation Order

R1 → R2 → R3 → R4 → R5

R1 is a prerequisite for R3 and R4 to be meaningful. R2 provides observability to validate
that R1–R4 are working correctly. R5 is the highest long-term leverage but highest risk;
validate the write path (R1–R3) and observability (R2) are solid before adding consolidation.
