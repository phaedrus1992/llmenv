//! Task tracking nudges and the commit deny-once (#2456), engine-neutral.
//!
//! The task tracker only reminded the agent at SessionStart and Stop. These handlers fire while
//! the work happens: after a workflow skill starts, after several tool calls with no task, after
//! a question to the user, and before the first `git commit` or `gh pr create` with no task in
//! progress. Per-session counters live in `state_dir/task_nudge/{session_id}.json`.
//!
//! Fail-soft: an I/O error is logged and named to the agent, and the call passes through. A deny
//! needs its marker saved.
//! Load-modify-save has no lock, like `repeat_detect`. Parallel tool calls of one session can
//! lose an update, which costs one extra or one missed nudge and nothing more.
//! The tracker state is per project, not per session: a task in progress in any open session of
//! the project counts as tracked work.
//! Design: docs/design/issue-2438-task-tracking-nudges.md

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::config::TaskTracker;
use crate::hook_run::shell_split::{
    ends_task_before_commit, nesting_reason, runs_commit_or_pr, shell_segments,
};
use crate::hook_run::shell_tool::is_shell_tool;
use crate::task::Tracking;

mod boundary;
mod deny;
mod notices;
mod state;
mod stop;
use boundary::boundary_reminder;
pub(crate) use deny::state_dir_deny;
use deny::{
    gate_unavailable_deny, state_unreadable_deny, store_unreadable_deny, unsaved_state_deny,
};
use notices::{needs_state, note_no_state, note_unlocked_skip};
use state::{load, read_state, save, state_path};
pub(crate) use stop::handle_stop;
#[cfg(test)]
pub(crate) use stop::handle_stop_read;
use stop::question_reminder;

/// Skills that start a multi-step workflow, when `workflow_skills` is not set.
const DEFAULT_WORKFLOW_SKILLS: [&str; 5] = [
    "dev-sprint",
    "ship-issue",
    "pre-pr-review",
    "executing-plans",
    "writing-plans",
];
/// Mutating tool calls with no task before the first nudge. A short task is two or three calls,
/// so eight means the work has more than one part.
const DEFAULT_NUDGE_AFTER: u32 = 8;
/// Calls between later nudges, so a long untracked run is reminded without nagging.
const DEFAULT_NUDGE_EVERY: u32 = 20;
/// Tools that change project files. `Bash` is left out: a read-only shell command is not a change,
/// and the commit gate covers commits.
const MUTATING_TOOLS: [&str; 3] = ["Edit", "Write", "MultiEdit"];

#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct NudgeState {
    /// Mutating calls seen while the work was untracked.
    #[serde(default)]
    calls: u32,
    /// The value of `calls` at the last nudge.
    #[serde(default)]
    last_nudge: u32,
    /// The skill reminder fired in this session.
    #[serde(default)]
    skill_reminded: bool,
    /// A commit or PR was denied once, and the retry is allowed.
    #[serde(default)]
    commit_denied: bool,
    /// The next task that the start reminder named. The reminder fires again when the next task
    /// changes, so a task started and finished between two tool calls still re-arms it.
    #[serde(default)]
    start_reminded: Option<String>,
    /// The step-boundary reminders fired, one key per task in progress and boundary kind, such as
    /// `ship-x gh pr merge` (#3318).
    #[serde(default)]
    boundary_reminded: Vec<String>,
    /// Turn ends since the last Stop reminder for a task in progress with tasks queued behind it.
    #[serde(default)]
    stop_quiet_turns: u32,
    /// The tasks of ownerless sessions that the question reminder already named in this
    /// conversation (#3187).
    #[serde(default)]
    ownerless_question_reminded: Vec<String>,
}

/// The commands that register work, for the state of the project.
fn how_to_track(tracking: &Tracking) -> String {
    match tracking {
        Tracking::NoSession => "Start a session with its first tasks: `llmenv task session start \
             <name> --task \"<step 1>\" --task \"<step 2>\"`. Then run `llmenv task start <slug>` \
             for the step you do first."
            .to_string(),
        Tracking::Tracked {
            next: Some(slug), ..
        } => start_next(slug),
        _ => "Add a task for each step: `llmenv task add \"<step>\"`. Then run `llmenv task start \
             <slug>` for the step you do first. Use `--child-of <slug>` for the parts of a step."
            .to_string(),
    }
}

/// The command that starts a task that is queued and not yet in progress.
fn start_next(slug: &str) -> String {
    format!("Run `llmenv task start {slug}` for the next open step.")
}

/// The skill name without its plugin prefix: `nbl-dev:ship-issue` is `ship-issue`.
fn bare_skill(name: &str) -> &str {
    name.rsplit(':').next().unwrap_or(name)
}

