//! #3318: a step boundary and a turn end both remind the agent when a task is left in progress
//! with tasks queued behind it.

use super::*;

/// One step in progress, and a second step queued behind it. Returns the in-progress slug.
fn wip_with_queue(dir: &Path) -> String {
    let session = open_session(dir);
    let one = add(dir, &session, "Step one");
    add(dir, &session, "Step two");
    start_task(dir, &one, false).unwrap();
    one
}

#[test]
fn a_push_with_a_stale_wip_task_and_a_queue_is_reminded_once() {
    let dir = TempDir::new().unwrap();
    wip_with_queue(dir.path());
    let tracker = TaskTracker::default();
    let push = || {
        handle_post_tool_use(
            &tracker,
            &bash("git push origin HEAD"),
            Some("s1"),
            dir.path(),
        )
    };
    let first = push();
    assert!(first.contains("step-one"), "{first}");
    assert!(first.contains("git push"), "{first}");
    assert!(first.contains("llmenv task done step-one"), "{first}");
    assert!(first.contains("llmenv task start step-two"), "{first}");
    // The same task and the same boundary stay quiet on the next push.
    assert_eq!(push(), "");
}

#[test]
fn a_monitor_started_push_gets_the_boundary_reminder() {
    let dir = TempDir::new().unwrap();
    wip_with_queue(dir.path());
    let payload = serde_json::json!({
        "tool_name": "Monitor",
        "tool_input": { "command": "git push origin HEAD" },
    });
    let text = handle_post_tool_use(&TaskTracker::default(), &payload, Some("s1"), dir.path());
    assert!(text.contains("llmenv task done step-one"), "{text}");
}

#[test]
fn a_pull_request_step_is_reminded_apart_from_a_push() {
    let dir = TempDir::new().unwrap();
    wip_with_queue(dir.path());
    let tracker = TaskTracker::default();
    let push = handle_post_tool_use(&tracker, &bash("git push"), Some("s1"), dir.path());
    assert!(push.contains("git push"), "{push}");
    let merge = handle_post_tool_use(
        &tracker,
        &bash("gh pr merge 3 --merge"),
        Some("s1"),
        dir.path(),
    );
    assert!(merge.contains("gh pr merge"), "{merge}");
}

#[test]
fn a_boundary_with_no_queued_task_is_quiet() {
    let dir = TempDir::new().unwrap();
    let session = open_session(dir.path());
    let one = add(dir.path(), &session, "Step one");
    start_task(dir.path(), &one, false).unwrap();
    let text = handle_post_tool_use(
        &TaskTracker::default(),
        &bash("git push"),
        Some("s1"),
        dir.path(),
    );
    assert_eq!(text, "");
}

#[test]
fn a_command_that_is_no_step_boundary_is_quiet() {
    let dir = TempDir::new().unwrap();
    wip_with_queue(dir.path());
    let text = handle_post_tool_use(
        &TaskTracker::default(),
        &bash("gh pr view 3"),
        Some("s1"),
        dir.path(),
    );
    assert_eq!(text, "");
}

#[test]
fn a_stalled_turn_end_is_reminded_first_and_then_every_n_turn_ends() {
    let dir = TempDir::new().unwrap();
    wip_with_queue(dir.path());
    let tracker = TaskTracker {
        stop_nudge_every: Some(3),
        ..TaskTracker::default()
    };
    let turn_end = || handle_stop_read(&tracker, &serde_json::json!({}), Some("s1"), dir.path());
    let reminded = |text: &str| text.contains("'step-one' is in progress");
    assert!(
        reminded(&turn_end()),
        "the first stalled turn end is reminded"
    );
    assert!(!reminded(&turn_end()));
    assert!(!reminded(&turn_end()));
    assert!(
        reminded(&turn_end()),
        "the fourth turn end is reminded again"
    );
}

#[test]
fn a_stall_that_ends_restarts_the_stop_throttle() {
    let dir = TempDir::new().unwrap();
    let session = open_session(dir.path());
    let one = add(dir.path(), &session, "Step one");
    add(dir.path(), &session, "Step two");
    add(dir.path(), &session, "Step three");
    start_task(dir.path(), &one, false).unwrap();
    let tracker = TaskTracker {
        stop_nudge_every: Some(10),
        ..TaskTracker::default()
    };
    let turn_end = || handle_stop_read(&tracker, &serde_json::json!({}), Some("s1"), dir.path());
    assert!(turn_end().contains("'step-one' is in progress"));
    assert_eq!(turn_end(), "", "the throttle holds inside its window");
    // The wip task ends, and a turn end sees no stall, so the throttle restarts. Step three stays
    // queued behind step two, so the next stall is reminded at once, not after the window.
    done_task(dir.path(), &one).unwrap();
    assert!(turn_end().contains("llmenv task start step-two"));
    start_task(dir.path(), "step-two", false).unwrap();
    assert!(turn_end().contains("'step-two' is in progress"));
}

#[test]
fn a_wip_task_with_an_empty_queue_gets_no_stop_reminder() {
    let dir = TempDir::new().unwrap();
    let session = open_session(dir.path());
    let one = add(dir.path(), &session, "Step one");
    start_task(dir.path(), &one, false).unwrap();
    let text = handle_stop_read(
        &TaskTracker::default(),
        &serde_json::json!({}),
        Some("s1"),
        dir.path(),
    );
    assert_eq!(text, "");
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(16))]

    /// However many pushes repeat, the same task and boundary reminds once.
    #[test]
    fn a_repeated_boundary_reminds_once(pushes in 1usize..6) {
        let dir = TempDir::new().unwrap();
        wip_with_queue(dir.path());
        let tracker = TaskTracker::default();
        let fired = (0..pushes)
            .filter(|_| {
                !handle_post_tool_use(&tracker, &bash("git push origin HEAD"), Some("s1"), dir.path())
                    .is_empty()
            })
            .count();
        prop_assert_eq!(fired, 1);
    }

    /// The Stop reminder fires on turn end 1, then every `every` turn ends.
    #[test]
    fn the_stop_throttle_reminds_at_every_nth_turn_end(every in 1u32..6, turns in 1usize..14) {
        let dir = TempDir::new().unwrap();
        wip_with_queue(dir.path());
        let tracker = TaskTracker {
            stop_nudge_every: Some(every),
            ..TaskTracker::default()
        };
        let fired = (0..turns)
            .filter(|_| {
                !handle_stop_read(&tracker, &serde_json::json!({}), Some("s1"), dir.path()).is_empty()
            })
            .count();
        prop_assert_eq!(fired, turns.div_ceil(every as usize));
    }
}
