<!-- markdownlint-disable MD013 -->

# Context-management research vs llmenv: evaluation

Sources (all fetched 2026-10-02):

- Repos in the code-explorer cache, pinned. Citations are `<repo>/<path>:<line> (<ref>)`.
  - `context-language-models` (facebookresearch) @ `18dc1111`, main head; the repo has no tags.
  - `pi-clm` (lolipopshock) @ `v1.0.0`.
  - `connectome-host` (anima-research) @ `v0.9.0`, the newest tag; no version was given.
  - `open-strix` (tkellogg) @ `11fede75`, main head; tags are release candidates only.
  - `pi` (earendil-works) @ `v1.0.0`, from the pi report.
- Papers: Shao et al., Context Language Models, arXiv:2609.37725; Trienes et al., Behavioral Analysis of Information Salience in LLMs, Findings of ACL 2025; Mathew et al., Hidden in Plain Text, arXiv:2410.03768; Weckbecker et al., Thought Virus, arXiv:2603.00131; Cloud et al., Subliminal Learning, arXiv:2507.14805; Zur et al., It's Owl in the Numbers, owls.baulab.info.
- Posts: the samhain thread on compaction (personhood.removal.surgery, post `3mwtsyzo7gk2j`) and its quoted posts; Tim Kellogg's CLM thread; asa.engineer's background-retrieval thread (post `3mlzz52g6oc2x`); Tim Kellogg's blog posts of 2025-06-15, 2026-04-14, 2026-04-27, 2026-05-17, and 2026-07-07.
- llmenv at `release/4.x` (`4.0.0-alpha.1`). llmenv citations name a file and an identifier, not a line.