fn is_workflow_skill(tracker: &TaskTracker, skill: &str) -> bool {
    let skill = bare_skill(skill.trim());
    match &tracker.workflow_skills {
        Some(list) => list.iter().any(|s| bare_skill(s.trim()) == skill),
        None => DEFAULT_WORKFLOW_SKILLS.contains(&skill),
    }
}

fn untracked(tracking: &Tracking) -> bool {
    matches!(tracking, Tracking::NoSession | Tracking::NoTasks)
}

/// The advisory text after a tool call, or an empty string.
pub(crate) fn handle_post_tool_use(
    tracker: &TaskTracker,
    payload: &serde_json::Value,
    session_id: Option<&str>,
    state_dir: &Path,
) -> String {
    if !tracker.nudges {
        return String::new();
    }
    let Some(tool) = payload["tool_name"].as_str() else {
        return String::new();
    };
    let tracking = crate::task::tracking(state_dir, session_id);
    if tool == "AskUserQuestion" {
        return question_reminder(&tracking, session_id, state_dir);
    }
    let Some(path) = session_id.and_then(|id| state_path(state_dir, id)) else {
        if needs_state(tool) {
            note_no_state(session_id);
        }
        return String::new();
    };
    // Held from the load to the save, so a concurrent hook cannot overwrite a deny marker (#2678).
    // No lock without a directory: taking it would create the directory for a session that never
    // wrote state, and there is no state yet to race on.
    let dir_and_id = session_id.zip(path.parent().filter(|dir| dir.is_dir()));
    let lock = dir_and_id
        .and_then(|(id, dir)| super::session_state::lock_state_file(dir, id, "task nudge"));
    // A save without the lock could overwrite the commit gate's deny marker, so it is skipped
    // and the agent is told (#2747).
    let may_save = dir_and_id.is_none() || lock.is_some();
    let Some((mut state, reset)) = load(&path) else {
        return String::new();
    };
    let before = state.clone();
    let text = if tool == "Skill" {
        skill_reminder(tracker, payload, &tracking, &mut state)
    } else if MUTATING_TOOLS.contains(&tool) {
        mutation_reminder(tracker, &tracking, &mut state)
    } else if is_shell_tool(tool) {
        boundary_reminder(payload, &tracking, &mut state)
    } else {
        String::new()
    };
    settle(&tracking, &mut state);
    if state != before || reset {
        if may_save {
            save(&path, &state);
        } else {
            note_unlocked_skip(&path);
        }
    }
    text
}

/// Logs and notices that the nudges are off for a call, because the payload has no session id
/// that can name a state file (#2536).
/// Reset the counters while tasks are tracked.
fn settle(tracking: &Tracking, state: &mut NudgeState) {
    if matches!(tracking, Tracking::Tracked { .. }) {
        state.calls = 0;
        state.last_nudge = 0;
    }
}

/// The reminder after a file edit. Queued tasks with none in progress get the start reminder,
/// once until a task starts. Otherwise the edit counts toward the work nudge.
fn mutation_reminder(tracker: &TaskTracker, tracking: &Tracking, state: &mut NudgeState) -> String {
    match tracking {
        Tracking::Tracked {
            wip: None,
            next: Some(slug),
            ..
        } => start_reminder(slug, state),
        _ => work_nudge(tracker, tracking, state),
    }
}

fn start_reminder(slug: &str, state: &mut NudgeState) -> String {
    if state.start_reminded.as_deref() == Some(slug) {
        return String::new();
    }
    state.start_reminded = Some(slug.to_owned());
    queued_reminder(slug)
}

/// The reminder that tasks are queued and none is in progress, with the command that starts one.
fn queued_reminder(slug: &str) -> String {
    format!(
        "llmenv task tracker: tasks are queued, but none is in progress. {}",
        start_next(slug)
    )
}

fn skill_reminder(
    tracker: &TaskTracker,
    payload: &serde_json::Value,
    tracking: &Tracking,
    state: &mut NudgeState,
) -> String {
    let skill = payload["tool_input"]["skill"].as_str().unwrap_or_default();
    if state.skill_reminded || !untracked(tracking) || !is_workflow_skill(tracker, skill) {
        return String::new();
    }
    state.skill_reminded = true;
    format!(
        "llmenv task tracker: the skill '{skill}' runs in several steps, and no task tracks them. \
         {}",
        how_to_track(tracking)
    )
}

