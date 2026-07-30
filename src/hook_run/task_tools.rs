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
/// `llmenv task` tracker under `state_dir`.
///
/// `state_dir` is caller-supplied, the way `read_once`/`repeat_detect` take
/// theirs — resolving it here instead would put every test that enables the
/// tracker on the developer's real tracker (#1109).
///
/// Returns `Some(text)` for a task tool (always a `__DENY__:` decision — the
/// native tool is suppressed either way), or `None` when `tool_name` isn't one
/// of the task tools, so the caller falls through to the normal pipeline.
///
/// Fail-soft: resolution/tracker errors become a `deny` with a diagnostic +
/// manual fallback rather than propagating (which would let the native tool run
/// and re-diverge from the tracker) or wedging the agent.
pub(crate) fn handle_pre_tool_use(payload: &Value, state_dir: &Path) -> Option<String> {
    let tool = task_tool_name(payload)?;
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
        state_dir,
        &project,
    ))
}

/// The redirect's answer when the caller couldn't resolve a state dir at all:
/// still deny, so the native tool can't run and diverge from the tracker, and
/// name the `error` the caller already has rather than re-deriving one.
pub(crate) fn deny_tracker_unavailable(payload: &Value, error: &anyhow::Error) -> Option<String> {
    task_tool_name(payload)?;
    Some(deny(&format!(
        "the llmenv task tracker is unavailable ({error}); track this with `llmenv task` manually."
    )))
}

/// The intercepted tool's name, or `None` when this isn't a task tool.
fn task_tool_name(payload: &Value) -> Option<&str> {
    let tool = payload.get("tool_name").and_then(Value::as_str)?;
    TASK_TOOLS.contains(&tool).then_some(tool)
}

/// Testable core: perform the tracker operation for an already-validated task
/// tool name.
fn handle_inner(tool: &str, input: Option<&Value>, state_dir: &Path, project: &str) -> String {
    match tool {
        "TaskCreate" => create(input, state_dir, project),
        "TaskList" => list(state_dir),
        "TaskUpdate" => update(input, state_dir),
        // Unreachable: `task_tool_name` admits only TASK_TOOLS.
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
            let mut msg = format!(
                "Tracked in the llmenv task tracker as '{slug}' — do NOT stop tracking and do \
                 NOT retry TaskCreate (it will keep being redirected). Update with `llmenv task \
                 start|note|done|wait {slug}`, block on another with `llmenv task block {slug} \
                 --on <other>`, and list with `llmenv task ls`.",
                slug = t.slug
            );
            // Keep the task even if the note write fails, but surface the loss —
            // reporting unqualified success while the description silently
            // vanished would violate the "never swallow silent" rule.
            if let Some(desc) = str_field(input, "description")
                .map(str::trim)
                .filter(|d| !d.is_empty())
                && let Err(e) = task::note_task(state_dir, &t.slug, desc)
            {
                tracing::warn!(error = %e, slug = %t.slug, "task redirect: description note not saved");
                msg.push_str(&format!(" (note: the description wasn't saved — {e})"));
            }
            deny(&msg)
        }
        // `e` already carries actionable guidance (e.g. ">1 open sessions — pass
        // --session <id>"), so surface it rather than re-suggesting a bare `add`
        // that would hit the same error.
        Err(e) => deny(&format!("couldn't record the task ({e}).")),
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
            "TaskUpdate carried no `taskId`; update via `llmenv task start|note|done|wait <id>` \
             or `llmenv task block <id> --on <other>` (see `llmenv task ls` for ids).",
        );
    };
    let slug = match task::resolve_identifier(state_dir, id) {
        Ok(s) => s,
        // Preserve the real reason (invalid id, no match, OR an ambiguous prefix
        // that matched several tasks) — flattening all three to "no match" would
        // misreport a too-many-matches case as zero.
        Err(e) => {
            return deny(&format!(
                "couldn't resolve task '{id}' ({e}); run `llmenv task ls` for the current ids."
            ));
        }
    };
    let status = str_field(input, "status");
    let outcome = match status {
        Some("in_progress") => {
            task::start_task(state_dir, &slug).map(|_| format!("started '{slug}'"))
        }
        Some("completed") => {
            task::done_task(state_dir, &slug).map(|_| format!("completed '{slug}'"))
        }
        Some("deleted") => task::delete_task(state_dir, &slug).map(|_| format!("deleted '{slug}'")),
        // Name the unrecognized value rather than silently reporting "unchanged"
        // as if the requested transition had been honored.
        Some(other) => Ok(format!(
            "left '{slug}' unchanged (status '{other}' not recognized)"
        )),
        None => Ok(format!("left '{slug}' unchanged")),
    };
    // A description/subject on an update becomes a progress note — but not on a
    // delete (the task is gone). Surface a failed note rather than reporting
    // silent success.
    let mut note_warning = String::new();
    if status != Some("deleted")
        && let Some(note) = str_field(input, "description")
            .or_else(|| str_field(input, "subject"))
            .map(str::trim)
            .filter(|s| !s.is_empty())
        && let Err(e) = task::note_task(state_dir, &slug, note)
    {
        tracing::warn!(error = %e, slug = %slug, "task redirect: progress note not saved");
        note_warning = format!(" (note not saved: {e})");
    }
    match outcome {
        Ok(msg) => deny(&format!(
            "{msg}{note_warning} — llmenv task tracker (TaskUpdate redirected)."
        )),
        Err(e) => deny(&format!("couldn't update '{slug}' ({e}).")),
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

    /// A `Some` here would mask `read_once`/`repeat_detect` for every non-task
    /// tool call whenever the tracker is enabled — `resolve_pre_tool_text`
    /// treats any `Some` as the primary decision.
    #[test]
    fn non_task_tool_passes_through() {
        let dir = tmp();
        assert_eq!(
            handle_pre_tool_use(
                &json!({ "tool_name": "Read", "tool_input": {} }),
                dir.path()
            ),
            None
        );
    }

    #[test]
    fn unavailable_tracker_denies_task_tools_and_names_the_error() {
        let err = anyhow::anyhow!("HOME is not set");
        let out = deny_tracker_unavailable(&json!({ "tool_name": "TaskCreate" }), &err)
            .expect("a task tool must still get a decision");
        assert!(out.starts_with("__DENY__:"), "{out}");
        assert!(
            out.contains("HOME is not set"),
            "names the real error: {out}"
        );
        assert!(
            out.contains("llmenv task"),
            "keeps the manual fallback: {out}"
        );
    }

    /// Degrading to a deny must not swallow non-task tools — they still belong
    /// to the rest of the pipeline.
    #[test]
    fn unavailable_tracker_passes_non_task_tools_through() {
        let err = anyhow::anyhow!("HOME is not set");
        assert_eq!(
            deny_tracker_unavailable(&json!({ "tool_name": "Read" }), &err),
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
    fn update_unrecognized_status_is_reported_not_swallowed() {
        let dir = tmp();
        handle_inner(
            "TaskCreate",
            Some(&json!({ "subject": "beta" })),
            dir.path(),
            PROJECT,
        );
        let slug = task::list_tasks(dir.path())[0].slug.clone();
        // `pending` isn't a transition this handler maps — it must say so, not
        // silently claim success.
        let out = handle_inner(
            "TaskUpdate",
            Some(&json!({ "taskId": slug, "status": "pending" })),
            dir.path(),
            PROJECT,
        );
        assert!(out.contains("not recognized"), "{out}");
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
