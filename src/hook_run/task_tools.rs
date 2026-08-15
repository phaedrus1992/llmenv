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

/// opencode's built-in task-list tool (#1304). There is only one: opencode has
/// no `todoread` — reading the list happens through session state and the UI,
/// not a tool call — so `todowrite` is the entire redirect surface.
///
/// It takes the *whole* list on every call (`{ todos: [{id, content, status,
/// priority}] }`), so unlike Claude's three atomic tools a single call can add,
/// start, and finish tasks at once. See [`todowrite`] for the mapping.
const OPENCODE_TASK_TOOL: &str = "todowrite";

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
            tracing::error!(error = %e, "task redirect: couldn't resolve project");
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
    tracing::error!(error = %error, "task redirect: tracker state dir unavailable");
    Some(deny(&format!(
        "the llmenv task tracker is unavailable ({error}); track this with `llmenv task` manually."
    )))
}

/// The intercepted tool's name, or `None` when this isn't a task tool.
fn task_tool_name(payload: &Value) -> Option<&str> {
    let tool = payload.get("tool_name").and_then(Value::as_str)?;
    (TASK_TOOLS.contains(&tool) || tool == OPENCODE_TASK_TOOL).then_some(tool)
}

/// Testable core: perform the tracker operation for an already-validated task
/// tool name.
fn handle_inner(tool: &str, input: Option<&Value>, state_dir: &Path, project: &str) -> String {
    match tool {
        OPENCODE_TASK_TOOL => todowrite(input, state_dir, project),
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
    if let Err(msg) = ensure_session(state_dir, project) {
        return msg;
    }
    match task::add_task(state_dir, subject, task::ParentSpec::Auto, None, project) {
        Ok(t) => {
            let mut msg = format!(
                "Tracked in the llmenv task tracker as '{slug}' — do NOT stop tracking and do \
                 NOT retry TaskCreate (it will keep being redirected). Update with `llmenv task \
                 start|note|done|wait {slug}`, block on another with `llmenv task block {slug} \
                 --on <other>`, and list with `llmenv task ls --session {session}` (#1124: \
                 `ls` requires --session or --all, never lists every session by default).",
                slug = t.slug,
                session = t.session.as_deref().unwrap_or("<session-id>")
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
        Err(e) => {
            tracing::error!(error = %e, "task redirect: couldn't record the task");
            deny(&format!("couldn't record the task ({e})."))
        }
    }
}

/// Redirect opencode's `todowrite` (#1304).
///
/// `todowrite` replaces the whole list on every call, so the mapping is a
/// reconciliation rather than a single operation. Todos are matched to tracked
/// tasks **by title**, not by opencode's `id`: those ids are per-session and
/// carry no meaning to the tracker, whereas a title is what the user and the
/// agent both see.
///
/// | todo | tracker |
/// | --- | --- |
/// | title not tracked yet | `llmenv task add` |
/// | `status: in_progress` | `llmenv task start` |
/// | `status: completed` | `llmenv task done` |
/// | `status: pending` | left as-is |
/// | tracked task absent from the array | **left open** |
///
/// That last row is the one real judgement call, and it's deliberately the
/// conservative one. A todo vanishing from the array is ambiguous — opencode
/// sends no tombstone, so "finished", "abandoned", and "the model rewrote the
/// list and forgot one" are indistinguishable. Closing tasks on that signal
/// would silently lose work the user still cares about, so absent tasks stay
/// open and the reply says how many there were.
fn todowrite(input: Option<&Value>, state_dir: &Path, project: &str) -> String {
    let Some(todos) = input.and_then(|v| v.get("todos")).and_then(Value::as_array) else {
        return deny(
            "todowrite carried no `todos` array; track this with `llmenv task add \"<title>\"`.",
        );
    };

    // Same auto-start as `create`: without an open session the first `add`
    // fails, which is the cold-start friction that made the manual fallback
    // unusable (#985).
    if let Err(msg) = ensure_session(state_dir, project) {
        return msg;
    }

    let tracked = match task::try_list_tasks(state_dir) {
        Ok(tasks) => task::filter_tasks_for_project(state_dir, project, tasks),
        Err(e) => {
            tracing::error!(error = %e, "task redirect: couldn't read the task store");
            return deny(&format!(
                "couldn't read the llmenv task tracker ({e}); track this with `llmenv task` \
                 manually."
            ));
        }
    };

    let mut added = Vec::new();
    let mut started = Vec::new();
    let mut finished = Vec::new();
    let mut failures = Vec::new();
    let mut seen: Vec<String> = Vec::new();

    for todo in todos {
        let Some(title) = todo
            .get("content")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|t| !t.is_empty())
        else {
            failures.push("a todo carried no `content`".to_string());
            continue;
        };
        let status = todo
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or("pending");
        seen.push(title.to_string());

        let existing = tracked.iter().find(|t| t.title == title);
        let slug = match existing {
            Some(t) => t.slug.clone(),
            None => match task::add_task(state_dir, title, task::ParentSpec::Auto, None, project) {
                Ok(t) => {
                    added.push(t.slug.clone());
                    t.slug
                }
                Err(e) => {
                    tracing::error!(error = %e, title, "task redirect: couldn't record the task");
                    failures.push(format!("'{title}' couldn't be added ({e})"));
                    continue;
                }
            },
        };

        // `start`/`done` are idempotent from the caller's side — re-sending the
        // same list must not error, since opencode does exactly that whenever
        // one entry changes.
        match status {
            "in_progress" if !existing.is_some_and(|t| t.state == task::TaskState::Wip) => {
                match task::start_task(state_dir, &slug, false) {
                    Ok(_) => started.push(slug.clone()),
                    Err(e) => failures.push(format!("'{title}' couldn't be started ({e})")),
                }
            }
            "completed" if !existing.is_some_and(|t| t.state == task::TaskState::Done) => {
                match task::done_task(state_dir, &slug) {
                    Ok(_) => finished.push(slug.clone()),
                    Err(e) => failures.push(format!("'{title}' couldn't be completed ({e})")),
                }
            }
            _ => {}
        }
    }

    let dropped = tracked
        .iter()
        .filter(|t| t.state != task::TaskState::Done && !seen.contains(&t.title))
        .count();

    deny(&render_todowrite_result(
        &added, &started, &finished, &failures, dropped,
    ))
}

/// Auto-start a task session when none is open, or return the deny to send
/// back. Shared by `create` and `todowrite`.
///
/// Without this the first task of a session fails ("no open session — run
/// `llmenv task session start` first"), which is exactly the cold-start
/// friction that made the manual fallback unusable (#985).
///
/// Uses the fallible `try_open_sessions_for_project` rather than the tolerant
/// `open_sessions_for_project`: an unreadable store must deny with the real
/// error, not be misread as "no sessions open" and auto-start a second session
/// on top of an existing-but-unreadable one (#1112).
fn ensure_session(state_dir: &Path, project: &str) -> Result<(), String> {
    let open_sessions = match session::try_open_sessions_for_project(state_dir, project) {
        Ok(sessions) => sessions,
        Err(e) => {
            tracing::error!(error = %e, "task redirect: couldn't read existing sessions");
            return Err(deny(&format!(
                "couldn't read the llmenv task tracker's sessions ({e}); \
                 track this with `llmenv task` manually."
            )));
        }
    };
    if open_sessions.is_empty()
        && let Err(e) =
            session::start_session(state_dir, None, None, project, session::StartDecision::Auto)
    {
        tracing::error!(error = %e, "task redirect: couldn't auto-start a session");
        return Err(deny(&format!(
            "couldn't auto-start a task session ({e}); run `llmenv task session start`."
        )));
    }
    Ok(())
}

/// The reply for a reconciled `todowrite`. Split out so the wording is
/// testable without a tracker on disk.
fn render_todowrite_result(
    added: &[String],
    started: &[String],
    finished: &[String],
    failures: &[String],
    dropped: usize,
) -> String {
    let mut parts = Vec::new();
    if !added.is_empty() {
        parts.push(format!("added {}", added.join(", ")));
    }
    if !started.is_empty() {
        parts.push(format!("started {}", started.join(", ")));
    }
    if !finished.is_empty() {
        parts.push(format!("completed {}", finished.join(", ")));
    }
    let summary = if parts.is_empty() {
        "no changes".to_string()
    } else {
        parts.join("; ")
    };
    let mut msg = format!(
        "Mirrored into the llmenv task tracker ({summary}) — do NOT retry todowrite, it will \
         keep being redirected. The tracker is authoritative: use `llmenv task \
         start|note|done|wait <id>` and `llmenv task ls --session <id>`."
    );
    if dropped > 0 {
        msg.push_str(&format!(
            " {dropped} tracked task(s) were absent from this list and were left open rather \
             than closed — a todo dropped from the array doesn't say whether it was finished \
             or abandoned. Close them with `llmenv task done <id>` if they're finished."
        ));
    }
    if !failures.is_empty() {
        msg.push_str(&format!(" Problems: {}.", failures.join("; ")));
    }
    msg
}

fn list(state_dir: &Path) -> String {
    // Uses the fallible `try_list_tasks` rather than the tolerant `list_tasks`:
    // an unreadable store must deny with the real error, not be misreported as
    // "(no tasks tracked yet)" — an affirmative claim that is false and can
    // make the agent re-create tasks that already exist (#1112).
    let tasks = match task::try_list_tasks(state_dir) {
        Ok(tasks) => tasks,
        Err(e) => {
            tracing::error!(error = %e, "task redirect: couldn't read the task store");
            return deny(&format!(
                "couldn't read the llmenv task tracker ({e}); track this with `llmenv task` manually."
            ));
        }
    };
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
             or `llmenv task block <id> --on <other>` (see `llmenv task ls --all` for ids).",
        );
    };
    let slug = match task::resolve_identifier(state_dir, id) {
        Ok(s) => s,
        // Preserve the real reason (invalid id, no match, OR an ambiguous prefix
        // that matched several tasks) — flattening all three to "no match" would
        // misreport a too-many-matches case as zero.
        Err(e) => {
            tracing::error!(error = %e, id = %id, "task redirect: couldn't resolve task id");
            return deny(&format!(
                "couldn't resolve task '{id}' ({e}); \
                 run `llmenv task ls --all` for the current ids."
            ));
        }
    };
    let status = str_field(input, "status");
    let outcome = match status {
        Some("in_progress") => {
            // force=false: the redirect must enforce the same hard-block on
            // an unmet `blocked_on` (#1164) that `llmenv task start` does --
            // an agent shouldn't bypass it just by using the native tool.
            task::start_task(state_dir, &slug, false).map(|started| {
                // The parent soft-block warning (#1164) belongs here too --
                // this redirect is exactly where an agent is likely to be
                // starting a subtask under a not-yet-done parent.
                match task::parent_soft_block_warning(state_dir, &started) {
                    Some(warning) => format!("started '{slug}'. {warning}"),
                    None => format!("started '{slug}'"),
                }
            })
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
        Err(e) => {
            tracing::error!(error = %e, slug = %slug, "task redirect: couldn't update task");
            deny(&format!("couldn't update '{slug}' ({e})."))
        }
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
    fn todowrite_payload(todos: serde_json::Value) -> Value {
        json!({ "tool_name": "todowrite", "tool_input": { "todos": todos } })
    }

    fn todo(content: &str, status: &str) -> serde_json::Value {
        json!({ "id": format!("todo-{content}"), "content": content, "status": status,
                "priority": "medium" })
    }

    // #1304: one call carries the whole list, so add/start/complete can all
    // happen at once and the tracker has to reconcile rather than apply a
    // single operation.
    #[test]
    fn todowrite_adds_starts_and_completes_in_one_call() {
        let dir = tmp();
        let first = handle_inner(
            "todowrite",
            todowrite_payload(json!([todo("write the parser", "in_progress")])).get("tool_input"),
            dir.path(),
            PROJECT,
        );
        assert!(first.starts_with("__DENY__:"), "{first}");
        assert!(first.contains("added"), "{first}");
        assert!(
            first.contains("started"),
            "an in_progress todo starts the task: {first}"
        );
        assert_eq!(
            task::list_tasks(dir.path())
                .iter()
                .find(|t| t.title == "write the parser")
                .map(|t| t.state),
            Some(task::TaskState::Wip),
            "in_progress must reach the tracker as `wip`, not just be reported"
        );

        let second = handle_inner(
            "todowrite",
            todowrite_payload(json!([
                todo("write the parser", "completed"),
                todo("write the tests", "pending"),
            ]))
            .get("tool_input"),
            dir.path(),
            PROJECT,
        );
        assert!(second.contains("completed"), "{second}");

        let tasks = task::list_tasks(dir.path());
        let parser = tasks
            .iter()
            .find(|t| t.title == "write the parser")
            .expect("parser task tracked");
        assert_eq!(parser.state, task::TaskState::Done);
        assert!(
            tasks.iter().any(|t| t.title == "write the tests"),
            "a pending todo is still tracked, just not started"
        );
    }

    // Re-sending an unchanged list is opencode's normal behaviour — every edit
    // resends everything — so it must not error or double-apply.
    #[test]
    fn todowrite_is_idempotent_for_an_unchanged_list() {
        let dir = tmp();
        let payload = todowrite_payload(json!([todo("ship it", "in_progress")]));
        let first = handle_inner("todowrite", payload.get("tool_input"), dir.path(), PROJECT);
        let second = handle_inner("todowrite", payload.get("tool_input"), dir.path(), PROJECT);
        assert!(first.contains("added"), "{first}");
        assert!(
            second.contains("no changes"),
            "a resent list should be a no-op, got {second}"
        );
        assert_eq!(task::list_tasks(dir.path()).len(), 1, "no duplicate task");

        // ...and the same for a completed entry: opencode keeps sending
        // finished todos in the array forever, so `done` must fire once.
        let done = todowrite_payload(json!([todo("ship it", "completed")]));
        let closed = handle_inner("todowrite", done.get("tool_input"), dir.path(), PROJECT);
        assert!(closed.contains("completed"), "{closed}");
        let resent = handle_inner("todowrite", done.get("tool_input"), dir.path(), PROJECT);
        assert!(
            resent.contains("no changes"),
            "an already-done todo must not be re-completed, got {resent}"
        );
    }

    // The one real judgement call: a todo vanishing from the array carries no
    // signal about *why*, so the task stays open and the reply says so.
    #[test]
    fn todowrite_leaves_dropped_todos_open_and_says_so() {
        let dir = tmp();
        // A finished task alongside the open one: it is *also* absent from the
        // next list, but it's already done, so it must not be counted as
        // dropped. Without it the count can't tell `&&` from `||`.
        handle_inner(
            "todowrite",
            todowrite_payload(json!([
                todo("keep me", "pending"),
                todo("keep me too", "pending"),
                todo("already finished", "completed"),
            ]))
            .get("tool_input"),
            dir.path(),
            PROJECT,
        );
        let out = handle_inner(
            "todowrite",
            todowrite_payload(json!([todo("something else", "pending")])).get("tool_input"),
            dir.path(),
            PROJECT,
        );
        assert!(
            out.contains("2 tracked task(s) were absent"),
            "exactly the two open, unlisted tasks count as dropped — counting \
             the finished one too, or counting only it, is wrong: {out}"
        );
        assert!(
            out.contains("left open"),
            "the reply must explain the dropped task, got {out}"
        );
        let kept = task::list_tasks(dir.path())
            .into_iter()
            .find(|t| t.title == "keep me")
            .expect("dropped task still tracked");
        assert_ne!(
            kept.state,
            task::TaskState::Done,
            "a dropped todo must not be silently completed"
        );
    }

    #[test]
    fn todowrite_without_a_todos_array_denies_with_a_fallback() {
        let dir = tmp();
        let out = handle_inner(
            "todowrite",
            json!({ "tool_input": {} }).get("tool_input"),
            dir.path(),
            PROJECT,
        );
        assert!(out.starts_with("__DENY__:"), "{out}");
        assert!(out.contains("llmenv task add"), "{out}");
    }

    #[test]
    fn todowrite_reports_a_todo_with_no_content_instead_of_skipping_silently() {
        let dir = tmp();
        let out = handle_inner(
            "todowrite",
            todowrite_payload(json!([json!({ "id": "x", "status": "pending" })])).get("tool_input"),
            dir.path(),
            PROJECT,
        );
        assert!(out.contains("no `content`"), "{out}");
    }

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

    #[cfg(unix)]
    #[test]
    fn list_denies_with_real_error_on_unreadable_store_not_empty_message() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tmp();
        // Seed a task so the store exists before making it unreadable.
        handle_inner(
            "TaskCreate",
            Some(&json!({ "subject": "seed" })),
            dir.path(),
            PROJECT,
        );
        let tasks_dir = task::tasks_dir(dir.path());
        std::fs::set_permissions(&tasks_dir, std::fs::Permissions::from_mode(0o000)).unwrap();

        let readable_anyway = std::fs::read_dir(&tasks_dir).is_ok();
        let out = handle_inner("TaskList", None, dir.path(), PROJECT);

        std::fs::set_permissions(&tasks_dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        if readable_anyway {
            return; // running as root / FS ignores perms — can't exercise EACCES
        }
        assert!(out.starts_with("__DENY__:"), "{out}");
        assert!(
            !out.contains("(no tasks tracked yet)"),
            "must not claim the store is empty when it's actually unreadable: {out}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn create_does_not_auto_start_second_session_when_store_unreadable() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tmp();
        // Seed one real session for PROJECT.
        handle_inner(
            "TaskCreate",
            Some(&json!({ "subject": "seed" })),
            dir.path(),
            PROJECT,
        );
        assert_eq!(session::list_sessions(dir.path()).len(), 1);

        let sessions_dir = task::tasks_dir(dir.path()).join("sessions");
        std::fs::set_permissions(&sessions_dir, std::fs::Permissions::from_mode(0o000)).unwrap();

        let readable_anyway = std::fs::read_dir(&sessions_dir).is_ok();
        let out = handle_inner(
            "TaskCreate",
            Some(&json!({ "subject": "second task" })),
            dir.path(),
            PROJECT,
        );

        std::fs::set_permissions(&sessions_dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        if readable_anyway {
            return; // running as root / FS ignores perms — can't exercise EACCES
        }
        assert!(out.starts_with("__DENY__:"), "{out}");
        assert_eq!(
            session::list_sessions(dir.path()).len(),
            1,
            "must not auto-start a second session over an unreadable store: {out}"
        );
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
