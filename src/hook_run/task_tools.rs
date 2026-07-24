//! Redirect Claude Code's built-in task tools to the `llmenv task` tracker (#985).
//!
//! When the task tracker is enabled, the Claude Code adapter registers a
//! `PreToolUse` hook on `TaskCreate`/`TaskList`/`TaskUpdate`. Those tools would
//! otherwise write to Claude Code's own ephemeral task state, bypassing the
//! durable `llmenv task` tracker entirely — so in practice the tracker sat
//! unused. This handler intercepts each call, performs the equivalent
//! `llmenv task` operation (auto-starting a session when none is open, which
//! removes the cold-start friction that made a manual fallback fail), and
//! `deny`s the native tool with a message naming the created task so the agent
//! keeps working against the real tracker instead of abandoning tracking.
//!
//! Returns are the same `__DENY__:<reason>` sentinel that `read_once` uses;
//! `run()` turns that into Claude Code's `permissionDecision: deny` envelope.

use std::path::Path;

use serde_json::Value;

use crate::task::{self, session};

/// The Claude Code built-in task tools this handler intercepts.
const TASK_TOOLS: [&str; 3] = ["TaskCreate", "TaskList", "TaskUpdate"];

/// Intercept a Claude Code task-tool `PreToolUse` call and redirect it to the
/// `llmenv task` tracker.
///
/// Returns `Some(text)` for a task tool (always a `__DENY__:` decision — the
/// native tool is suppressed either way), or `None` when `tool_name` isn't one
/// of the task tools, so the caller falls through to the normal pipeline.
///
/// Fail-soft: resolution/tracker errors become a `deny` with a diagnostic +
/// manual fallback rather than propagating (which would let the native tool run
/// and re-diverge from the tracker) or wedging the agent.
pub(crate) fn handle_pre_tool_use(payload: &Value) -> Option<String> {
    let tool = payload.get("tool_name").and_then(Value::as_str)?;
    if !TASK_TOOLS.contains(&tool) {
        return None;
    }
    let state_dir = match crate::paths::state_dir() {
        Ok(dir) => dir,
        Err(e) => {
            return Some(deny(&format!(
                "the llmenv task tracker is unavailable ({e}); track this with `llmenv task` manually."
            )));
        }
    };
    let project = match task::project::current_tag() {
        Ok(p) => p,
        Err(e) => {
            return Some(deny(&format!(
                "couldn't resolve the project for the llmenv task tracker ({e}); \
                 track this with `llmenv task` manually."
            )));
        }
    };
    Some(handle_inner(
        tool,
        payload.get("tool_input"),
        &state_dir,
        &project,
    ))
}

/// Testable core: dispatch on the (already-validated) task tool name.
fn handle_inner(tool: &str, input: Option<&Value>, state_dir: &Path, project: &str) -> String {
    match tool {
        "TaskCreate" => create(input, state_dir, project),
        "TaskList" => list(state_dir),
        "TaskUpdate" => update(input, state_dir),
        // Unreachable: `handle_pre_tool_use` only calls this for TASK_TOOLS.
        other => deny(&format!(
            "unhandled task tool '{other}'; use `llmenv task`."
        )),
    }
}

fn create(input: Option<&Value>, state_dir: &Path, project: &str) -> String {
    let Some(subject) = str_field(input, "subject")
        .map(str::trim)
        .filter(|s| !s.is_empty())
    else {
        return deny("TaskCreate carried no `subject`; use `llmenv task add \"<title>\"`.");
    };
    // Auto-start a session when none is open. Without this the first task of a
    // session fails ("no open session — run `llmenv task session start` first"),
    // which is exactly what made the manual fallback unusable (#985).
    if session::open_sessions_for_project(state_dir, project).is_empty()
        && let Err(e) =
            session::start_session(state_dir, None, None, project, session::StartDecision::Auto)
    {
        return deny(&format!(
            "couldn't auto-start a task session ({e}); run `llmenv task session start`."
        ));
    }
    match task::add_task(state_dir, subject, None, None, project) {
        Ok(t) => {
            if let Some(desc) = str_field(input, "description")
                .map(str::trim)
                .filter(|d| !d.is_empty())
            {
                // Best-effort: the task exists; a failed note shouldn't undo it.
                let _ = task::note_task(state_dir, &t.slug, desc);
            }
            deny(&format!(
                "Tracked in the llmenv task tracker as '{slug}'. Claude's built-in task tools are \
                 redirected here so tasks persist across /clear and new sessions — do NOT stop \
                 tracking and do NOT retry TaskCreate (it will keep being redirected). Update this \
                 task with `llmenv task start|note|done {slug}` and list tasks with `llmenv task ls`.",
                slug = t.slug
            ))
        }
        Err(e) => deny(&format!(
            "couldn't record the task ({e}); track it with `llmenv task add \"{subject}\"` manually."
        )),
    }
}

fn list(state_dir: &Path) -> String {
    let tasks = task::list_tasks(state_dir);
    let refs: Vec<&task::Task> = tasks.iter().collect();
    let rendered = task::render_task_list(&refs);
    let body = if rendered.trim().is_empty() {
        "(no tasks tracked yet)".to_string()
    } else {
        rendered
    };
    deny(&format!(
        "The llmenv task tracker is authoritative (Claude's TaskList is redirected here):\n{body}"
    ))
}

