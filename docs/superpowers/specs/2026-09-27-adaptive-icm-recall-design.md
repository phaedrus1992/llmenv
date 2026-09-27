# Adaptive ICM recall: session dedup, activity relevance, related topics

Issue: #2249.
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

## Goals

1. Send a memory once per model context, not once per turn.
2. Pick per-turn memories from what the session does: the prompt, the tools used, the files touched, the errors seen, and the model's own recent text.
3. Add memories from related topics, with the ICM tools that exist today.
4. Inject error-relevant memories at the moment a tool fails.
5. Give a subagent memories that match its task.

## Non-goals

- Changes to the ICM server.
  ICM can be a remote, shared `icm serve`, so per-agent session state stays in llmenv.
- Wiring for the opencode and crush adapters.
- Configurable ring sizes, byte budgets, or wave limits.
  These stay constants, the same as the current recall budget.

## Decisions

| Question | Decision |
| --- | --- |
| The scope-tagged set | Send it once per epoch, then use the per-turn budget for relevance. |
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

A record hash covers the record topic plus its normalized text (whitespace collapsed).
ICM recall output carries no record id, so the content is the only stable key.

Subagent hooks share the parent `session_id`.
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
- `SubagentStart`: inject task-relevant memories.

The existing `SessionStart` registration also handles the reset.
No `PostCompact` registration is needed, because `SessionStart` fires after a compaction with `source: "compact"`.

## Data flow

### `SessionStart`

- `source` is `startup`, `clear`, or `compact`: reset the ledger.
  Increment `epoch`, and clear all `agents` entries.
  Keep `activity` and `errors`, because they still describe the session.
- `source` is `resume`: keep the ledger if it exists.
  The resumed transcript still holds the earlier injections.
- The existing `icm_wake_up` call stays.
  Any `[topic]` records in its output go into `agents.main.sent`.

### `TurnStart` (Claude Code `UserPromptSubmit`)

1. Load the ledger.
2. If `agents.main.scope_sent` is false, run the current scope-tag recall (ranked by `recall.rs:31-60`), then set `scope_sent`.
3. Build the relevance query.
   If the query hash equals `last_query_hash` and no activity arrived after `last_turn_at`, make no relevance calls.
4. Wave 1, concurrent:
   - Main recall: the query, `limit: 10`, default project filter.
   - Keyword fanout: each fanout keyword as the `keyword` filter, `project: ""`, `limit: 3`.
   - Cross-project recall: the query, `project: ""`, `limit: 3`.
5. Wave 2, concurrent, after wave 1: topic fanout.
   At most 2 recalls, one per sibling topic, with `topic` set and `limit: 3`.
   Skip wave 2 when wave 1 took more than 1.5 seconds.
6. Merge in budget order: the scope set (step 2 only), main, keyword fanout, topic fanout, cross-project.
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

### `SubagentStart`

1. Build a relevance query from the subagent task text, capped at 600 characters.
2. Run wave 1 and wave 2 as for `TurnStart`, with no scope-set step and no skip rule.
3. Filter against `agents[agent_id].sent`, which starts empty.
   A subagent starts with a fresh context, so the parent `sent` set does not apply.
4. Apply a 4,000-byte budget, inject through `additionalContext`, and record the kept hashes under `agents[agent_id]`.

## Latency

`HOOK_TIMEOUT` stays at 2 seconds per MCP call (`src/hook_run/mod.rs:137`).
Today the calls run one after another, so the worst case is (tags + bundles + 1) × 2 seconds.
The new flow runs each wave concurrently on the existing current-thread runtime.
The worst case is about 4 seconds on a turn that also sends the scope set, and about 2 seconds after that.
`PostToolBatch` makes no MCP call.

## Failure handling

A hook must never block or break a turn.

- A missing, corrupt, or unreadable ledger: log a trace line and continue as a fresh epoch.
  The cost is one repeat of the scope set.
- An invalid `session_id`: skip the ledger and run the current stateless recall.
- An MCP timeout or error in one call: keep the results of the other calls.
- Claude Code does not put `agent_id` in hook input (fact 4 below is false):
  - `SubagentStart` still injects, and records its hashes under a key made from the `SubagentStart` payload.
    That key is never read again, so the parent `sent` set does not change.
  - `PostToolUseFailure` cannot tell a subagent from the parent.
    It filters against `agents.main.sent`, injects, and records no hashes.
    A later turn can repeat one of those records, but no record is marked as sent to a context that did not see it.

## Config

One new key: `features.memory[].adaptive_recall` (bool, default `true`).
`false` restores the current stateless recall exactly, which gives a rollback path.

## Facts to verify before code

The plan must confirm these against Claude Code before it writes code that depends on them:

1. The `PostToolBatch` payload shape.
   If it lacks per-tool inputs, record activity from `PostToolUse` instead, with the same local-write-only rule.
2. Whether `PostToolUseFailure` accepts `hookSpecificOutput.additionalContext`.
3. Whether the `SubagentStart` payload carries the subagent task text.
4. Whether hook input fired inside a subagent carries `agent_id`.
5. The `source` values of `SessionStart`: `startup`, `resume`, `clear`, `compact`.

## Testing

- Unit and property tests for `relevance.rs`: the query caps, term extraction, sibling-topic derivation, and the same output for the same input.
- Ledger tests: the reset per `source`, hash dedup per agent key, the TTL prune, a corrupt file, an invalid id, and two concurrent writers with no lost update.
- Integration tests with the existing mock MCP server:
  - Turn 1 sends the scope set, and turn 2 does not repeat it.
  - A `compact` reset sends the scope set again.
  - A change of prompt changes the relevance calls.
  - The skip rule makes no calls for a repeated query with no new activity.
  - A tool failure injects error memories, and a later turn does not repeat them.
  - A subagent gets memories that the parent already saw, and the parent `sent` set does not change.
  - `adaptive_recall: false` gives the current output byte for byte.
- Adapter tests: the new hooks register only when a memory backend is active.

## Docs

- A `website/docs/` section for adaptive recall, tagged `(added in v3.12.0)`, with the config key.
- A changelog entry under `[Unreleased]`.
- An update to `docs/design/issue-2159-2141-icm-recall-prioritization.md`, so that it describes the new flow.

## Follow-up

- Upstream ICM: expose record ids and links in `icm_memory_recall` output.
  This allows graph-based related-topic expansion and exact dedup by id.
