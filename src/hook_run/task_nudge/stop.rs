//! The task reminders at the end of a turn, and the reminder for a question to the user.

use std::path::Path;

use super::queued_reminder;
use crate::config::TaskTracker;
use crate::task::Tracking;

/// The reminder to park the task that waits for the user's answer.
pub(super) fn waiting_reminder(tracking: &Tracking) -> String {
    match tracking {
        Tracking::Tracked {
            wip: Some(slug), ..
        } => format!(
            "llmenv task tracker: you asked the user a question while '{slug}' is in progress. If \
             you wait for the answer, run `llmenv task wait {slug} \"<what you wait for>\"`. After \
             the user answers, run `llmenv task start {slug}`."
        ),
        _ => String::new(),
    }
}

/// The addition to the Stop reminder. A question to the user parks the task in progress, and
/// queued tasks with none in progress name the next start.
pub(crate) fn handle_stop(
    tracker: &TaskTracker,
    payload: &serde_json::Value,
    caller: Option<&str>,
    state_dir: &Path,
) -> String {
    if !tracker.nudges {
        return String::new();
    }
    let asks = payload["last_assistant_message"]
        .as_str()
        .is_some_and(|m| m.trim_end().ends_with('?'));
    let tracking = crate::task::tracking(state_dir, caller);
    match &tracking {
        Tracking::Tracked { wip: Some(_), .. } if asks => waiting_reminder(&tracking),
        Tracking::Tracked { waiting, .. } if asks && !waiting.is_empty() => format!(
            "llmenv task tracker: waiting on the user: {}. After the user answers, run `llmenv \
             task start <slug>`.",
            waiting.join(", ")
        ),
        Tracking::Tracked {
            wip: None,
            next: Some(slug),
            ..
        } => queued_reminder(slug),
        // The nudges cannot run, so the agent hears why at the end of each turn.
        Tracking::Unknown(cause) => format!(
            "llmenv task tracker: the task reminders did not run, because the task store cannot \
             be read ({cause}). Fix: run `llmenv doctor` to check the state directory."
        ),
        _ => String::new(),
    }
}
