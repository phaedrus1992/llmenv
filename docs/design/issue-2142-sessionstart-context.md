# Issue #2142 — deliver ICM wake-up context on SessionStart

- **Issue:** https://github.com/phaedrus1992/llmenv/issues/2142
- **Milestone:** `v3.12.0`
- **Base branch:** `release/3.x` (forward-merges to `release/4.x`)
- **Type:** bug fix with a behavior change
- **Depends on:** #2159 (uses its `split_recall_records` and `RECALL_BUDGET_BYTES`); see `issue-2159-2141-icm-recall-prioritization.md`

This is a spec, not a plan.

## Problem

On `SessionStart`, `hook-run` runs `Action::WakeUp` (`icm_wake_up`), which returns a summary of the most relevant memories.
Then `emit_hook_context` returns an empty string for `SessionStart`, so the summary is thrown away.
The model never sees the wake-up output, and every session pays for an ICM call that does nothing.

The suppression came from #558.
#558 fixed a real bug for `SessionEnd`: Claude Code rejects `additionalContext` on `SessionEnd`.
#558 listed `SessionStart` only as "verify", and nobody verified it.

Claude Code accepts `hookSpecificOutput.additionalContext` on `SessionStart`.
llmenv itself relies on this: the `llmenv config-context` SessionStart hook emits exactly that shape, and its text reaches the model.
Claude Code 2.1.277 also fixed the one known defect: a session continued after `/clear` lost part of its first message and missed the prompt cache when a `SessionStart` hook printed output.

## Verified facts (release/3.x)

| Fact | Location |
| --- | --- |
| `dispatch(SessionStart, …)` returns `vec![Action::WakeUp(wakeup_max_tokens)]` | `src/hook_run/mod.rs` near line 235 |
| Shared emitter returns `""` for `SessionStart` and `SessionEnd` | `emit_hook_context`, `src/adapter/mod.rs` near line 537 |
| Claude Code, Crush and opencode adapters all call the shared emitter | `src/adapter/claude_code.rs` near line 544, `crush.rs` near line 483, `opencode.rs` near line 1221 |
| `llmenv config-context` emits SessionStart `additionalContext` and works | `Command::ConfigContext`, `src/cli/mod.rs` near line 256; hook registered in `src/adapter/claude_code.rs` near line 1297 |
| `run()` writes whatever the adapter emitter returns to stdout | `src/hook_run/mod.rs` near line 523 |
| 3.x has no Codex adapter | `src/adapter/` |

## Decisions

1. **Claude Code only.** Only the Claude Code adapter emits `SessionStart` context.
   Crush and opencode keep today's behavior (no output on `SessionStart`), because nothing shows their hook schemas accept it.
   Opening them up needs its own issue with evidence.
2. **`SessionEnd` stays suppressed for every adapter.** That is the #558 fix and it is correct.
3. **Header.** The wrapper line for `SessionStart` is `[ICM MEMORY CONTEXT (session start)]`.
   `TurnStart` keeps `[ICM MEMORY CONTEXT (auto-injected)]`.
   The model can then tell the one-time summary from per-prompt recall.
4. **Size.** Wake-up text goes through `split_recall_records` and the same `RECALL_BUDGET_BYTES` (8,000) budget as #2159, with the same omission line.
   `SessionStart` output is subject to the same inline limit in Claude Code.
5. **`source` handling.** Claude Code's `SessionStart` payload carries `source`: `startup`, `resume`, `clear`, `compact` or `fork` (hooks reference, fetched 2026-09-26).
   - `startup`, `clear`, `compact`, missing, or any other value: run `WakeUp` and emit.
   - `resume` or `fork`: do not run `WakeUp` at all. Both carry the earlier conversation, which already holds the earlier injection, and re-sending it doubles the context.
6. **Nothing moves out of `TurnStart` in this issue.** The scope chunk is only `Store` content at `SessionEnd`, not per-prompt injection, so there is nothing else to move.
   Per-prompt recall stays per-prompt; #2141 makes it specific to the prompt.

## Design

### Emitter

Change the shared function signature in `src/adapter/mod.rs`:

```rust
/// Which lifecycle events an engine accepts `additionalContext` for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SessionStartContext {
    Accepted,
    Rejected,
}

pub(crate) fn emit_hook_context(
    hook_event_name: &str,
    text: &str,
    session_start: SessionStartContext,
) -> String
```

Rules, in order:

1. Whitespace-only `text` returns `""` (unchanged, #978).
2. `SessionEnd` returns `""` (unchanged, #558).
3. `SessionStart` with `Rejected` returns `""`.
4. `SessionStart` with `Accepted` wraps with the session-start header.
5. Every other event wraps with the existing auto-injected header.

The trait method `AgentAdapter::emit_hook_context(&self, hook_event_name, text)` keeps its signature.
`ClaudeCodeAdapter` passes `Accepted`.
`CrushAdapter` and `OpenCodeAdapter` pass `Rejected`.
Update the comment on the shared function: remove the claim that all adapters reject `SessionStart` context, and cite this issue.

### Dispatch

`dispatch` gains a `bool` argument named `continued`, true when `source` is `resume` or `fork`.
`SessionStart` with `continued == true` returns `vec![]`.
Read `source` from `stdin_payload["source"]` in `run_inner`.
`dispatch` has 4 positional parameters today; adding one makes 5, which is the project limit, so do not add more.

### Budget

Apply the #2159 record split and budget to the wake-up text before it reaches the emitter.
Reuse the same function #2159 adds; do not write a second budget loop.

## Tests

1. Emitter table test: every combination of event (`SessionStart`, `SessionEnd`, `UserPromptSubmit`) and `SessionStartContext` gives the expected output or `""`.
2. Claude Code adapter test: `emit_hook_context("SessionStart", "x")` returns JSON whose `hookSpecificOutput.hookEventName` is `SessionStart` and whose `additionalContext` starts with `[ICM MEMORY CONTEXT (session start)]`.
3. Crush and opencode adapter tests: `SessionStart` returns `""`.
4. `dispatch` test: `SessionStart` with `continued == true` returns no actions; with `false` returns `WakeUp`.
5. `source` parsing test: `"resume"` and `"fork"` set `continued`; `"startup"`, `"clear"`, `"compact"`, missing and non-string all do not.
6. Budget test: a wake-up text larger than the budget is cut at whole records with the omission line.

## Acceptance criteria

1. A new Claude Code session with ICM configured shows the `[ICM MEMORY CONTEXT (session start)]` block in context.
2. `claude --resume` and a forked session do not add a second wake-up block, and the hook makes no `icm_wake_up` call (check with `LLMENV_TRACE_TIMING=1`).
3. `SessionEnd` still produces no hook output and no Claude Code validation error.
4. Changelog entry under `Fixed`: wake-up memories now reach the model at session start.
5. The memory docs page describes the session-start block, tagged `(changed in v3.12.0)`.

## Out of scope

- Crush and opencode `SessionStart` context.
- Moving per-prompt recall to `SessionStart`.
- Changing what `icm_wake_up` returns.
