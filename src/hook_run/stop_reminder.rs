//! The `Stop` reminder: the task tracker text, the slippage critique, and the repeat check.

use super::stop_dedupe;

/// The task-tracker's `Stop` reminder, with repeat-detection applied: once
/// the identical reminder has fired `threshold` times in a row for this
/// session, `repeat_detect::handle_stop` appends a pointer to `llmenv task
/// wait` (see #1006 — this is the same on-by-default detector as
/// `resolve_pre_tool_text`, applied to Stop-event nagging instead of
/// `PreToolUse` tool-call repetition).
pub(super) fn resolve_stop_reminder(
    state_dir: &std::path::Path,
    claude_session_id: Option<&str>,
    config: &crate::config::Config,
    stop_payload: &serde_json::Value,
) -> String {
    // #2511: a stop that a Stop hook caused gets no reminder, so the hook cannot loop the agent.
    if stop_dedupe::is_hook_continuation(stop_payload) {
        return String::new();
    }
    let mut reminder = crate::task::stop_hook_reminder(state_dir, claude_session_id);
    // #2456: a turn that ends with a question to the user parks the task that waits for it.
    if let Some(tracker) = config
        .features
        .as_ref()
        .and_then(|f| f.task_tracker.as_ref())
    {
        let waiting = crate::hook_run::task_nudge::handle_stop(
            tracker,
            stop_payload,
            claude_session_id,
            state_dir,
        );
        if !waiting.is_empty() {
            if !reminder.is_empty() {
                reminder.push_str("\n\n");
            }
            reminder.push_str(&waiting);
        }
    }
    // #317: appended before repeat-detect wraps the text, so a repeat warning
    // stays the last thing read — it's about the turn that just happened,
    // while the checklist is about what to do before ending.
    let critique = crate::hook_run::slippage::handle_stop(
        config.features.as_ref().and_then(|f| f.slippage.as_ref()),
    );
    if !critique.is_empty() {
        if !reminder.is_empty() {
            reminder.push_str("\n\n");
        }
        reminder.push_str(&critique);
    }
    // #2511: runs before repeat-detect, which counts only the reminders that are emitted.
    let reminder = stop_dedupe::emit_once(state_dir, claude_session_id, &reminder);
    let repeat_detect_cfg = config
        .features
        .as_ref()
        .and_then(|f| f.repeat_detect.clone())
        .unwrap_or_default();
    if repeat_detect_cfg.enabled {
        crate::hook_run::repeat_detect::handle_stop(
            &reminder,
            claude_session_id,
            &repeat_detect_cfg,
            state_dir,
        )
    } else {
        reminder
    }
}
