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

mod update;

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
            tracing::error!(error = %e, "task redirect: couldn't resolve project");
            return Some(deny(&format!(
                "couldn't resolve the project for the llmenv task tracker ({e}); \
                 track this with `llmenv task` manually."
            )));
        }
    };
    // The payload's `session_id` is the conversation that owns any session
    // the redirect starts, and picks that session when several are open (#2365).
    let owner =
        session::EngineIdentity::from_session_id(payload.get("session_id").and_then(Value::as_str));
    Some(handle_inner_as(
        tool,
        payload.get("tool_input"),
        state_dir,
        &project,
        &owner,
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
    TASK_TOOLS.contains(&tool).then_some(tool)
}

/// [`handle_inner_as`] with no engine identity, for tests that do not
/// exercise session ownership.
#[cfg(test)]
fn handle_inner(tool: &str, input: Option<&Value>, state_dir: &Path, project: &str) -> String {
    handle_inner_as(
        tool,
        input,
        state_dir,
        project,
        &session::EngineIdentity::default(),
    )
}

/// Testable core: perform the tracker operation for an already-validated task
/// tool name, on behalf of `owner`.
fn handle_inner_as(
    tool: &str,
    input: Option<&Value>,
    state_dir: &Path,
    project: &str,
    owner: &session::EngineIdentity,
) -> String {
    match tool {
        "TaskCreate" => create(input, state_dir, project, owner),
        "TaskList" => list(state_dir),
        "TaskUpdate" => update::update(input, state_dir, project, owner),
        // Unreachable: `task_tool_name` admits only TASK_TOOLS.
        other => deny(&format!(
            "unhandled task tool '{other}'; use `llmenv task`."
        )),
    }
}

fn create(
    input: Option<&Value>,
    state_dir: &Path,
    project: &str,
    owner: &session::EngineIdentity,
) -> String {
    let Some(subject) = str_field(input, "subject")
        .map(str::trim)
        .filter(|s| !s.is_empty())
    else {
        return deny("TaskCreate carried no `subject`; use `llmenv task add \"<title>\"`.");
    };
    // The pick, the ownership check, and the add share one store lock. Otherwise an owner change
    // between the pick and the add goes unseen (#3589). `add_task` re-enters this lock.
    task::with_store_lock(state_dir, || {
        Ok(create_under_lock(input, state_dir, subject, project, owner))
    })
    .unwrap_or_else(|e| {
        tracing::error!(error = %format!("{e:#}"), "task redirect: task store lock failed");
        deny(&format!(
            "cannot lock the llmenv task store ({e:#}). Fix: run `llmenv doctor`, then retry TaskCreate."
        ))
    })
}

/// The pick and the add of [`create`], run under the store lock that [`create`] holds.
fn create_under_lock(
    input: Option<&Value>,
    state_dir: &Path,
    subject: &str,
    project: &str,
    owner: &session::EngineIdentity,
) -> String {
    let session = match ensure_session(state_dir, project, owner) {
        Ok(session) => session,
        Err(msg) => return msg,
    };
    match task::add_task(
        state_dir,
        subject,
        task::ParentSpec::Detached,
        task::SessionChoice::Checked(&session),
        project,
    ) {
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
                && let Err(e) =
                    task::note_task(state_dir, task::TaskTarget::Resolved(&t.slug), desc)
            {
                tracing::error!(
                    error = %e,
                    slug = %t.slug,
                    "task redirect: description note not saved"
                );
                msg.push_str(&format!(" (note: the description wasn't saved — {e:#})"));
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

/// Auto-start a task session when none is open, or return the deny to send
/// back for `create`.
///
/// Without this the first task of a session fails ("no open session — run
/// `llmenv task session start` first"), which is exactly the cold-start
/// friction that made the manual fallback unusable (#985).
///
/// Uses the strict `open_sessions_strict`: an unreadable store or a corrupt session file
/// must deny with the real error, not be misread as "no sessions open" and auto-start a
/// second session on top of an existing-but-unreadable one (#1112, #2665).
fn ensure_session(
    state_dir: &Path,
    project: &str,
    owner: &session::EngineIdentity,
) -> Result<session::Session, String> {
    let open_sessions = match session::open_sessions_strict(state_dir, project) {
        Ok(sessions) => sessions,
        Err(e) => {
            tracing::error!(error = %e, "task redirect: couldn't read existing sessions");
            return Err(deny(&format!(
                "couldn't read the llmenv task tracker's sessions ({e}); \
                 track this with `llmenv task` manually."
            )));
        }
    };
    let detected = std::env::current_dir()
        .map_err(anyhow::Error::from)
        .and_then(|cwd| crate::task::resume::ResumeContext::detected(&cwd))
        .map_err(|e| {
            deny(&format!(
                "cannot read the working directory or the git branch for the auto-started task \
                 session ({e:#}); track this with `llmenv task` manually."
            ))
        })?;
    if !open_sessions.is_empty() {
        // The one read of the session files picks the session the task goes to (#3474).
        let picked = session::pick_open_session(open_sessions, owner).map_err(|e| {
            let cause = e
                .ambiguity_message("pass --session <id>, or see `llmenv task session ls`")
                .unwrap_or_else(|| "the task session could not be picked".to_string());
            tracing::error!(cause = %cause, "task redirect: couldn't pick the task session");
            deny(&format!(
                "couldn't pick the task session ({cause}); track this with `llmenv task` manually."
            ))
        })?;
        crate::task::ownership::check_takes_tasks(&picked, owner).map_err(|e| {
            tracing::error!(error = %e, "task redirect: the picked session is not the caller's");
            deny(&format!("{e:#}; track this with `llmenv task` manually."))
        })?;
        return Ok(picked);
    }
    match session::start_session_as(
        state_dir,
        &session::StartRequest {
            name: None,
            description: None,
            project,
            owner,
            resume: &detected,
        },
        session::StartDecision::Auto,
    ) {
        Ok(
            session::StartOutcome::Created(s)
            | session::StartOutcome::Resumed(s)
            | session::StartOutcome::Replaced { session: s, .. },
        ) => Ok(s),
        Err(e) => {
            tracing::error!(error = %e, "task redirect: couldn't auto-start a session");
            Err(deny(&format!(
                "couldn't auto-start a task session ({e}); run `llmenv task session start`."
            )))
        }
    }
}

fn list(state_dir: &Path) -> String {
    // A strict read: an unreadable store or task file must deny with the real error, not be
    // misreported as "(no tasks tracked yet)" or as a short list. That claim is false and can
    // make the agent create tasks that already exist (#1112, #3471).
    let tasks = match task::list_tasks_strict(state_dir) {
        Ok(tasks) => tasks.into_vec(),
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
mod tests;
