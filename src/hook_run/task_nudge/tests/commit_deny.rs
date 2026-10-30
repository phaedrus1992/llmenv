use super::*;

#[test]
fn the_first_commit_with_no_task_in_progress_is_denied_and_the_retry_passes() {
    let dir = TempDir::new().unwrap();
    let tracker = TaskTracker::default();
    let pre = |command: &str| handle_pre_tool_use(&tracker, &bash(command), Some("s1"), dir.path());
    let denied = pre("git commit -m x");
    assert!(
        denied.starts_with("__DENY__:"),
        "the commit gate must deny; got {} bytes",
        denied.len()
    );
    assert!(
        denied.contains("llmenv task session start"),
        "the deny must name the session start command; got {} bytes",
        denied.len()
    );
    assert!(
        denied.contains("enforce_commit"),
        "the deny must name enforce_commit; got {} bytes",
        denied.len()
    );
    assert_eq!(pre("git commit -m x"), "", "the retry is allowed");
    assert_eq!(pre("git status"), "", "other commands never deny");
}

#[test]
fn a_busy_lock_denies_the_commit_and_skips_the_nudge_save() {
    let dir = TempDir::new().unwrap();
    let tracker = TaskTracker {
        nudge_after: Some(1),
        ..TaskTracker::default()
    };
    let path = state_path(dir.path(), "busy-2747").unwrap();
    let state_dir = path.parent().unwrap();
    let held =
        crate::hook_run::session_state::lock_state_file(state_dir, "busy-2747", "test").unwrap();

    let denied = handle_pre_tool_use(
        &tracker,
        &bash("git commit -m x"),
        Some("busy-2747"),
        dir.path(),
    );
    // The message names no session id: the deny text holds the state path.
    assert!(
        denied.starts_with("__DENY__:") && denied.contains("busy"),
        "a busy lock must deny the commit"
    );
    let edit = serde_json::json!({ "tool_name": "Edit" });
    handle_post_tool_use(&tracker, &edit, Some("busy-2747"), dir.path());
    assert!(!path.exists(), "no save without the lock");
    let notices = crate::agent_notice::current().unwrap_or_default();
    assert!(
        notices.contains(&path.display().to_string()),
        "the skipped save must be noticed"
    );

    drop(held);
    handle_post_tool_use(&tracker, &edit, Some("busy-2747"), dir.path());
    assert!(path.exists(), "the save goes through once the lock is free");
}

#[test]
fn a_task_in_progress_allows_the_commit_and_rearms_the_deny() {
    let dir = TempDir::new().unwrap();
    let tracker = TaskTracker::default();
    let session = open_session(dir.path());
    let slug = add(dir.path(), &session, "Step one");
    assert!(
        handle_pre_tool_use(&tracker, &bash("git commit"), Some("s1"), dir.path())
            .contains("llmenv task start")
    );
    start_task(dir.path(), &slug, false).unwrap();
    let logs = crate::test_log_capture::capture_logs(|| {
        assert_eq!(
            handle_pre_tool_use(&tracker, &bash("git commit"), Some("s1"), dir.path()),
            ""
        );
    });
    assert!(
        !load(&state_path(dir.path(), "s1").unwrap())
            .unwrap()
            .0
            .commit_denied
    );
    assert!(
        !logs.contains("deny marker"),
        "a saved marker logs nothing: {logs}"
    );
}

#[test]
fn the_deny_switch_and_other_tools_pass_through() {
    let dir = TempDir::new().unwrap();
    let off = TaskTracker {
        enforce_commit: false,
        ..TaskTracker::default()
    };
    assert_eq!(
        handle_pre_tool_use(&off, &bash("git commit"), Some("s1"), dir.path()),
        ""
    );
    let on = TaskTracker::default();
    let edit =
        serde_json::json!({ "tool_name": "Edit", "tool_input": { "command": "git commit" } });
    assert_eq!(handle_pre_tool_use(&on, &edit, Some("s1"), dir.path()), "");
    // A payload without a session id cannot be checked, so the commit is denied (#2630).
    let no_session = handle_pre_tool_use(&on, &bash("git commit"), None, dir.path());
    assert!(no_session.contains("has no session id"));
    assert_eq!(
        handle_pre_tool_use(&on, &bash("git status"), None, dir.path()),
        ""
    );
}

#[test]
fn a_question_to_the_user_parks_the_task_in_progress() {
    let dir = TempDir::new().unwrap();
    let session = open_session(dir.path());
    let slug = add(dir.path(), &session, "Step one");
    start_task(dir.path(), &slug, false).unwrap();
    let tracker = TaskTracker::default();
    let ask = serde_json::json!({ "tool_name": "AskUserQuestion" });
    let text = handle_post_tool_use(&tracker, &ask, Some("s1"), dir.path());
    assert!(text.contains(&format!("llmenv task wait {slug}")), "{text}");
    assert!(
        text.contains(&format!("llmenv task start {slug}")),
        "{text}"
    );
    let stop = serde_json::json!({ "last_assistant_message": "Which one do you want?" });
    assert!(handle_stop(&tracker, &stop, Some("s1"), dir.path()).contains("llmenv task wait"));
    let no_question = serde_json::json!({ "last_assistant_message": "Done." });
    assert_eq!(
        handle_stop(&tracker, &no_question, Some("s1"), dir.path()),
        ""
    );
    let off = TaskTracker {
        nudges: false,
        ..TaskTracker::default()
    };
    assert_eq!(handle_stop(&off, &stop, Some("s1"), dir.path()), "");
}

