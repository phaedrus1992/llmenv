use super::*;
use serde_json::json;

const PROJECT: &str = "proj-x";

fn tmp() -> tempfile::TempDir {
    tempfile::tempdir().unwrap()
}

#[test]
fn task_create_with_two_open_sessions_joins_the_payload_conversations_session() {
    let dir = tmp();
    let start = |name: &str, conv: &str| {
        let owner = session::EngineIdentity::from_session_id(Some(conv));
        let request = session::StartRequest {
            name: Some(name),
            description: None,
            project: PROJECT,
            owner: &owner,
            resume: &crate::task::resume::ResumeContext::default(),
        };
        match session::start_session_as(dir.path(), &request, session::StartDecision::New).unwrap()
        {
            session::StartOutcome::Created(s) => s,
            other => panic!("expected Created, got {other:?}"),
        }
    };
    start("theirs", "conv-other");
    let mine = start("mine", "conv-me");

    let owner = session::EngineIdentity::from_session_id(Some("conv-me"));
    let out = handle_inner_as(
        "TaskCreate",
        Some(&json!({ "subject": "my step" })),
        dir.path(),
        PROJECT,
        &owner,
    );
    assert!(out.contains("Tracked"), "{out}");
    let task = task::list_tasks(dir.path())
        .into_iter()
        .find(|t| t.title == "my step")
        .expect("task created");
    assert_eq!(task.session.as_deref(), Some(mine.id.as_str()));
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
    assert_eq!(
        session::list_sessions_strict(dir.path())
            .expect("test")
            .len(),
        1
    );

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
        session::list_sessions_strict(dir.path())
            .expect("test")
            .len(),
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
fn update_refuses_done_on_an_open_task() {
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
    assert!(
        out.contains("never started") && out.contains("llmenv task start"),
        "open -> done must be refused with the fix (#2416): {out}"
    );
    let t = &task::list_tasks(dir.path())[0];
    assert_eq!(
        t.state,
        task::TaskState::Open,
        "a refused done leaves the task open: {t:?}"
    );
}

#[test]
fn update_completed_after_start_has_no_never_started_warning() {
    let dir = tmp();
    handle_inner(
        "TaskCreate",
        Some(&json!({ "subject": "ship it" })),
        dir.path(),
        PROJECT,
    );
    let slug = task::list_tasks(dir.path())[0].slug.clone();
    handle_inner(
        "TaskUpdate",
        Some(&json!({ "taskId": slug, "status": "in_progress" })),
        dir.path(),
        PROJECT,
    );
    let out = handle_inner(
        "TaskUpdate",
        Some(&json!({ "taskId": slug, "status": "completed" })),
        dir.path(),
        PROJECT,
    );
    assert!(!out.contains("never started"), "{out}");
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

/// One corrupt task file denies with the read error, not a short list or an empty one (#3471).
#[test]
fn list_denies_on_one_corrupt_task_file() {
    let dir = tmp();
    handle_inner(
        "TaskCreate",
        Some(&json!({ "subject": "seed" })),
        dir.path(),
        PROJECT,
    );
    std::fs::write(task::tasks_dir(dir.path()).join("broken.json"), "not json").unwrap();
    let out = handle_inner("TaskList", None, dir.path(), PROJECT);
    assert!(out.starts_with("__DENY__:"), "{out}");
    assert!(
        out.contains("couldn't read the llmenv task tracker"),
        "{out}"
    );
}
