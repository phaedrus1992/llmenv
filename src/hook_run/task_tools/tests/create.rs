//! Hook tests for the TaskCreate redirect: session auto-start, descriptions, and bad input.

use super::*;
use serde_json::json;

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
fn create_saves_a_description_as_a_note_and_skips_a_blank_one() {
    let dir = tmp();
    handle_inner(
        "TaskCreate",
        Some(&json!({ "subject": "with note", "description": " split the handler " })),
        dir.path(),
        PROJECT,
    );
    let blank = handle_inner(
        "TaskCreate",
        Some(&json!({ "subject": "blank note", "description": "   " })),
        dir.path(),
        PROJECT,
    );
    assert!(!blank.contains("wasn't saved"), "{blank}");
    let notes_of = |title: &str| {
        let task = task::list_tasks(dir.path())
            .into_iter()
            .find(|t| t.title == title)
            .unwrap();
        serde_json::to_value(&task).unwrap()["notes"]
            .as_array()
            .map_or(0, Vec::len)
    };
    assert_eq!(notes_of("with note"), 1);
    assert_eq!(notes_of("blank note"), 0);
}

#[test]
fn create_without_subject_denies_with_guidance() {
    let dir = tmp();
    let out = handle_inner("TaskCreate", Some(&json!({})), dir.path(), PROJECT);
    assert!(out.starts_with("__DENY__:"));
    assert!(out.contains("llmenv task add"));
    assert!(task::list_tasks(dir.path()).is_empty());
}
