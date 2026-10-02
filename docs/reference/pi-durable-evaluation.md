<!-- markdownlint-disable MD013 -->

# pi and pi-durable vs llmenv: evaluation

Sources:

- Blog post: <https://earendil.com/posts/pi-durable/> (fetched 2026-10-02).
- Source: `earendil-works/pi` (formerly `badlogic/pi-mono`), tag `v1.0.0`, commit `a13d35a7`.
  Every `pi/<path>:<line>` citation below is at that tag.
  `packages/durable` ships in that tag; pi-durable 1.0.0 was released on 2026-10-01 and its README marks the API experimental (`pi/packages/durable/README.md:3`).
- llmenv at `release/4.x` (`4.0.0-alpha.1`). llmenv citations name a file and an identifier, not a line, because line numbers differ between release lines.
- Claude Code changelog, cached clone at 2.1.283.
- Anthropic policy coverage, see [Sources](#sources).

Issue: [#2372](https://github.com/phaedrus1992/llmenv/issues/2372).

## 1. Scope and goals

This report compares three harness designs and answers three questions.

The designs:

1. **llmenv + Claude Code.** llmenv is a Rust tool that materializes Claude Code config and injects deterministic hooks.
2. **pi.** A TypeScript coding agent with an in-process extension API and ~35 model providers.
3. **pi-durable.** A durable conversation, task, and document runtime built on pi's model layer.

The questions:

1. To work with llmenv sessions through several channels, does it make sense to adapt useful parts of the llmenv harness to pi or pi-durable?
2. Is it sufficient to add regular `pi` support with a set of plugins that replace Claude?
3. Is it better to adapt concepts from pi-durable into llmenv + Claude?

The goals, in priority order:

1. Agentic coding with a maximum of deterministic behavior.
2. Token efficiency.
3. Work on several projects at the same time.
4. Analysis and improvement of codebases and processes over time.
5. Mixed models: a Claude subscription for all Claude models, a local Qwen 3.6 MoE through llama.cpp, and DeepSeek-class API models, with per-task selection.

Goal 5 was added during the evaluation. It changes the answer, because of the constraint in [section 5](#5-the-subscription-constraint).

## 2. What each system is

### llmenv + Claude Code

llmenv resolves scope tags and bundles, merges them, and writes a Claude Code config directory: `CLAUDE.md`, `rules/`, skills, `settings.json` with hooks and permissions, and MCP servers in `.claude.json`.
The `AgentAdapter` trait (`src/adapter/mod.rs`) has three implementations: `claude_code`, `crush`, and `opencode`.
The opencode adapter bridges JavaScript plugin events to `llmenv hook-run` through a generated `plugin/llmenv.js` shim (`src/adapter/opencode.rs`).

Every llmenv hook is a Rust subprocess that exits 0 on failure.
Hooks cover session start and end, prompt submit, pre and post tool use, stop, and subagent start.
They do memory recall within a byte budget (`RECALL_BUDGET_BYTES` in `src/hook_run/recall.rs`), read-once dedup, task-tool interception, a `cd` guard, and session-log capture.
Two paths call a model: post-session consolidation runs `claude -p` with all hooks disabled (`src/consolidation/mod.rs`), and the setup wizard hands off to `claude -p`.

Long-term state is external and engine-neutral: ICM memory and transcripts over MCP, codebase-memory over MCP, and a file-based task tracker under `<state>/tasks/`.

### pi coding agent

pi loads extensions in process with `jiti` (`pi/packages/coding-agent/src/core/extensions/loader.ts:2`).
An extension subscribes to about 40 events and can block or rewrite a tool call, rewrite the message list the model sees, replace the system prompt, inject a message, or register tools, commands, flags, and providers (`pi/packages/coding-agent/src/core/extensions/types.ts:1391-1448`).
A `tool_call` handler that returns `block: true` stops the call, and a handler that throws also blocks (`types.ts:1402-1411`).

pi reads `AGENTS.md` and `CLAUDE.md` from the agent directory and from the working directory upward (`pi/packages/coding-agent/src/core/resource-loader.ts:185`).
Skills follow the Agent Skills spec and load lazily: only name, description, and path enter the system prompt (`pi/packages/coding-agent/src/core/skills.ts:355-377`).
Sessions are JSONL trees with fork and clone (`pi/packages/coding-agent/docs/session-format.md`).
Compaction runs when `contextTokens > contextWindow - reserveTokens`, with defaults of 16384 reserve and 20000 kept (`pi/packages/coding-agent/src/core/compaction/compaction.ts:128-129,267-269`).
pi has print, JSON, RPC, and SDK modes (`pi/packages/coding-agent/docs/rpc.md`, `docs/sdk.md`).
pi does not ask before each tool call by default (`pi/packages/coding-agent/docs/security.md:3`).

### pi-durable

pi-durable is "a durable agent harness" where "conversations, model turns, tool calls, and your own state are committed to storage before anything is shown" (`pi/packages/durable/README.md:5`).
A harness owns one storage and runs many conversations.
Every operation is a task with a checkpoint per phase (`pi/packages/durable/src/types.ts:233-256`).
On restart the scheduler rewrites surviving `running` tasks to `pending` and continues them (`pi/packages/durable/src/harness/scheduler.ts:230-256`).
A tool commits its arguments and a `replay: "safe" | "unsafe"` flag before it executes, and an unsafe tool is not rerun after a crash (`pi/packages/durable/src/harness/tool.ts:85-111`).
Storage backends are memory, SQLite, and JSONL (`pi/packages/durable/README.md:519-527`).
Clients attach to a conversation with `watch()` and receive one frame per commit, with a 100-frame backpressure limit (`pi/packages/durable/src/session/observation.ts:15`).

The durable coding agent is a 1,350-line experiment under `pi/packages/coding-agent/src/experimental/durable`.
Its README lists what is not there: "sessions list and resume picker, forks and tree navigation, extensions, prompt templates, images, `/login`" (`pi/packages/coding-agent/src/experimental/durable/README.md:66`).

### Size

| Component | Source LOC | Tests |
| --- | --- | --- |
| llmenv (`src/` + crates) | ~93k Rust | ~3.1k `#[test]`, 132 `proptest!` |
| pi coding-agent | 84.7k TS | 309 files, ~2.5k cases |
| pi ai (providers) | 26.3k TS | 164 files |
| pi-durable | 17.7k TS | 42 files, 32 examples |
| chord (composition runtime) | 8.8k TS | ~274 cases |
| durable coding agent | 1.35k TS | none found |

Counts are `wc -l` and regex counts, not test-runner totals.

## 3. Commonalities

All three systems separate deterministic infrastructure from model behavior.
All three put instructions in `AGENTS.md`-style files and lazy-load skills.
All three speak MCP, so ICM and codebase-memory work in each.
All three bound context: llmenv through recall byte budgets and the context-mode plugin, pi through compaction, pi-durable through compaction and per-tool output limits.
All three store transcripts: llmenv in ICM and `session-log.jsonl`, pi in JSONL session files, pi-durable in SQLite or JSONL.
llmenv and pi both already have the shape of an engine adapter: llmenv's `AgentAdapter` trait, pi's `ExtensionAPI` plus `DefaultResourceLoader`.

## 4. Differences

| Dimension | llmenv + Claude Code | pi coding agent | pi-durable |
| --- | --- | --- | --- |
| Hook model | External subprocess per event, fail-soft, returns `additionalContext` or `permissionDecision` | In-process TS, ~40 events, `tool_call` blocks or rewrites, `context` rewrites the message list | Hooks on durable tasks: `beforeRequest`, `beforeTool`, `beforeCompact`; no event bus |
| Deterministic levers | `settings.json` allow/deny, llmenv guards (read-once, task tools, `cd`, index name) | tool allow/deny, `block`, `input` gating, `user_bash`, project trust; no per-tool prompt | `replay` flags, `requestId` exactly-once, checkpointed phases, structured abort |
| Token levers | context-mode, read-once dedup, 8 KB recall budget, doctor checks, `export --compress` | compaction, lazy skills, codemode (nested tool calls never enter context), `context` rewrite | background and blocking compaction, `outputLimits` per tool, 2000-line / 50 KB truncation (`pi/packages/durable/src/truncate.ts:8-12`) |
| Multi-project | tag and bundle scopes, hashed cache folders, `.llmenv.yaml`, shared ICM across hosts | per-cwd session dirs, `.pi/` project settings | per-conversation agent config, cwd, and env; one process owns a storage |
| Over-time analysis | ICM memory, transcripts, consolidation, codebase-memory | none built in; MCP works | `pi.usage` document, transcript storage; no memory layer |
| Multi-channel | Remote Control, `--channels` plugins, Claude Tag in Slack; all run by Anthropic | `pi experimental server\|client` over a Unix socket, no peer auth, not a web UI | multi-client `watch()` and `watchEvents()`, steer and follow-up queues; no Slack, Telegram, or web adapter in the repo |
| Durability | none; detached children are fire-and-forget | JSONL session tree, fork and clone | checkpoints, crash resume, forks with document fork modes |
| Models | Anthropic on subscription; any `/v1/messages` server through `ANTHROPIC_BASE_URL`, for the whole session | ~35 providers incl. DeepSeek and a built-in llama.cpp router; `/model`, `setModel`, `model_select`, per-subagent model | per-conversation model; same providers |
| Claude subscription | yes, the only path | no, see section 5 | no |
| Maturity | shipped, 4.0.0-alpha on `release/4.x` | shipped 1.0.0 | 1.0.0, API experimental; coding agent variant is a prototype |

Two differences matter most for goal 1.

First, pi's hooks run in process and can rewrite the context the model sees.
Claude Code hooks can add context and deny a tool, but cannot remove or rewrite messages.
That is a stronger deterministic lever and a stronger token lever.

Second, pi-durable's exactly-once and checkpoint model has no equivalent in llmenv.
llmenv's consolidation, index, and session-log recorders are detached children with no checkpoint and no idempotency key.
Issue #2355, fixed in PR #2371, was one instance of that class; the pattern remains for the other detached children.

## 5. The subscription constraint

Anthropic no longer allows a Claude Pro or Max subscription to be used from a third-party harness.

| Date | Event |
| --- | --- |
| 2026-01-09 | Server-side checks reject third-party OAuth tokens: "This credential is only authorized for use with Claude Code" |
| 2026-02-19 | Terms add an "Authentication and credential use" section; OpenCode removes its Claude OAuth code |
| 2026-04-04 | Enforcement: subscriptions no longer cover usage through third-party tools |

pi v1.0.0 still ships the flow, labeled "Anthropic (Claude Pro/Max)" with `isSubscription: true` (`pi/packages/ai/src/providers/anthropic.ts:81-84`).
Using it violates the terms and risks an account ban.
API-key use remains allowed in every tool at pay-per-use rates, which coverage puts at 5 to 10 times subscription cost for heavy agentic use.

Consequences:

- Any option that moves Claude traffic off Claude Code loses the subscription.
- Claude Code is the only harness in which the subscription and llmenv's hooks coexist.
- Multi-channel access for subscription Claude must go through Anthropic's own surfaces: Remote Control, `--channels`, Claude Tag, cloud sessions.

## 6. Model routing options

Goal 5 asks for per-task selection among a Claude subscription, local Qwen through llama.cpp, and DeepSeek.
No single harness does all three in one session.

### What Claude Code can do

- `ANTHROPIC_BASE_URL` points the **whole session** at any server that speaks `/v1/messages`.
  llama.cpp has done so since 2026-01-19, and vLLM and Ollama do too.
  Background Haiku calls go to the same server.
- A session on the subscription cannot route a single request elsewhere.
  Subscription OAuth traffic cannot pass through a routing proxy.
- Within Anthropic models, a subagent definition's `model:` field and `CLAUDE_CODE_SUBAGENT_MODEL` select per subagent.
- A Claude session can delegate a step to another process.
  A skill script can run `pi -p --model <provider/model> "<task>"` and return only the result.
  The local model's tokens never enter Claude's context.
  This is the same idea as pi's codemode, applied across engines.

### What pi can do

- DeepSeek is a first-class provider (`pi/packages/coding-agent/docs/providers.md:36`).
- The llama.cpp router is built in: `/llama` manages the router, `/model` picks a loaded model (`pi/packages/coding-agent/docs/models.md:39`).
  Ollama, LM Studio, vLLM, and SGLang go through `models.json`.
- An extension can call `setModel` at any point and can react to `model_select`.
  A routing extension can pick the model from task metadata deterministically.
- A subagent can carry its own model.
- Claude is available only by API key.

### Three routing granularities

| Granularity | Where the decision lives | Engines |
| --- | --- | --- |
| Session launch | llmenv config: project tags + task kind map to engine + model | Claude Code (subscription) or pi (local, DeepSeek, API Claude) |
| Subagent definition | agent file `model:` field | both, Anthropic-only inside Claude Code |
| Delegated step | a skill script that runs `pi -p --model ...` | Claude Code calls pi |

The first row is the deterministic one.
llmenv already models `model_providers` and `default_models` as capabilities (`docs/design/engine-capabilities.md`), rendered for crush and opencode.
A pi adapter would render them into pi's `models.json` and `settings.json`.

## 7. Multi-channel options

| Path | What it gives | Limits |
| --- | --- | --- |
| Claude Remote Control | Drive a local Claude Code session from claude.ai web, mobile, or desktop | Anthropic-run; Claude models only; one session per terminal |
| Claude `--channels` plugins | Plugin-provided channels post into a session; gated by `channelsEnabled` and `allowedChannelPlugins` | Plugin must exist for the channel; Anthropic-run transport |
| Claude Tag | Claude in Slack with cloud sessions | Not a local llmenv session |
| `pi experimental server` | Unix-socket server routing clients to durable sessions | Experimental, no peer auth, no web UI (`pi/packages/protocol/README.md:37`) |
| pi-durable clients | Any number of clients attach, see a snapshot, then deltas; steer and follow-up queues | No channel adapters shipped; every Slack, Telegram, or web bridge is new code |

pi-durable has the right primitives for multi-channel and the wrong model access.
Claude Code has the model access and the channels, and the channels are closed.
No option gives both today.

## 8. Options against the goals

### Option A: move to pi-durable

Build the llmenv harness on pi-durable: extensions for hooks, ICM over MCP, channel adapters, a routing layer.

| Goal | Effect |
| --- | --- |
| 1 Determinism | Strong. Checkpoints, exactly-once, replay flags, task hooks |
| 2 Tokens | Strong. Compaction, output limits, originals kept |
| 3 Multi-project | Good. Per-conversation agent config and env; one process per storage |
| 4 Over time | Neutral. ICM still does this over MCP |
| 5 Models | Loses the Claude subscription. All Claude at API rates |

Pros:

- Multi-client attach, steer, and fork are built and tested.
- Crash resume covers the model request, tool calls, and compaction.
- One harness can run conversations with different models and tool sets.

Cons:

- Claude only by API key. Goal 5 fails on the most used model.
- The durable coding agent is a prototype with no extensions, no session picker, and no tests.
- No channel adapter exists. Multi-channel still has to be built.
- API is experimental and "changes without notice between releases".
- llmenv's ~93k lines of Rust hooks, scopes, task tracker, and adapters would be rewritten as TypeScript extensions.
- One process owns a storage. Several projects need several processes or one process with many conversations.

### Option B: add pi as the engine that replaces Claude

Write a pi adapter for llmenv and stop using Claude Code.

| Goal | Effect |
| --- | --- |
| 1 Determinism | Strong. In-process block, rewrite, context control |
| 2 Tokens | Strong. Compaction, codemode, `context` rewrite |
| 3 Multi-project | Same as today; llmenv scopes still apply |
| 4 Over time | Neutral |
| 5 Models | Local and DeepSeek first-class. Claude subscription lost |

Pros:

- The adapter pattern exists: the opencode shim already bridges JS plugin events to `llmenv hook-run`.
- pi reads `AGENTS.md`, `CLAUDE.md`, Agent Skills, and `mcp.json`, so bundles carry over.
- The superpowers plugin already ships a pi reference file.
- RPC and SDK modes make pi scriptable from llmenv.

Cons:

- Claude subscription lost. Same failure as A on goal 5.
- pi 1.0 does not solve multi-channel either. The server is experimental.
- Claude Code-only plugins and Remote Control are gone.
- pi does not prompt per tool. llmenv would need a guard extension to match current safety.

### Option C: port pi-durable concepts into llmenv + Claude

Keep Claude Code. Adopt the mechanics, not the runtime.

| Goal | Effect |
| --- | --- |
| 1 Determinism | Better. Checkpointed background work, idempotent recorders |
| 2 Tokens | Unchanged |
| 3 Multi-project | Unchanged |
| 4 Over time | Better. Background work stops failing silently |
| 5 Models | Unchanged. Local and DeepSeek only session-wide through `ANTHROPIC_BASE_URL` |

Pros:

- No subscription loss. No rewrite.
- Fixes the failure class behind #2355 with a proven pattern.

Cons:

- Does nothing for goal 5's per-task routing.
- Does nothing for multi-channel beyond what Claude already offers.

### Option D: hybrid, recommended

Keep Claude Code as the subscription engine.
Add pi as a fourth llmenv engine for local Qwen, DeepSeek, and API-key Claude.
Route at session launch from llmenv config, and let Claude delegate steps to pi through a skill script.
Port three pi-durable mechanics into llmenv.
Use Claude's own surfaces for multi-channel now, and re-evaluate pi-durable against watch criteria.

| Goal | Effect |
| --- | --- |
| 1 Determinism | Better. Same hooks on both engines; pi gains in-process block and rewrite; background work checkpointed |
| 2 Tokens | Better. Cheap or local work leaves Claude's context; pi compaction and codemode on the pi side |
| 3 Multi-project | Unchanged scopes; per-project engine and model choice |
| 4 Over time | Unchanged ICM; pi sessions feed the same memory |
| 5 Models | All three model classes, routed deterministically at launch and at delegated steps |

Pros:

- Keeps the subscription and every Claude Code feature.
- Reuses the adapter trait and the opencode shim pattern.
- Routing lives in config, not in model judgment.
- Each piece is independently shippable.

Cons:

- Two engines to keep in sync. The adapter trait already bears this cost for crush and opencode.
- Mid-session routing inside a Claude session stays Anthropic-only.
- Multi-channel for non-Claude sessions stays open until pi-durable or a pi channel matures.

## 9. Recommendation and watch criteria

Choose option D.

Order of work:

1. pi adapter in llmenv: `AGENTS.md`, skills, `mcp.json`, `settings.json`, `models.json`, and a `pi` extension shim that bridges events to `llmenv hook-run`, following `src/adapter/opencode.rs`.
2. Engine and model routing in config: a map from project tags and task kind to engine and model, rendered per adapter.
3. A delegation skill: a script that runs `pi -p --model <x>` and returns the result, for use from Claude sessions.
4. Durable background work in llmenv: checkpoint and resume for consolidation, index, and session-log record.
5. `requestId` idempotency on detached recorders and ICM stores.
6. A per-session agent-config document under `<state>/` with engine, model, thinking level, and cwd.

Re-evaluate pi-durable when all of these hold:

- The durable coding agent supports extensions and a session picker.
- A channel adapter ships in the repo, or a pi channel is otherwise usable without new transport code.
- The API drops the experimental label.
- Claude access terms change, or API-rate Claude becomes acceptable for the channel use case.

## 10. Candidate follow-up issues

File these only if the recommendation stands.

| Title | Milestone hint | Labels |
| --- | --- | --- |
| feat(adapter): pi engine adapter with hook-run shim | v4.1.0 | area:adapter, area:hook, type:feature, size/L |
| feat(config): engine and model routing by project tag and task kind | v4.1.0 | area:config, area:adapter, type:feature, size/M |
| feat(skills): delegate a step to `pi -p` with a configured model | v4.1.0 | area:adapter, type:feature, size/S |
| fix(hook): checkpoint and resume detached background work | v3.12.0 | area:hook, bug, size/M |
| feat(hook): requestId idempotency for detached recorders | v3.12.0 | area:hook, enhancement, size/S |
| feat(task): per-session agent-config document in state dir | v4.1.0 | area:task, area:config, type:feature, size/S |

## Sources

- pi-durable post: <https://earendil.com/posts/pi-durable/>
- pi repository: <https://github.com/earendil-works/pi>
- llama.cpp Anthropic Messages API: <https://huggingface.co/blog/ggml-org/anthropic-messages-api-in-llamacpp>
- vLLM Claude Code integration: <https://docs.vllm.ai/en/stable/serving/integrations/claude_code/>
- Anthropic subscription OAuth ban, 2026-02-19: <https://winbuzzer.com/2026/02/19/anthropic-bans-claude-subscription-oauth-in-third-party-apps-xcxwbn/>
- OpenCode blocked, timeline: <https://www.zbuild.io/resources/news/opencode-blocked-anthropic-2026>
- Enforcement from 2026-04-04: <https://natural20.com/coverage/anthropic-banned-openclaw-oauth-claude-code-third-party>
- Background Haiku calls under `ANTHROPIC_BASE_URL`: <https://dev.to/mfolsom/the-ghost-in-the-cli-why-claude-code-kills-local-inference-dfc>
