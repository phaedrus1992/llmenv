use super::*;
use serde_json::json;

mod create;

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
fn task_create_without_a_conversation_id_is_refused_for_an_owned_session() {
    let dir = tmp();
    let owner = session::EngineIdentity::from_session_id(Some("conv-other"));
    let request = session::StartRequest {
        name: Some("theirs"),
        description: None,
        project: PROJECT,
        owner: &owner,
        resume: &crate::task::resume::ResumeContext::default(),
    };
    session::start_session_as(dir.path(), &request, session::StartDecision::New).unwrap();

    // A payload with no `session_id` must not make the owned session the caller's (#3480).
    let no_owner = session::EngineIdentity::from_session_id(None);
    let out = handle_inner_as(
        "TaskCreate",
        Some(&json!({ "subject": "step" })),
        dir.path(),
        PROJECT,
        &no_owner,
    );
    assert!(out.contains("belongs to another conversation"), "{out}");
    assert!(task::list_tasks(dir.path()).is_empty());
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

/// An id with no task names the id, through the one lookup of the task command (#3474).
#[test]
fn update_of_an_unknown_id_names_it() {
    let dir = tmp();
    handle_inner(
        "TaskCreate",
        Some(&json!({ "subject": "seed" })),
        dir.path(),
        PROJECT,
    );
    let out = handle_inner(
        "TaskUpdate",
        Some(&json!({ "taskId": "nope-3474", "status": "in_progress" })),
        dir.path(),
        PROJECT,
    );
    assert!(out.contains("couldn't update task 'nope-3474'"), "{out}");
}

/// A start with a description saves the note to the slug that the start resolved (#3474).
#[test]
fn update_with_a_note_saves_it_on_the_resolved_task() {
    let dir = tmp();
    handle_inner(
        "TaskCreate",
        Some(&json!({ "subject": "first step" })),
        dir.path(),
        PROJECT,
    );
    let out = handle_inner(
        "TaskUpdate",
        Some(&json!({ "taskId": "first", "status": "in_progress", "description": "begun" })),
        dir.path(),
        PROJECT,
    );
    assert!(out.contains("started 'first-step'"), "{out}");
    assert!(!out.contains("note not saved"), "{out}");
}

/// A refused update still keeps its note, and the deny says where the note went (#3474).
#[test]
fn a_refused_update_says_where_its_note_went() {
    let dir = tmp();
    handle_inner(
        "TaskCreate",
        Some(&json!({ "subject": "never begun" })),
        dir.path(),
        PROJECT,
    );
    let out = handle_inner(
        "TaskUpdate",
        Some(&json!({ "taskId": "never-begun", "status": "completed", "description": "why" })),
        dir.path(),
        PROJECT,
    );
    assert!(out.contains("couldn't update task 'never-begun'"), "{out}");
    assert!(out.contains("note saved to 'never-begun'"), "{out}");
}
