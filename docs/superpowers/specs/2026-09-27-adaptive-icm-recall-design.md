# Adaptive ICM recall: session dedup, activity relevance, related topics

Issues: #2249, #2251.
Milestone: v3.12.0.
Base branch: `release/3.x`.

## Problem

The per-turn ICM memory block is identical on every turn.
The `TurnStart` recall (wired to Claude Code `UserPromptSubmit`) builds its inputs from the active scope tags only.

- `run_inner` sets `query = tags.join(", ")` (`src/hook_run/mod.rs:1104`).
- `Action::arguments` sends one `RecallTag` call per tag, one `RecallBundle` call per bundle, and one final `Recall` call with the joined tags as the query (`src/hook_run/action.rs:113-127`).
- The prompt text never reaches a recall query.
  `event_content` reads `prompt` only for the session-log handler (`src/hook_run/mod.rs:316-318`).
- No state survives between turns.
  `RecallBudget.seen` (`src/hook_run/recall.rs:66`) removes duplicates inside one turn only.

The same tags give the same records on every turn.
The block uses up to 8,000 bytes per turn and carries no signal about the work in progress.

A second defect (#2251): `emit_hook_context` returns an empty string for `SessionStart` (`src/adapter/mod.rs:548`), and the Claude Code adapter delegates to it (`src/adapter/claude_code.rs:552`).
Every session start runs `icm_wake_up` and then discards the result.
The suppression cites #558, which was about `SessionEnd`.
The Claude Code hooks reference says `SessionStart` accepts `hookSpecificOutput.additionalContext`.

## Goals

1. Send a memory once per model context, not once per turn.
2. Pick per-turn memories from what the session does: the prompt, the tools used, the files touched, the errors seen, and the model's own recent text.
3. Add memories from related topics, with the ICM tools that exist today.
4. Inject error-relevant memories at the moment a tool fails.
5. Give a subagent memories that match its task.
6. Deliver the `SessionStart` context to the model in Claude Code (#2251).

## Non-goals

- Changes to the ICM server.
  ICM can be a remote, shared `icm serve`, so per-agent session state stays in llmenv.
- Wiring for the opencode and crush adapters.
- Configurable ring sizes, byte budgets, or wave limits.
  These stay constants, the same as the current recall budget.

## Decisions

| Question | Decision |
| --- | --- |
| The scope-tagged set | Send it once per epoch from `SessionStart`, with the wake-up pack. `TurnStart` uses its budget for relevance. |
| Relevance signals | Prompt text, tool activity, errors, and the transcript tail. |
| Related topics | Topic fanout and keyword fanout with `icm_memory_recall`. |
| Mid-turn injection | On tool failure only. |
| Architecture | A session ledger in llmenv, fed by hooks, plus a transcript-tail read. |
| Subagents | In scope, with a separate `sent` set per agent. |

An epoch is the span of one model context: from a session start, a `/clear`, or a compaction, to the next of these.

## Components

### `src/hook_run/session_ledger.rs` (new)

The ledger owns one JSON file per session at `state_dir()/recall_session/{session_id}.json`.
It copies the safety rules of `read_once` (`src/hook_run/read_once.rs:35-108`):

- The `session_id` comes from unsanitized stdin, so the ledger checks it with `paths::is_valid_short_name` before it builds a path.
- Writes are atomic: a write to a temporary file, then a rename.
- Stale files are removed with `prune_stale_json_files` from `src/hook_run/session_state.rs`.

Fields:

| Field | Type | Meaning |
| --- | --- | --- |
| `epoch` | integer | Goes up by one on each reset. |
| `agents` | map of agent key to agent state | One entry per context: `main` for the parent, the `agent_id` for a subagent. |
| `agents[k].sent` | set of record hashes | Records injected into context `k` in this epoch. |
| `agents[k].scope_sent` | bool | The scope-tagged set went out to context `k` in this epoch. |
| `activity` | ring, 20 entries | Tool name, file path or command name, and a timestamp. |
| `errors` | ring, 5 entries | Tool name and the first 300 bytes of the error text, with a timestamp. |
| `last_query_hash` | hash | The hash of the previous `TurnStart` relevance query. |
| `last_turn_at` | timestamp | The time of the previous `TurnStart`. |
| `pending_subagents` | queue, 8 entries | `subagent_type`, the first 600 characters of the task `prompt`, and a timestamp, from each `Agent` tool call. Entries older than 5 minutes are dropped. |

A record hash covers the record topic plus its normalized text (whitespace collapsed).
ICM recall output carries no record id, so the content is the only stable key.

Subagent hooks share the parent `session_id`, and carry the subagent `agent_id` as a common input field.
The `sent` set is keyed per agent for that reason.
Without the key, a record injected into a subagent is marked as sent for the parent, which never saw it.

Concurrency: parallel tool calls can fire several hooks at the same time.
Each read-modify-write holds a lock file next to the ledger.
A lock that is not free within 200 ms skips the write and logs a trace line.
The hook never blocks the agent on the lock.

### `src/hook_run/relevance.rs` (new)

Pure functions with no I/O.
Inputs: the prompt, the ledger rings, and the transcript tail.
Outputs: the relevance query, the fanout keywords, and the sibling topics for a set of hit topics.

Query construction, in priority order:

1. The prompt, capped at 500 characters.
2. Terms from the last 10 `activity` entries: distinct file stems, directory or crate names (for example `hook_run`, `llmenv-config`), and command names (for example `cargo`, `gh`, `git`).
3. The head of the newest `errors` entry, if its timestamp is after `last_turn_at`.
4. The first 300 characters of the last assistant text message in the transcript.

The total query is capped at 600 characters.

Fanout keywords: up to 2 activity terms, the most recent first.

Sibling topics: from a hit topic of the canonical form `context-X` or `decisions-X`, derive the other form plus `errors-resolved`.
Topics already present in the hits are not repeated.
A topic that does not match a canonical form gives no siblings.

### `src/hook_run/recall.rs` (changed)

- `RecallBudget` takes the `sent` set of the current context as a filter before the byte budget.
- `RecallBudget` returns the hashes of the records it kept, so that the caller records them in the ledger.
- The trace counter `advisory_stripped` counts duplicate records (`recall.rs:122`, `recall.rs:126`).
  Rename it to say what it counts.

### `src/hook_run/action.rs` (changed)

- `Action::Recall` gets optional `topic`, `keyword`, `project`, and `limit` arguments for the relevance and fanout calls.
  No new MCP tool is needed.
- The final `Recall` call no longer sends the joined tag list as a natural-language query.
  The relevance query replaces it.

### Adapter wiring (`src/adapter/claude_code.rs`)

New registrations, each only when a memory backend is active (the same rule as `turn_start` today, `src/adapter/claude_code.rs:1563-1569`):

- `PostToolBatch`: record activity.
- `PostToolUseFailure`: record the error, then inject.
- `PreToolUse` with the matcher `Agent`: queue the subagent task text in `pending_subagents`.
  This is a local file write only: no MCP call, no output, and no permission decision.
- `SubagentStart`: inject task-relevant memories.

The existing `SessionStart` registration also handles the reset and the scope set.
No `PostCompact` registration is needed, because `SessionStart` fires after a compaction with `source: "compact"`.

`ClaudeCodeAdapter::emit_hook_context` stops delegating `SessionStart` to the shared function.
It emits the `additionalContext` envelope for `SessionStart`, and keeps `SessionEnd` suppressed (#2251).
The shared function keeps its current behavior for the other adapters.

## Data flow

### `SessionStart`

- `source` is `startup`, `clear`, or `compact`: reset the ledger.
  Increment `epoch`, and clear all `agents` entries.
  Keep `activity` and `errors`, because they still describe the session.
- `source` is `resume`: keep the ledger if it exists.
  On resume, Claude Code replays the saved hook text from the transcript, so the earlier injections are still in context.
- `source` is `fork`: the fork gets a new `session_id`, so its ledger starts empty.
  The fork keeps the parent context, so the scope set repeats one time.
  The payload does not name the parent session, so the ledger cannot copy the parent state.
- `/clear` also gives a new `session_id`, so the reset for `clear` is automatic.
  The explicit reset stays, because it costs nothing.
- After the reset decision, run the wake-up pack and the scope set:
  1. `icm_wake_up`, unchanged.
     Its text passes through outside the byte budget, the same as today.
  2. The current scope-tag recall, ranked by `recall.rs:31-60`, inside the 8,000-byte budget.
  3. Drop scope records whose hash is in `agents.main.sent`.
     On `resume` this removes the records that the replayed transcript already holds.
  4. Add the kept hashes to `agents.main.sent`, set `agents.main.scope_sent`, and save the ledger.
  5. Inject the text through `additionalContext`.

### `TurnStart` (Claude Code `UserPromptSubmit`)

1. Load the ledger.
2. If `agents.main.scope_sent` is false, run the scope-tag recall here, then set `scope_sent`.
   This is a fallback for a session where the `SessionStart` hook did not run or failed, for example when llmenv was installed mid-session.
3. Build the relevance query.
   If the query hash equals `last_query_hash` and no activity arrived after `last_turn_at`, make no relevance calls.
4. Wave 1, concurrent:
   - Main recall: the query, `limit: 10`, default project filter.
   - Keyword fanout: each fanout keyword as the `keyword` filter, `project: ""`, `limit: 3`.
   - Cross-project recall: the query, `project: ""`, `limit: 3`.
5. Wave 2, concurrent, after wave 1: topic fanout.
   At most 2 recalls, one per sibling topic, with `topic` set and `limit: 3`.
   Skip wave 2 when wave 1 took more than 1.5 seconds.
6. Merge in budget order: the scope set (step 2 fallback only), main, keyword fanout, topic fanout, cross-project.
7. Drop records whose hash is in `agents.main.sent`.
   Apply the 8,000-byte budget (`RECALL_BUDGET_BYTES`).
   The existing omission notice reports records that did not fit.
8. Add the kept hashes to `agents.main.sent`.
   Store `last_query_hash` and `last_turn_at`.
   Save the ledger.
9. If no record is left, emit nothing.
   `emit_hook_context` already skips an empty block (`src/adapter/mod.rs:540`).

### `PostToolBatch`

Append one `activity` entry per tool call.
This is a local file write only: no MCP call and no output.

### `PostToolUseFailure`

1. Append the failure to `errors`.
2. Run the error recall, concurrent:
   - The tool name plus the error head as the query, default project filter, `limit: 5`.
   - The same query with `topic: "errors-resolved"`, `project: ""`, `limit: 3`.
3. Filter against the `sent` set of the current context (`agent_id` from the hook input, else `main`).
4. Apply a 2,000-byte budget, inject, and record the kept hashes.

### `PreToolUse` on `Agent`

Append `tool_input.subagent_type` and the head of `tool_input.prompt` to `pending_subagents`.
The hook returns no output, so it never changes the permission flow.

### `SubagentStart`

The `SubagentStart` payload has `agent_id` and `agent_type`, but no task text.

1. Take the oldest `pending_subagents` entry whose `subagent_type` equals `agent_type`, and remove it from the queue.
   Parallel launches of the same agent type can swap task texts between siblings.
   All siblings come from the same parent turn, so the cost of a swap is a less exact query, not a wrong context.
2. Build a relevance query from that task text, capped at 600 characters.
   If no entry matches (for example, a resumed subagent or an agent-team message), build the query from `agent_type` plus the parent activity terms.
3. Run wave 1 and wave 2 as for `TurnStart`, with no scope-set step and no skip rule.
4. Filter against `agents[agent_id].sent`, which starts empty.
   A subagent starts with a fresh context, so the parent `sent` set does not apply.
   `SubagentStart` fires again when a subagent resumes, and the per-agent `sent` set prevents a repeat.
5. Apply a 4,000-byte budget, inject through `additionalContext`, and record the kept hashes under `agents[agent_id]`.

## Latency

`HOOK_TIMEOUT` stays at 2 seconds per MCP call (`src/hook_run/mod.rs:137`).
Today the calls run one after another, so the worst case is (tags + bundles + 1) × 2 seconds.
The new flow runs each wave concurrently on the existing current-thread runtime.
The `TurnStart` worst case is two waves, about 4 seconds.
Wave 2 is skipped after a slow wave 1, so a slow backend costs about 2 seconds.
The scope-set calls move to `SessionStart`, which already runs them in the current serial order and cost.
`PostToolBatch` and `PreToolUse` on `Agent` make no MCP call.

## Failure handling

A hook must never block or break a turn.

- A missing, corrupt, or unreadable ledger: log a trace line and continue as a fresh epoch.
  The cost is one repeat of the scope set.
- An invalid `session_id`: skip the ledger and run the current stateless recall.
- An MCP timeout or error in one call: keep the results of the other calls.
- A hook input with an `agent_id` that fails `is_valid_short_name`: treat the context as `main` for filters, and record no hashes.

## Injected text

The Claude Code docs warn that text framed as out-of-band system commands can trigger prompt-injection defenses.
The block keeps its current factual header, and adds no imperative framing around the records.

## Config

One new key: `features.memory[].adaptive_recall` (bool, default `true`).
`false` restores the current stateless recall exactly, which gives a rollback path.

## Verified hook facts

Source: the Claude Code hooks reference, `https://code.claude.com/docs/en/hooks.md`, read on 2026-09-27.

1. `PostToolBatch` input has `tool_calls`, an array of `tool_name`, `tool_input`, `tool_use_id`, and `tool_response`.
   It fires once per batch, with no matcher, and accepts `additionalContext`.
2. `PostToolUseFailure` input has `tool_name`, `tool_input`, `tool_use_id`, `error`, and the optional `is_interrupt` and `duration_ms`.
   It accepts `hookSpecificOutput.additionalContext`.
   The `error` format varies by tool, so only its first line and a byte-capped head are used.
3. `SubagentStart` input has `agent_id` and `agent_type` only.
   It accepts `additionalContext`, which goes to the subagent context before its first prompt.
4. Hook input fired inside a subagent carries `agent_id` and `agent_type` as common input fields.
5. `SessionStart` `source` is one of `startup`, `resume`, `clear`, `compact`, or `fork`.
   `/clear` gives a new `session_id`.
6. The `Agent` tool input has `subagent_type` and `prompt`.

## Testing

- Unit and property tests for `relevance.rs`: the query caps, term extraction, sibling-topic derivation, and the same output for the same input.
- Ledger tests: the reset per `source`, hash dedup per agent key, the TTL prune, a corrupt file, an invalid id, and two concurrent writers with no lost update.
- Integration tests with the existing mock MCP server:
  - `SessionStart` sends the wake-up pack and the scope set, and the next `TurnStart` does not repeat a scope record.
  - A `compact` `SessionStart` resets the ledger and sends the scope set again.
  - A `resume` `SessionStart` keeps the ledger and sends no scope record already in `sent`.
  - A `TurnStart` with `scope_sent` false (no `SessionStart` ran) sends the scope set as a fallback.
  - A change of prompt changes the relevance calls.
  - The skip rule makes no calls for a repeated query with no new activity.
  - A tool failure injects error memories, and a later turn does not repeat them.
  - A subagent gets memories that the parent already saw, and the parent `sent` set does not change.
  - A `PreToolUse` on `Agent` queues the task text, and the matching `SubagentStart` uses it for the query.
  - A `SubagentStart` with no queued entry falls back to `agent_type` plus the activity terms.
  - `adaptive_recall: false` gives the current output byte for byte.
- Adapter tests: the new hooks register only when a memory backend is active.
- Adapter tests (#2251): the Claude Code adapter emits the `additionalContext` envelope for `SessionStart`, and still emits nothing for `SessionEnd`.
  The shared `emit_hook_context` keeps its `SessionStart` suppression for the other adapters.

## Docs

- A `website/docs/` section for adaptive recall, tagged `(added in v3.12.0)`, with the config key.
- A changelog entry under `[Unreleased]`.
- An update to `docs/design/issue-2159-2141-icm-recall-prioritization.md`, so that it describes the new flow.

## Follow-up

- Upstream ICM: expose record ids and links in `icm_memory_recall` output.
  This allows graph-based related-topic expansion and exact dedup by id.