fn update(input: Option<&Value>, state_dir: &Path) -> String {
    // Claude Code may stream either `taskId` or the pre-rename `task_id`.
    let Some(id) = str_field(input, "taskId")
        .or_else(|| str_field(input, "task_id"))
        .map(str::trim)
        .filter(|s| !s.is_empty())
    else {
        return deny(
            "TaskUpdate carried no `taskId`; update via `llmenv task start|note|done <id>` \
             (see `llmenv task ls` for ids).",
        );
    };
    let slug = match task::resolve_identifier(state_dir, id) {
        Ok(s) => s,
        Err(_) => {
            return deny(&format!(
                "no llmenv task matches '{id}'; run `llmenv task ls` for the current ids."
            ));
        }
    };
    let outcome = match str_field(input, "status") {
        Some("in_progress") => {
            task::start_task(state_dir, &slug).map(|_| format!("started '{slug}'"))
        }
        Some("completed") => {
            task::done_task(state_dir, &slug).map(|_| format!("completed '{slug}'"))
        }
        Some("deleted") => task::delete_task(state_dir, &slug).map(|_| format!("deleted '{slug}'")),
        _ => Ok(format!("left '{slug}' unchanged")),
    };
    // A description/subject on an update becomes a progress note.
    if let Some(note) = str_field(input, "description")
        .or_else(|| str_field(input, "subject"))
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        let _ = task::note_task(state_dir, &slug, note);
    }
    match outcome {
        Ok(msg) => deny(&format!(
            "llmenv task tracker: {msg} (TaskUpdate redirected). Keep using `llmenv task`."
        )),
        Err(e) => deny(&format!(
            "couldn't update '{slug}' ({e}); adjust it with `llmenv task` manually."
        )),
    }
}

/// Read a string field off the tool input, if present.
fn str_field<'a>(input: Option<&'a Value>, key: &str) -> Option<&'a str> {
    input.and_then(|i| i.get(key)).and_then(Value::as_str)
}

/// Wrap a message in the `__DENY__:` sentinel `run()` translates into a Claude
/// Code `permissionDecision: deny` envelope.
fn deny(msg: &str) -> String {
    format!("__DENY__:{msg}")
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use serde_json::json;

    const PROJECT: &str = "proj-x";

    fn tmp() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    #[test]
    fn non_task_tool_passes_through() {
        assert_eq!(
            handle_pre_tool_use(&json!({ "tool_name": "Read", "tool_input": {} })),
            None
        );
    }

    #[test]
    fn create_auto_starts_session_and_records_task() {
        let dir = tmp();
        let out = handle_inner(
            "TaskCreate",
            Some(&json!({ "subject": "Fix the auth bug", "description": "split the handler" })),
            dir.path(),
            PROJECT,
        );
        assert!(out.starts_with("__DENY__:"), "must deny: {out}");
        let tasks = task::list_tasks(dir.path());
        assert_eq!(tasks.len(), 1, "task should be recorded");
        assert_eq!(tasks[0].title, "Fix the auth bug");
        // The created slug is surfaced so the agent can update it.
        assert!(out.contains(&tasks[0].slug), "deny names the slug: {out}");
    }

    #[test]
    fn create_without_subject_denies_with_guidance() {
        let dir = tmp();
        let out = handle_inner("TaskCreate", Some(&json!({})), dir.path(), PROJECT);
        assert!(out.starts_with("__DENY__:"));
        assert!(out.contains("llmenv task add"));
        assert!(task::list_tasks(dir.path()).is_empty());
    }

    #[test]
    fn update_maps_status_to_done() {
        let dir = tmp();
        // Seed a task via the same create path.
        handle_inner(
            "TaskCreate",
            Some(&json!({ "subject": "ship it" })),
            dir.path(),
            PROJECT,
        );
        let slug = task::list_tasks(dir.path())[0].slug.clone();

        let out = handle_inner(
            "TaskUpdate",
            Some(&json!({ "taskId": slug, "status": "completed" })),
            dir.path(),
            PROJECT,
        );
        assert!(out.starts_with("__DENY__:"), "{out}");
        let t = &task::list_tasks(dir.path())[0];
        assert_eq!(
            t.state,
            task::TaskState::Done,
            "state should be Done: {t:?}"
        );
    }

    #[test]
    fn update_unknown_id_denies_without_error() {
        let dir = tmp();
        let out = handle_inner(
            "TaskUpdate",
            Some(&json!({ "taskId": "does-not-exist", "status": "completed" })),
            dir.path(),
            PROJECT,
        );
        assert!(out.starts_with("__DENY__:"));
        assert!(out.contains("llmenv task ls"));
    }

    #[test]
    fn list_renders_tracker_state() {
        let dir = tmp();
        handle_inner(
            "TaskCreate",
            Some(&json!({ "subject": "alpha" })),
            dir.path(),
            PROJECT,
        );
        let out = handle_inner("TaskList", None, dir.path(), PROJECT);
        assert!(out.starts_with("__DENY__:"));
        assert!(out.contains("alpha"), "list should show the task: {out}");
    }

    #[test]
    fn list_empty_is_graceful() {
        let dir = tmp();
        let out = handle_inner("TaskList", None, dir.path(), PROJECT);
        assert!(out.contains("no tasks tracked yet"), "{out}");
    }
}
