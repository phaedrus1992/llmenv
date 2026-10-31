use super::*;

#[test]
fn the_work_nudge_fires_after_n_calls_and_then_every_m() {
    let dir = TempDir::new().unwrap();
    let tracker = TaskTracker {
        nudge_after: Some(3),
        nudge_every: Some(2),
        ..TaskTracker::default()
    };
    let call = |name: &str| {
        handle_post_tool_use(
            &tracker,
            &serde_json::json!({ "tool_name": name }),
            Some("s1"),
            dir.path(),
        )
    };
    // No session yet: the project is untracked.
    assert_eq!(call("Edit"), "");
    assert_eq!(call("Write"), "");
    let third = call("Edit");
    assert!(
        third.contains("3 file edits") && third.contains("llmenv task session start"),
        "{third}"
    );
    assert_eq!(call("Edit"), "");
    assert!(call("Edit").contains("5 file edits"));
    // A read-only tool is not counted.
    assert_eq!(call("Read"), "");
}

#[test]
fn a_read_only_shell_command_is_never_counted_as_a_change() {
    let dir = TempDir::new().unwrap();
    let tracker = TaskTracker {
        nudge_after: Some(1),
        ..TaskTracker::default()
    };
    for _ in 0..3 {
        let text = handle_post_tool_use(
            &tracker,
            &bash("sed -n 1,5p README.md"),
            Some("s1"),
            dir.path(),
        );
        assert_eq!(text, "");
    }
    assert!(!dir.path().join("task_nudge").exists());
}

#[test]
fn a_session_with_tasks_stops_the_nudges_and_resets_the_count() {
    let dir = TempDir::new().unwrap();
    let tracker = TaskTracker {
        nudge_after: Some(2),
        ..TaskTracker::default()
    };
    let post = |name: &str| {
        handle_post_tool_use(
            &tracker,
            &serde_json::json!({ "tool_name": name }),
            Some("s1"),
            dir.path(),
        )
    };
    assert_eq!(post("Edit"), "");
    let session = open_session(dir.path());
    let text = post("Edit");
    assert!(
        text.contains("llmenv task add"),
        "an empty session gets the add command: {text}"
    );
    add(dir.path(), &session, "Step one");
    // Queued and not started: the first edit asks for the start, then the nudges stay quiet.
    assert!(post("Edit").contains("llmenv task start step-one"));
    for _ in 0..5 {
        assert_eq!(post("Edit"), "");
    }
    let (state, _) = load(&state_path(dir.path(), "s1").unwrap()).unwrap();
    assert_eq!(state.calls, 0, "tracked work resets the count");
}

#[test]
fn nudges_off_means_no_text_and_no_state() {
    let dir = TempDir::new().unwrap();
    let tracker = TaskTracker {
        nudges: false,
        nudge_after: Some(1),
        ..TaskTracker::default()
    };
    let text = handle_post_tool_use(
        &tracker,
        &serde_json::json!({ "tool_name": "Edit" }),
        Some("s1"),
        dir.path(),
    );
    assert_eq!(text, "");
    assert!(!dir.path().join("task_nudge").exists());
}

#[test]
fn the_skill_reminder_fires_once_for_a_workflow_skill_with_no_task() {
    let dir = TempDir::new().unwrap();
    let tracker = TaskTracker::default();
    let skill = |name: &str| {
        handle_post_tool_use(
            &tracker,
            &serde_json::json!({ "tool_name": "Skill", "tool_input": { "skill": name } }),
            Some("s1"),
            dir.path(),
        )
    };
    assert_eq!(skill("brainstorming"), "");
    let first = skill("nbl-dev:ship-issue");
    assert!(
        first.contains("'nbl-dev:ship-issue'") && first.contains("--task"),
        "{first}"
    );
    assert_eq!(skill("dev-sprint"), "", "once for each session");
}

#[test]
fn the_skill_reminder_stays_quiet_when_tasks_exist() {
    let dir = TempDir::new().unwrap();
    let session = open_session(dir.path());
    add(dir.path(), &session, "Step one");
    let text = handle_post_tool_use(
        &TaskTracker::default(),
        &serde_json::json!({ "tool_name": "Skill", "tool_input": { "skill": "dev-sprint" } }),
        Some("s1"),
        dir.path(),
    );
    assert_eq!(text, "");
}

#[test]
fn a_stop_with_queued_tasks_and_none_started_names_the_next_one() {
    let dir = TempDir::new().unwrap();
    let session = open_session(dir.path());
    add(dir.path(), &session, "Step one");
    for message in ["Ready?", "Done."] {
        let stop = serde_json::json!({ "last_assistant_message": message });
        let text = handle_stop(&TaskTracker::default(), &stop, Some("s1"), dir.path());
        assert!(
            text.contains("llmenv task start step-one"),
            "{message}: {text}"
        );
    }
}

#[test]
fn the_commit_deny_names_the_next_open_task_when_tasks_are_queued() {
    let dir = TempDir::new().unwrap();
    let session = open_session(dir.path());
    add(dir.path(), &session, "Step one");
    add(dir.path(), &session, "Step two");
    let denied = handle_pre_tool_use(
        &TaskTracker::default(),
        &bash("git commit -m x"),
        Some("s1"),
        dir.path(),
    );
    assert!(
        denied.contains("llmenv task start step-one"),
        "the deny must name the start command for step one; got {} bytes",
        denied.len()
    );
    assert!(
        !denied.contains("llmenv task add"),
        "the deny must not suggest adding a task; got {} bytes",
        denied.len()
    );
}

#[test]
fn a_file_edit_with_queued_tasks_and_none_started_nudges_once() {
    let dir = TempDir::new().unwrap();
    let session = open_session(dir.path());
    add(dir.path(), &session, "Step one");
    let tracker = TaskTracker::default();
    let edit = serde_json::json!({ "tool_name": "Edit" });
    let post = || handle_post_tool_use(&tracker, &edit, Some("s1"), dir.path());
    let first = post();
    assert!(first.contains("llmenv task start step-one"), "{first}");
    assert_eq!(post(), "", "once until a task starts");
}

#[test]
fn the_next_file_edit_after_a_task_is_done_nudges_the_next_start() {
    let dir = TempDir::new().unwrap();
    let session = open_session(dir.path());
    let one = add(dir.path(), &session, "Step one");
    add(dir.path(), &session, "Step two");
    start_task(dir.path(), &one, false).unwrap();
    let tracker = TaskTracker::default();
    let edit = serde_json::json!({ "tool_name": "Edit" });
    let post = || handle_post_tool_use(&tracker, &edit, Some("s1"), dir.path());
    assert_eq!(post(), "", "a task is in progress");
    done_task(dir.path(), &one).unwrap();
    let text = post();
    assert!(text.contains("llmenv task start step-two"), "{text}");
}