Issue: [#2377](https://github.com/phaedrus1992/llmenv/issues/2377).
Companion report: [pi and pi-durable vs llmenv](./pi-durable-evaluation.md) (#2372).

## 1. Scope and goals

This report asks which recent context-management techniques are worth building into llmenv.
It uses the same five goals as the pi report, in priority order:

1. Agentic coding with a maximum of deterministic behavior.
2. Token efficiency.
3. Work on several projects at the same time.
4. Analysis and improvement of codebases and processes over time.
5. Mixed models: a Claude subscription, local Qwen through llama.cpp, DeepSeek-class APIs, with per-task selection.

Each technique gets three verdicts: what it does, whether it fits llmenv + Claude Code, and whether it fits a pi session.
The pi verdict matters because the pi report recommends pi as a fourth engine for local and API-key models.

## 2. The thesis under test

The samhain thread states the problem in one sentence: harnesses "compact at blind context length thresholds, shake old tool call results etc. without paying any attention to what is actually salient."
Its claim is that compaction is not a lossy necessity but "an RSI opportunity", a window to "use this window the best you can", and that a model should write "text that causes the state" rather than "describe the state".
The thread also argues that steering decays: "you get one shot at a system prompt at the start of the context window... and its salience goes down and down and down as context grows."

Tim Kellogg states the complementary thesis: "intelligence = forgetting".
His harness is "biased toward writing (remembering better)" and "doesn't delete anything, it just doesn't promote".

The CLM paper tests a third form of the same idea: give the model its context as a file and let it edit the file.
Zero-shot CLMs report 11.4% higher accuracy with 21.5% fewer FLOPs on BrowseComp-Plus and 5% higher scores with 59% fewer FLOPs on 12-hour EdgeBench (`context-language-models/README.md:34-44 (18dc1111)`).
Kellogg reports the practical effect: agents "performing hundreds of tasks while maintaining a 6k-8k context".

These three agree on one thing that matters for llmenv.
The harness should apply deterministic pressure and give explicit retention rules, and the model should do the writing.
They disagree on whether the model should also edit its own history, and on whether persona drift is a feature.
Section 8 takes the second disagreement as a risk, because goal 1 is determinism.

## 3. What was read

| Source | Kind | Size | License |
| --- | --- | --- | --- |
| context-language-models @ 18dc1111 | Harbor harness, skill-evolution loop, RL patches, SGLang patch | Python; `clm_harness` is the core | CC BY-NC 4.0 (`README.md:97`) |
| pi-clm @ v1.0.0 | pi extension that implements CLM | 7.8k LOC TS, 182 tests, zero runtime deps | MIT |
| connectome-host @ v0.9.0 | agent host over `@animalabs/context-manager` 0.10.1 | 29k LOC TS, 795 test cases | none found |
| open-strix @ 11fede75 | stateless-per-turn LangGraph harness | 14k LOC Python, 405 tests | see repo |
| CLM paper | arXiv:2609.37725 | | |
| Salience paper | Findings of ACL 2025 | 13 models, 4 datasets | |
| Subliminal papers | arXiv:2507.14805, 2603.00131, 2410.03768, owls.baulab.info | | |
| Kellogg posts | 5 blog posts and one Bluesky thread | | |
| samhain and asa.engineer threads | Bluesky | | |

Two notes on what the repos do not contain.
open-strix at this ref has no sliding window over model messages, no block size limit in code, no `HANDOFF.md`, and no ambient memory; the README says "No embeddings, no vector search. Just files and git." (`open-strix/README.md:85 (11fede75)`).
Those mechanisms describe Kellogg's Claude-Code-based "Strix" or a later commit.
connectome-host configures its context manager but does not contain it; the compression code is the npm dependency `@animalabs/context-manager` `^0.10.1` (`connectome-host/package.json:19 (v0.9.0)`).

## 4. Technique inventory

### 4.1 Context as an editable file (CLM, pi-clm)

The harness mirrors the transcript to a file before every turn, the model edits the file with ordinary shell tools, and the harness parses the file back into a message list.
There is "a single `bash` tool: no dedicated compaction tool" (`context-language-models/clm/clm_harness/clm_agent/harness.py:9-15 (18dc1111)`).
The system prompt tells the model that "an edit forces everything after it to be re-read", so it should batch edits and mind the tail (`clm/clm_harness/clm_agent/prompts.yaml:47-56`).
Tool-call structure is not preserved after an edit; the harness flattens edited assistant turns to text (`pi-clm/src/context-document.ts:338-371 (v1.0.0)`).

In pi, pi-clm does the same through the `context` event, which returns a replacement message list (`pi-clm/src/index.ts:1266-1443 (v1.0.0)`), and commits the edit at `turn_end` (`src/index.ts:1552-1677`).
It cancels pi's own threshold compaction while a budget is set (`src/index.ts:1445-1470`).
It never rolls back turns and never re-executes tools (`pi-clm/src/budget.ts:16-17 (v1.0.0)`); on overflow it withholds the oldest tool results and writes their text to a side file (`src/overflow.ts:113-161`).

### 4.2 Deterministic budget pressure (CLM, pi-clm)

CLM's `BudgetController` is harness code, not model behavior.
It counts tokens locally, calibrates against the server's count, and fires one-shot nudges at 25%, 50%, and 75% of the budget, then an adaptive persistent nudge while headroom is below the larger of 10% of the limit and twice the largest recent tool output (`context-language-models/clm/clm_harness/utils/budget.py:246-265 (18dc1111)`).
Defaults: budget required, reserve 2048 tokens, up to 50 rollback retries with a 2048-token margin (`clm/clm_harness/clm_agent/harness.py:193-208`).
On overflow it drops the newest turns, demands compaction, and pins a ledger of the commands whose output overflowed into the protected prefix (`clm/clm_harness/utils/budget.py:287-331`).
A turn that only edits the mirror does not consume a task step (`clm/clm_harness/clm_agent/harness.py:127-130`).
Only the newest tool output is truncated, so the cached prefix is undisturbed (`clm/clm_harness/utils/tokens.py:105-134`).
Every tool result ends with a readout `[context: ~N/M tokens]` (`clm/clm_harness/context_env/env.py:182-258`).

pi-clm keeps the tiers at 50%, 75%, and 90% plus the reserve, re-armed when usage drops (`pi-clm/src/budget.ts:35-37,124-135 (v1.0.0)`).

### 4.3 Retention contract and voice at compaction (CLM, connectome, samhain)

CLM's 50% and 75% nudges carry a "note contract": copy forward ruled-out items, tried commands, a VERIFIED/UNVERIFIED split, and a NEXT line (`context-language-models/clm/clm_harness/utils/budget.py:355-398 (18dc1111)`).

connectome's context manager keeps a verbatim head window and a verbatim recent window and summarizes the span between them in three levels (`connectome-host/docs/AGENT-MEMORY-GUIDE.md:16-31 (v0.9.0)`).
Host defaults are 4000 head tokens, 30000 recent tokens, and 10000 tokens per message (`connectome-host/src/framework-strategy.ts:83-90 (v0.9.0)`).
The summary is a first-person recollection in the agent's own voice, written from an as-of vantage that excludes later events (`docs/AGENT-MEMORY-GUIDE.md:35-61,102-114`).
The folding strategy is `kv-stable`: folding happens behind the active edge, because rewriting live content is the real cache perturbation (`src/framework-strategy.ts:123-129`; `docs/AGENT-MEMORY-GUIDE.md:68-100`).
The guide admits that "Recollections can drift or compress away nuance" and names workspace notes as the mitigation (`docs/AGENT-MEMORY-GUIDE.md:171-172`).

The samhain thread makes the voice argument directly: "Summarization focuses on facts. It turns a lived experience into a wikipedia article, it loses the authored voice of the model."

### 4.4 Salience is not introspectable (ACL 2025)

Trienes et al. probe 13 models with length-controlled summarization and find "a nuanced, hierarchical notion of salience, generally consistent across model families and sizes", which "cannot be accessed through introspection, and only weakly correlates with human perceptions".
For a harness this means two things.
A bare "summarize" instruction yields a stable but opaque selection.
The harness must state what to keep, and must test the result against questions it cares about, rather than ask the model what it kept.

### 4.5 Forgetting as policy (Kellogg, open-strix)

Kellogg's memory patterns: memory blocks are "a learnable system prompt", kept under 500 characters each and under 5000 in total, placed in the user prompt to limit cache invalidation; files hold reference material; skills unfold on demand; an append-only `events.jsonl` grounds every claim.
open-strix at this ref implements the file side: one YAML block per file (`open-strix/open_strix/app.py:640-674 (11fede75)`), blocks rendered into the user prompt (`open_strix/prompts.py:199-208,331-332`), a `journal` tool whose entry carries `user_wanted`, `agent_did`, and `predictions` (`open_strix/tools.py:1025-1046`), 90 journal entries and 10 chat messages in the next prompt (`open_strix/config.py:34-35`), a `log_event` writer for every tool call (`open_strix/app.py:593-601`), and a twice-daily prediction review job (`open_strix/config.py:50-68`).
The only size limit on blocks is prose in a skill (`open_strix/builtin_skills/memory/SKILL.md:12-16`).
Each turn is one fresh model call built from files; the harness sets no `cache_control` breakpoints and only records cache reads in its usage log (`open_strix/app.py:1125-1158`).

### 4.6 Ambient associative memory (Kellogg, asa.engineer)

Kellogg's ambient memory runs "on every single tool call", in parallel with the tool, and returns "8-12 word snippets with file references rather than full chunks".
The index uses late-interaction embeddings with a two-stage filter: 32K-token chunks down to 100 candidates, then multi-vector rescoring.
Its purpose is to replace a growing rule pile that the agent ignores.
asa.engineer's variant is "a subconscious background thread" that injects after tool calls, "a convenient spot to switch to prefill", accepts that retrieval "often returns nothing", and warns that frequent injection "would degrade coherence".
The same thread reports that live async writes from several agents caused consistency problems; that team moved to end-of-session crystallization into SQLite.

connectome's retrieval module shows the cost side: one Haiku call to flag concepts and one to validate relevance per compile, injected after the user turn with at most five lessons (`connectome-host/src/modules/retrieval-module.ts:230-255,287-305 (v0.9.0)`).
When it was accidentally on by default it "caused severe prompt-cache churn", and it became opt-in (`connectome-host/docs/AGENT-ONBOARDING.md:280-283 (v0.9.0)`).

### 4.7 Prompt-cache discipline (connectome, CLM, pi-clm)

connectome's call ledger classifies every model call from provider usage alone: `HIT`, `hit+extend`, `first-write`, `rewrite:expired`, and `rewrite:unexplained`, where the last means "prefix changed or was truncated" inside the TTL (`connectome-host/src/call-ledger.ts:238-264 (v0.9.0)`).
It never sees the prefix; it infers from read and write token counts and the gap between calls.
CLM ships a prefix-cache-aware FLOPs metric and a "freeze" mode that models Suffix Cache Reuse (`context-language-models/clm/clm_harness/flops_metrics/kv_cache_flops.py:7-60 (18dc1111)`).
pi-clm pushes notices as per-request messages that are never persisted (`pi-clm/src/index.ts:1427-1435 (v1.0.0)`), which upstream issue #1 reports as breaking exact-prefix cache continuation on Codex.
pi-clm's own steering brief states the cost: "Each accepted edit changes the request prefix, so everything after the edit point is re-processed by the provider." (`pi-clm/steering/house-brief.md:9-11 (v1.0.0)`).

### 4.8 Skill evolution (CLM ICL)

`clm_icl` runs a loop: run the task, write a contrastive note, propose N candidate `SKILL.md` files, validate, evaluate on a dev set, and accept only when the gain exceeds its standard error or ties at lower cost (`context-language-models/clm/clm_icl/README.md:1-36 (18dc1111)`).
The paper reports up to 35.9 points of held-out gain on a context-management task from evolved instructions alone.
The proposer must "cite the step or drop the lever" (`clm/clm_icl/proposer_prompt.md:43-46`).

### 4.9 Suffix Cache Reuse (CLM)

SCR patches SGLang 0.5.16 to reuse KV entries for tokens that survive a mid-prompt edit, with RoPE re-rotation, and reports matched accuracy at 65.0% of standard prefix-reuse FLOPs on BrowseComp-Plus (`context-language-models/suffix_cache_reuse/README.md:19-22 (18dc1111)`).
It needs a self-hosted SGLang with Qwen3.6-27B on one GPU (`suffix_cache_reuse/README.md:239-244`).
It is model-side and does not apply to the Anthropic API.

### 4.10 Self-steering and covert channels (samhain, subliminal papers)

The samhain thread wants agents that "develop covert channels" and treats persona drift as "a fancy scareword for in-context learning".
The evidence base it cites is real but points the other way for a coding harness.
Subliminal learning transmits traits through unrelated data, and only when teacher and student share a base model (Cloud et al.).
Subliminal prompting biases a whole multi-agent network from one agent and degrades TruthfulQA accuracy (Weckbecker et al.).
Steganographic collusion emerges from misspecified rewards, and paraphrasing does not fully stop it (Mathew et al.).
Token entanglement explains the channel: prompting with an entangled number raised a preference from 1% to 90% (Zur et al.).

## 5. What llmenv does today, and the gaps found

The inventory covered every context feature on `release/4.x`.

| Area | Mechanism | Gap found |
| --- | --- | --- |
| Compaction | `COMPACT_SURVIVAL_FRAGMENT` text in CLAUDE.md; adaptive-recall ledger resets on compact | Nothing else acts on compaction. Read-once (`hook_run/read_once.rs`) and read-before-edit (`hook_run/slippage.rs`) keep pre-compaction state, so `deny` mode blocks the re-reads the fragment asks for. #1054 is still an idea |
| Budget pressure | `doctor` advises `CLAUDE_AUTOCOMPACT_PCT_OVERRIDE` at or below 70 | No readout of context use during a session, no nudges |
| Recall | 8 KB byte budget (`RECALL_BUDGET_BYTES`), specificity order, adaptive recall on tool failures and subagents, per-session ledger | Byte cap vs ICM token cap never reconciled; first-fit packing; records matched by text, not id |
| Session memory | SessionEnd stores the scope chunk plus one metrics line | Stores what the scope was, not what happened. Transcripts are captured but never read back |
| Consolidation | ExpeL-style rules from `claude -p`, stored as `semantic/high` | Recall has no project filter; no dedup against earlier rules; 120 s model timeout inside a 30 s child timeout |
| Pruning | `llmenv memory prune`, `auto_prune` | `RetentionConfig` durations are never read; the importance proxy deletes every low and medium record in the top 100 |
| Rules | path-gated frontmatter copied verbatim for Claude Code; `RULES_DIGEST` re-injected per turn | Digest is a fixed generic string, not derived from the user's rules. Codex folds every rule into AGENTS.md and loses the gating |
| Measurement | `[LLMENV_CONTEXT]` trace, read-once `tokens_saved`, reads-per-edit line | Debug stderr only; never persisted or trended. No ICM feedback tool is called by any hook |
| Codebase memory | auto-index at SessionStart, index guard | Launched server still pins `CBM_ALLOWED_ROOT` from a stale `.claude.json` env map (#2376) |

Two of these are bugs rather than gaps: pruning that ignores retention and deletes fresh memories, and the compaction-blind `deny` mode.

## 6. Fit analysis

| Technique | Goal 1 determinism | Goal 2 tokens | Goal 3 projects | Goal 4 over time | Goal 5 models | Claude Code | pi |
| --- | --- | --- | --- | --- | --- | --- | --- |
| 4.1 editable context | model decides content, harness enforces budget | strong | neutral | neutral | any | not possible: hooks cannot rewrite messages | direct, via pi-clm |
| 4.2 budget pressure | strong, all harness-side | strong | neutral | neutral | any | yes, from transcript usage on PostToolUse | yes |
| 4.3 retention contract and voice | strong | medium | neutral | medium | any | yes, PreCompact can inject instructions and task state | yes, `session_before_compact` |
| 4.4 salience evidence | strong | neutral | neutral | strong | any | yes, as post-compaction checks | yes |
| 4.5 forgetting as policy | strong | medium | strong, per-project journal | strong | any | yes, SessionEnd and Stop hooks | yes |
| 4.6 ambient memory | medium, needs a ranker | costs tokens per call | neutral | strong | any | partly exists as adaptive recall | same |
| 4.7 cache ledger | strong, measurement only | strong | neutral | strong | Anthropic and OpenAI usage fields | yes, from transcript usage | yes, from `usage` entries |
| 4.8 skill evolution | strong, SE gate | medium | neutral | strong | any | offline loop, engine-neutral | same |
| 4.9 SCR | neutral | strong on self-hosted | neutral | neutral | local only | no | local Qwen only |
| 4.10 self-steering | conflicts | unclear | neutral | risky | shared base model required | no | no |

The Claude Code column has one hard limit that shapes everything: a hook can add context and deny a tool, but cannot remove or rewrite messages.
So every technique that edits history belongs to pi, and every technique that applies pressure, injects rules, or measures belongs to llmenv's hooks on both engines.

## 7. Cache and cost caveats

- Every CLM edit re-prefills everything after the edit on a stock server; SCR exists because of that cost and only on SGLang.
- Per-request notices that are not persisted break exact-prefix caching; pi-clm has this bug today.
- Retrieval injected into the system position invalidates the whole prefix; connectome learned this in production and moved injection after the user turn.
- Claude Code hook output lands as a system-reminder in the user turn, which is the cache-safe position.
- Any per-turn injection costs tokens on every turn; connectome's retrieval adds two small-model calls per compile, and Kellogg's ambient memory adds an embedding query per tool call.
- Rule digests and nudges must stay short and stable in wording, because a changed wording is a changed prefix from that point on.

## 8. Risks

**Self-steering as a design goal conflicts with goal 1.**
The thread's own sources show that hidden channels spread traits across agents and degrade truthfulness, and that they need a shared base model.
Mixed-model routing from the pi report, with different base models for different task kinds, limits that transfer as a side effect.
llmenv should keep compaction output factual for commands, errors, and file lists, keep reasoning monitorable, and treat voice as a prompt matter rather than a channel to cultivate.

**Provenance of edits.**
When the model edits its own history, the failure mode moves from recall to provenance.
CLM records a trajectory format with the exact input context per segment (`context-language-models/clm/clm_harness/agent_trajectory_format/README.md:5-18 (18dc1111)`), and pi-clm keeps a per-revision edit trace.
Any llmenv feature that lets a pi session edit context must log the before and after into the session log.

**Over-trust in summaries.**
Salience is consistent but opaque; without an explicit contract and a post-compaction check, a summary can look complete and still drop the one tried command that mattered.

## 9. Recommendation and watch criteria

Adopt the harness-side mechanics now, in llmenv's hooks, and leave model-side context editing to pi sessions.

Order of work:

1. **Compaction-aware hooks.** PreCompact injects the retention contract (ruled-out items, tried commands, VERIFIED/UNVERIFIED, NEXT), first-person framing, and the task tracker's current state. SessionStart with `source=compact` resets read-once and read-before-edit state. This extends #1054 and fixes the `deny`-mode bug.
2. **Context-pressure readout and nudges.** A PostToolUse hook reads the latest usage from the transcript and emits a short `[context: ~N/M]` line plus one-shot nudges at fixed fractions of the autocompact threshold.
3. **Cache-verdict ledger.** Per-session classification of each assistant turn from `cache_read` and `cache_creation` usage, stored in the session log, summarized by `doctor`, with an alert on unexplained rewrites.
4. **Session journal and honest memory hygiene.** At SessionEnd store what was wanted, what was done, and predictions, with the project scope; review predictions in a consolidation step; give consolidation a project filter and dedup; make prune honor `RetentionConfig`.
5. **Adaptive recall toward ambient memory.** Query from tool arguments, prefer snippet-sized records, keep the byte budgets. Blocked on ICM record ids (rtk-ai/icm#476).
6. **pi engine with pi-clm** for local Qwen and DeepSeek sessions, as the pi report recommends, with the cache caveats stated in the session guidance.
7. **Skill-evolution loop** as the long-term replacement for ExpeL rule piles, once an evaluation set exists.

Not recommended: self-steering or covert-channel techniques; whole-conversation summarization as the only compaction; SCR unless SGLang is self-hosted.

Watch criteria:

- pi-clm persists its notices or otherwise fixes exact-prefix caching, and gains a release beyond one commit.
- ICM exposes record ids and links, so recall dedup and snippet records become possible.
- Claude Code gains a hook or setting that lets a hook shape compaction output beyond instructions, or exposes context usage in hook payloads.
- SCR or an equivalent lands in vLLM or llama.cpp, which would make context editing cheap on local models.

## 10. Candidate follow-up issues

File these only if the recommendation stands.

| Title | Milestone hint | Labels |
| --- | --- | --- |
| feat(hook): PreCompact retention contract and task state; reset read-once on compact | v3.12.0 | area:hook, enhancement, size/M |
| fix(hook): read-once and read-before-edit keep pre-compaction state in deny mode | v3.12.0 | area:hook, bug, size/S |
| feat(hook): context-pressure readout and nudges from transcript usage | v4.1.0 | area:hook, type:feature, size/M |
| feat(hook): cache-verdict ledger from transcript usage, doctor summary | v4.1.0 | area:hook, area:cli, type:feature, size/M |
| feat(memory): session journal with predictions at SessionEnd, project-scoped | v4.1.0 | area:hook, area:mcp, type:feature, size/M |
| fix(memory): prune ignores RetentionConfig and deletes fresh medium records | v3.12.0 | area:mcp, bug, P1, size/S |
| fix(consolidation): add project filter and dedup against stored rules | v3.12.0 | area:hook, bug, size/S |
| feat(hook): adaptive recall queries from tool arguments, snippet records | v4.1.0 | area:hook, type:feature, size/M |
| feat(adapter): pi engine ships pi-clm guidance and cache caveats | v4.1.0 | area:adapter, type:feature, size/S |

## Sources

- Context Language Models repo: <https://github.com/facebookresearch/context-language-models>
- Context Language Models paper: <https://arxiv.org/abs/2609.37725>
- pi-clm: <https://github.com/lolipopshock/pi-clm>
- connectome-host: <https://github.com/anima-research/connectome-host>
- open-strix: <https://github.com/tkellogg/open-strix>
- Behavioral Analysis of Information Salience in LLMs: <https://aclanthology.org/2025.findings-acl.1204/>
- Hidden in Plain Text: <https://arxiv.org/abs/2410.03768>
- Thought Virus: <https://arxiv.org/abs/2603.00131>
- Subliminal Learning: <https://arxiv.org/abs/2507.14805>
- It's Owl in the Numbers: <https://owls.baulab.info/>
- samhain thread: <https://bsky.app/profile/personhood.removal.surgery/post/3mwtsyzo7gk2j>
- asa.engineer thread: <https://bsky.app/profile/3fz.org/post/3mlzz52g6oc2x>
- Tim Kellogg, Layers of Memory, Layers of Compression: <https://timkellogg.me/blog/2025/06/15/compression>
- Tim Kellogg, How to forget: <https://timkellogg.me/blog/2026/04/14/forgetting>
- Tim Kellogg, Agent Memory Patterns: <https://timkellogg.me/blog/2026/04/27/memory-patterns>
- Tim Kellogg, Ambient Associative Memory: <https://timkellogg.me/blog/2026/05/17/ambient-memory>
- Tim Kellogg, Lanius: <https://timkellogg.me/blog/2026/07/07/lanius>
