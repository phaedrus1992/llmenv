//! The task reminders at the end of a turn, and the reminder for a question to the user.

use std::path::Path;

use super::{NudgeState, load, queued_reminder, save, state_path};
use crate::config::TaskTracker;
use crate::hook_run::session_state::lock_state_file;
use crate::task::Tracking;

/// Turn ends between two Stop reminders for a task in progress with tasks queued behind it. Absent
/// means 5, so a long run is reminded once every five turn ends (#3318).
const DEFAULT_STOP_NUDGE_EVERY: u32 = 5;

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

/// The question reminder for the conversation `caller`. A task of a session with no recorded
/// owner is visible to every conversation, so each conversation hears about it once, at its first
/// question, and not at each question after that (#3187). This is an exception to the
/// `visible_to` rule for the question reminder only.
pub(super) fn question_reminder(
    tracking: &Tracking,
    caller: Option<&str>,
    state_dir: &Path,
) -> String {
    let text = waiting_reminder(tracking);
    let Tracking::Tracked {
        wip: Some(slug), ..
    } = tracking
    else {
        return text;
    };
    if text.is_empty() || !crate::task::wip_session_is_ownerless(state_dir, caller, slug) {
        return text;
    }
    let first = with_stop_state(caller, state_dir, |state| {
        let first = !state.ownerless_question_reminded.contains(slug);
        if first {
            state.ownerless_question_reminded.push(slug.clone());
        }
        first
    });
    // A state that cannot be read or locked keeps the reminder, and its helper notes why.
    if first == Some(false) {
        String::new()
    } else {
        text
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
    let stalled = matches!(
        &tracking,
        Tracking::Tracked { wip: Some(_), queued, .. } if *queued > 0
    );
    if !stalled {
        // The throttle restarts with the next stalled turn end, so that one is reminded at once.
        clear_stop_turns(caller, state_dir);
    }
    match &tracking {
        Tracking::Tracked { wip: Some(_), .. } if asks => {
            question_reminder(&tracking, caller, state_dir)
        }
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
        Tracking::Tracked {
            wip: Some(slug),
            queued,
            ..
        } if *queued > 0 => stop_wip_reminder(tracker, slug, *queued, caller, state_dir),
        // The nudges cannot run, so the agent hears why at the end of each turn.
        Tracking::Unknown(cause) => format!(
            "llmenv task tracker: the task reminders did not run, because the task store cannot \
             be read ({cause}). Fix: run `llmenv doctor` to check the state directory."
        ),
        _ => String::new(),
    }
}

/// The reminder at a turn end while a task is in progress with tasks queued behind it. It comes at
/// the first such turn end, then every `stop_nudge_every` turn ends, so a long run is not nagged.
fn stop_wip_reminder(
    tracker: &TaskTracker,
    slug: &str,
    queued: usize,
    caller: Option<&str>,
    state_dir: &Path,
) -> String {
    let every = tracker
        .stop_nudge_every
        .unwrap_or(DEFAULT_STOP_NUDGE_EVERY)
        .max(1);
    // Without a usable session id no state can count the turns, so the reminder shows at each
    // turn end. A failed lock or read is noticed by its own helper and the reminder is held.
    let counted = caller.is_some_and(|id| state_path(state_dir, id).is_some());
    let due = !counted
        || with_stop_state(caller, state_dir, |state| {
            let due = state.stop_quiet_turns == 0;
            state.stop_quiet_turns = state.stop_quiet_turns.saturating_add(1) % every;
            due
        }) == Some(true);
    if !due {
        return String::new();
    }
    format!(
        "llmenv task tracker: '{slug}' is in progress, and {queued} task(s) are queued behind it. If \
         '{slug}' is done, run `llmenv task done {slug}`. Then run `llmenv task start <slug>` for the \
         next open step."
    )
}

/// Restart the Stop throttle. Only an existing state file is touched, so a session that never
/// wrote one gets no new file.
fn clear_stop_turns(caller: Option<&str>, state_dir: &Path) {
    let Some(path) = caller.and_then(|id| state_path(state_dir, id)) else {
        return;
    };
    match path.try_exists() {
        Ok(true) => {
            with_stop_state(caller, state_dir, |state| state.stop_quiet_turns = 0);
        }
        Ok(false) => {}
        Err(e) => crate::agent_notice::record_logged(format!(
            "llmenv: the task nudge state {} cannot be checked: {e}. Fix: check the permissions of \
             the state directory.",
            path.display()
        )),
    }
}

/// Apply `change` to the session's nudge state under its lock, and save the state when `change`
/// altered it. Returns `None` when there is no session, or when the state cannot be locked or
/// read. Each of those failures records its own agent notice.
fn with_stop_state<T>(
    caller: Option<&str>,
    state_dir: &Path,
    change: impl FnOnce(&mut NudgeState) -> T,
) -> Option<T> {
    let id = caller?;
    let path = state_path(state_dir, id)?;
    let dir = path.parent()?;
    let _lock = lock_state_file(dir, id, "task nudge")?;
    // A corrupt file loads as the default state with `reset` set. Saving it replaces the file,
    // so the reset notice is raised once and not again at every turn end.
    let (mut state, reset) = load(&path)?;
    let before = state.clone();
    let out = change(&mut state);
    if state != before || reset {
        // `save` records its own agent notice when the write fails.
        save(&path, &state);
    }
    Some(out)
}
