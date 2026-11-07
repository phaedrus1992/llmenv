use super::*;

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

/// A store that cannot be read is named as the cause. The deny must not blame the id, so the
/// agent does not go looking for a different id.
#[test]
fn update_on_an_unreadable_store_blames_the_store_not_the_id() {
    let dir = tmp();
    handle_inner(
        "TaskCreate",
        Some(&json!({ "subject": "seed" })),
        dir.path(),
        PROJECT,
    );
    std::fs::write(task::tasks_dir(dir.path()).join("broken.json"), "not json").unwrap();
    let out = handle_inner(
        "TaskUpdate",
        Some(&json!({ "taskId": "seed", "status": "in_progress" })),
        dir.path(),
        PROJECT,
    );
    assert!(out.contains("the task store is unavailable"), "{out}");
    assert!(!out.contains("If the id is wrong"), "{out}");
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

/// Makes task `slug` a task with no session, as a replace of its session leaves it (#3586).
fn orphan(dir: &Path, slug: &str) {
    let mut task = crate::task::load_task(dir, slug).expect("load task");
    task.session = None;
    crate::task::save_task(dir, &task).expect("save orphaned task");
}

fn task_file(dir: &Path, slug: &str) -> String {
    let path = crate::task::tasks_dir(dir).join(format!("{slug}.json"));
    std::fs::read_to_string(path).expect("read task file")
}

/// A change to a task with no session names the caller on that task (#3586).
#[test]
fn update_of_a_task_with_no_session_records_the_caller_on_the_task() {
    let dir = tmp();
    let conv = session::EngineIdentity::from_session_id(Some("conv-me"));
    handle_inner_as(
        "TaskCreate",
        Some(&json!({ "subject": "owner-less step" })),
        dir.path(),
        PROJECT,
        &conv,
    );
    orphan(dir.path(), "owner-less-step");

    let out = handle_inner_as(
        "TaskUpdate",
        Some(&json!({ "taskId": "owner-less-step", "status": "in_progress" })),
        dir.path(),
        PROJECT,
        &conv,
    );

    assert!(out.contains("started 'owner-less-step'"), "{out}");
    assert!(
        task_file(dir.path(), "owner-less-step")
            .contains("acted on a task with no session; caller: session"),
        "the change must be on the task"
    );
}

/// A delete of a task with no session records the deletion on the caller's session (#3586).
#[test]
fn deleting_a_task_with_no_session_records_the_deletion_on_the_callers_session() {
    let dir = tmp();
    let conv = session::EngineIdentity::from_session_id(Some("conv-me"));
    handle_inner_as(
        "TaskCreate",
        Some(&json!({ "subject": "owner-less step" })),
        dir.path(),
        PROJECT,
        &conv,
    );
    orphan(dir.path(), "owner-less-step");

    let out = handle_inner_as(
        "TaskUpdate",
        Some(&json!({ "taskId": "owner-less-step", "status": "deleted" })),
        dir.path(),
        PROJECT,
        &conv,
    );

    assert!(out.contains("deleted 'owner-less-step'"), "{out}");
    let open = session::open_sessions_strict(dir.path(), PROJECT).expect("read sessions");
    let context = open
        .iter()
        .find_map(|s| s.resume.context.clone())
        .unwrap_or_default();
    assert!(
        context.contains("Deleted tasks with no session: owner-less-step"),
        "the deletion must be on the caller's session: {context}"
    );
}

/// A delete of a task with no session is refused when the caller has no session to record it on,
/// and the task stays (#3586).
#[test]
fn deleting_a_task_with_no_session_is_refused_without_a_caller_session() {
    let dir = tmp();
    let conv = session::EngineIdentity::from_session_id(Some("conv-me"));
    handle_inner_as(
        "TaskCreate",
        Some(&json!({ "subject": "owner-less step" })),
        dir.path(),
        PROJECT,
        &conv,
    );
    orphan(dir.path(), "owner-less-step");

    let out = handle_inner_as(
        "TaskUpdate",
        Some(&json!({ "taskId": "owner-less-step", "status": "deleted" })),
        dir.path(),
        PROJECT,
        &session::EngineIdentity::default(),
    );

    assert!(out.contains("no session to record"), "{out}");
    assert!(
        task_file(dir.path(), "owner-less-step").contains("owner-less step"),
        "a refused delete must leave the task"
    );
}

/// A change, as opposed to a delete, to a task with no session is allowed for a caller with no
/// session, and the change is recorded on the task (#3586).
#[test]
fn update_of_a_task_with_no_session_is_allowed_for_a_caller_without_a_session() {
    let dir = tmp();
    let conv = session::EngineIdentity::from_session_id(Some("conv-me"));
    handle_inner_as(
        "TaskCreate",
        Some(&json!({ "subject": "owner-less step" })),
        dir.path(),
        PROJECT,
        &conv,
    );
    orphan(dir.path(), "owner-less-step");

    let out = handle_inner_as(
        "TaskUpdate",
        Some(&json!({ "taskId": "owner-less-step", "status": "in_progress" })),
        dir.path(),
        PROJECT,
        &session::EngineIdentity::default(),
    );

    assert!(out.contains("started 'owner-less-step'"), "{out}");
    assert!(
        task_file(dir.path(), "owner-less-step")
            .contains("acted on a task with no session; caller: an unidentified caller"),
        "the change must be on the task"
    );
}
