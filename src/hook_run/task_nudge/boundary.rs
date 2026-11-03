//! The reminder at a step boundary. A push, or a pull request opened, marked ready, or merged,
//! ends a step of the agent's work. When a task is in progress and tasks are queued behind it,
//! the step needs its `done` and the next `start` (#3318).

use super::{NudgeState, start_next};
use crate::hook_run::shell_split::{nesting_reason, step_boundary};
use crate::task::Tracking;

/// The reminder after a shell command that reaches a step boundary, or an empty string. It fires
/// once for each task in progress and boundary kind, so a repeated push stays quiet.
pub(super) fn boundary_reminder(
    payload: &serde_json::Value,
    tracking: &Tracking,
    state: &mut NudgeState,
) -> String {
    let command = payload["tool_input"]["command"]
        .as_str()
        .unwrap_or_default();
    let event = match step_boundary(command) {
        Ok(Some(event)) => event,
        Ok(None) => return String::new(),
        Err(_) => {
            crate::agent_notice::record_logged(format!(
                "llmenv task tracker: the step reminder did not run. {}",
                nesting_reason()
            ));
            return String::new();
        }
    };
    if let Tracking::Unknown(cause) = tracking {
        crate::agent_notice::record_logged(format!(
            "llmenv task tracker: the step reminder did not run, because the task store cannot be \
             read ({cause}). Fix: run `llmenv doctor` to check the state directory."
        ));
        return String::new();
    }
    let Tracking::Tracked {
        wip: Some(slug),
        queued,
        next,
        ..
    } = tracking
    else {
        return String::new();
    };
    if *queued == 0 {
        return String::new();
    }
    let key = format!("{slug} {event}");
    if state.boundary_reminded.contains(&key) {
        return String::new();
    }
    state.boundary_reminded.push(key);
    let start = next
        .as_deref()
        .map_or_else(String::new, |n| format!(" {}", start_next(n)));
    format!(
        "llmenv task tracker: `{event}` ends a step, but '{slug}' is still in progress, with \
         {queued} task(s) queued behind it. If '{slug}' is done, run `llmenv task done {slug}`.{start}"
    )
}