#[test]
fn a_waiting_task_is_reported_as_waiting_on_the_user_at_a_question() {
    let dir = TempDir::new().unwrap();
    let session = open_session(dir.path());
    let slug = add(dir.path(), &session, "Step one");
    start_task(dir.path(), &slug, false).unwrap();
    crate::task::wait_task(dir.path(), &slug, "needs an answer").unwrap();
    let stop = serde_json::json!({ "last_assistant_message": "Ready?" });
    let text = handle_stop(&TaskTracker::default(), &stop, Some("s1"), dir.path());
    assert!(text.contains("waiting on the user: step-one"), "{text}");
}

#[test]
fn a_state_file_that_cannot_be_read_denies_a_commit_and_skips_the_nudge() {
    let dir = TempDir::new().unwrap();
    let tracker = TaskTracker {
        nudge_after: Some(1),
        ..TaskTracker::default()
    };
    let path = state_path(dir.path(), "s1").unwrap();
    // A directory in place of the file: reading fails with an error that is not NotFound.
    std::fs::create_dir_all(&path).unwrap();
    let edit = serde_json::json!({ "tool_name": "Edit" });
    assert_eq!(
        handle_post_tool_use(&tracker, &edit, Some("s1"), dir.path()),
        ""
    );
    let deny = handle_pre_tool_use(&tracker, &bash("git commit"), Some("s1"), dir.path());
    assert!(deny.starts_with("__DENY__:"));
    assert!(deny.contains("cannot be read"));
    assert!(deny.contains(&path.display().to_string()));
    std::fs::remove_dir(&path).unwrap();
    std::fs::write(&path, "not json").unwrap();
    let text = handle_post_tool_use(&tracker, &edit, Some("s1"), dir.path());
    assert!(text.contains("1 file edits"), "{text}");
}

#[test]
fn unsaved_state_deny_names_the_path_and_denies() {
    let deny = unsaved_state_deny(Path::new("/state/task_nudge/one.json"));
    assert!(deny.starts_with("__DENY__:"));
    assert!(deny.contains("/state/task_nudge/one.json cannot be saved"));
}

#[test]
fn state_dir_deny_covers_only_a_commit_with_the_gate_on() {
    let error = anyhow::anyhow!("HOME is not valid UTF-8");
    let on = TaskTracker::default();
    let off = TaskTracker {
        enforce_commit: false,
        ..TaskTracker::default()
    };
    let deny = state_dir_deny(&on, &bash("git commit -m x"), &error).unwrap();
    assert!(deny.starts_with("__DENY__:"));
    assert!(deny.contains("HOME is not valid UTF-8"));
    assert_eq!(state_dir_deny(&off, &bash("git commit"), &error), None);
    assert_eq!(state_dir_deny(&on, &bash("git status"), &error), None);
    let edit =
        serde_json::json!({ "tool_name": "Edit", "tool_input": { "command": "git commit" } });
    assert_eq!(state_dir_deny(&on, &edit, &error), None);
    // A call with no command is denied, as the main path denies it (#2536).
    let no_command = serde_json::json!({ "tool_name": "Bash" });
    let deny = state_dir_deny(&on, &no_command, &error).unwrap();
    assert!(deny.contains("tool_input.command"), "{deny}");
}

#[test]
fn an_unreadable_task_store_denies_a_commit_and_skips_the_nudge() {
    let dir = TempDir::new().unwrap();
    // A file where the task store directory belongs: the store cannot be read.
    std::fs::write(dir.path().join("tasks"), "x").unwrap();
    assert!(matches!(
        crate::task::tracking(dir.path(), Some("s1")),
        Tracking::Unknown(_)
    ));
    let tracker = TaskTracker {
        nudge_after: Some(1),
        ..TaskTracker::default()
    };
    let deny = handle_pre_tool_use(&tracker, &bash("git commit"), Some("s1"), dir.path());
    assert!(deny.starts_with("__DENY__:"));
    // No message argument: CodeQL reads the deny text as derived from the session id.
    assert!(
        deny.contains("task store cannot be read ("),
        "the deny names the cause"
    );
    let edit = serde_json::json!({ "tool_name": "Edit" });
    assert_eq!(
        handle_post_tool_use(&tracker, &edit, Some("s1"), dir.path()),
        ""
    );
    // The end of the turn says why the reminders did not run.
    let stop = handle_stop(&tracker, &serde_json::json!({}), Some("s1"), dir.path());
    assert!(stop.contains("task reminders did not run"), "{stop}");
}
