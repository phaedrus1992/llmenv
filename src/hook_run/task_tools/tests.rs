use super::*;
use serde_json::json;

mod create;
mod update;

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
