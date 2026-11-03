//! The ledger writes of the adaptive recall that make no MCP call.

use serde_json::Value;

use super::{ACTIVITY_UNSAVED, note_unless_saved};
use crate::hook_run::HookEvent;
use crate::hook_run::relevance;
use crate::hook_run::session_ledger::LedgerStore;
use crate::hook_run::session_state::unix_now;

/// Local ledger writes with no MCP call: batch activity and queued subagent tasks.
pub(in crate::hook_run) fn record_local(
    event: HookEvent,
    store: &LedgerStore,
    session_id: &str,
    payload: &Value,
) {
    let now = unix_now();
    let t0 = std::time::Instant::now();
    match event {
        HookEvent::PostToolBatch => {
            let empty = Vec::new();
            let calls = payload["tool_calls"].as_array().unwrap_or(&empty);
            note_unless_saved(
                store.update(session_id, |l| {
                    for call in calls {
                        let tool = call["tool_name"].as_str().unwrap_or_default();
                        if tool == "Agent"
                            && let Some(id) = call["tool_use_id"].as_str()
                        {
                            l.drop_subagent(id);
                        }
                        l.push_activity(relevance::activity_from_tool_call(
                            tool,
                            &call["tool_input"],
                            now,
                        ));
                    }
                }),
                ACTIVITY_UNSAVED,
            );
        }
        HookEvent::SubagentTask if payload["tool_name"].as_str() == Some("Agent") => {
            let input = &payload["tool_input"];
            let kind = input["subagent_type"].as_str().unwrap_or("general-purpose");
            // The id keys the queue, so a missing one would merge two subagents into one entry.
            let (Some(id), Some(prompt)) = (
                payload["tool_use_id"].as_str().filter(|id| !id.is_empty()),
                input["prompt"].as_str().filter(|p| !p.is_empty()),
            ) else {
                crate::agent_notice::record_logged(format!(
                    "llmenv: subagent task not queued for session {session_id}: the Agent call has no string tool_use_id or tool_input.prompt, so the subagent gets no task memories. Fix: send the Agent call with a string tool_use_id and tool_input.prompt."
                ));
                return;
            };
            note_unless_saved(
                store.update(session_id, |l| l.queue_subagent(id, kind, prompt, now)),
                ACTIVITY_UNSAVED,
            );
        }
        _ => {}
    }
    let elapsed = t0.elapsed();
    // Only these two events update the ledger. The time includes a busy-lock wait.
    let touched = matches!(event, HookEvent::PostToolBatch | HookEvent::SubagentTask);
    if touched {
        crate::trace::emit_ledger_trace("update", elapsed);
    }
}