fn work_nudge(tracker: &TaskTracker, tracking: &Tracking, state: &mut NudgeState) -> String {
    if !untracked(tracking) {
        return String::new();
    }
    state.calls = state.calls.saturating_add(1);
    let after = tracker.nudge_after.unwrap_or(DEFAULT_NUDGE_AFTER);
    let every = tracker.nudge_every.unwrap_or(DEFAULT_NUDGE_EVERY);
    let due = if state.last_nudge == 0 {
        state.calls >= after
    } else {
        state.calls.saturating_sub(state.last_nudge) >= every
    };
    if !due {
        return String::new();
    }
    state.last_nudge = state.calls;
    format!(
        "llmenv task tracker: {} file edits ran with no task open. If the work has more than one \
         part, track it. {}",
        state.calls,
        how_to_track(tracking)
    )
}

/// The `__DENY__` text for the first commit or pull request with no task in progress, or an
/// empty string. The retry passes, and the marker clears when a task is in progress.
pub(crate) fn handle_pre_tool_use(
    tracker: &TaskTracker,
    payload: &serde_json::Value,
    session_id: Option<&str>,
    state_dir: &Path,
) -> String {
    let is_shell = payload["tool_name"].as_str().is_some_and(is_shell_tool);
    if !tracker.enforce_commit || !is_shell {
        return String::new();
    }
    let Some(command) = payload["tool_input"]["command"].as_str() else {
        // An empty command would pass unchecked, so a malformed call is denied and named (#2536).
        tracing::error!(
            "task commit gate: the Bash call has no string tool_input.command: {}",
            payload["tool_input"]
        );
        return gate_unavailable_deny("the Bash call has no string `tool_input.command`");
    };
    if !runs_commit_or_pr(command) {
        return String::new();
    }
    let Some(id) = session_id else {
        return gate_unavailable_deny("the hook payload has no session id");
    };
    let Some(path) = state_path(state_dir, id) else {
        return gate_unavailable_deny(&format!(
            "the session id {id:?} is not a plain name, so its state file cannot be placed"
        ));
    };
    // Same lock as the post-tool handler, so the two read-change-save cycles cannot interleave (#2678).
    // Without the lock, this check and a post-tool save could overwrite each other's marker,
    // so the commit is denied until the lock can be taken (#2747).
    let lock = match path.parent() {
        Some(dir) => super::session_state::lock_state_file_why(dir, id, "task nudge"),
        None => Err(format!("{} has no parent folder", path.display())),
    };
    let _lock = match lock {
        Ok(lock) => lock,
        Err(why) => {
            return gate_unavailable_deny(&format!(
                "the lock for the task nudge state {} cannot be taken: {why}",
                path.display()
            ));
        }
    };
    let tracking = crate::task::tracking(state_dir, Some(id));
    let (mut state, reset) = match read_state(&path) {
        Ok(loaded) => loaded,
        Err(e) => return state_unreadable_deny(&e),
    };
    // A failed save here is noticed by `save`; the branches below save again where it counts.
    if reset {
        save(&path, &state);
    }
    // The tracking is read before the command runs, so a task that this command ends still shows
    // as in progress. It does not count for the commit.
    let nested = shell_segments(command).is_err();
    let wip_open = match &tracking {
        Tracking::Tracked {
            wip: Some(slug), ..
        } => !ends_task_before_commit(command, slug),
        _ => false,
    };
    match tracking {
        Tracking::Unknown(cause) => {
            tracing::error!("task commit gate: the task store cannot be read: {cause}");
            store_unreadable_deny(&cause)
        }
        _ if wip_open => {
            // A marker that stays set would let a later commit with no task pass unchecked, so a
            // failed clear denies this commit too (#2630).
            if state.commit_denied {
                state.commit_denied = false;
                if !save(&path, &state) {
                    return unsaved_state_deny(&path);
                }
            }
            String::new()
        }
        // A command nested too deeply to split is denied on every attempt. Only a split command
        // can pass, so the retry that passes the no-task deny does not apply to it.
        _ if state.commit_denied && !nested => String::new(),
        tracking => {
            state.commit_denied = true;
            // Without the marker the retry would be denied again, so the deny cannot be recorded
            // and the commit is denied as well (#2630).
            if !save(&path, &state) {
                return unsaved_state_deny(&path);
            }
            if nested {
                return format!(
                    "__DENY__:llmenv blocked this commit or pull request once: {}",
                    nesting_reason()
                );
            }
            format!(
                "__DENY__:llmenv blocked this commit or pull request once: no task is in \
                 progress. {} Then run the same command again, and it goes through. To turn this \
                 off, set features.task_tracker.enforce_commit to false.",
                how_to_track(&tracking)
            )
        }
    }
}

#[cfg(test)]
#[expect(clippy::unwrap_used, clippy::panic, reason = "test code")]
mod tests;
