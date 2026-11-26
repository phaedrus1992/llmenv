//! Tests of the idle-session Stop reminder (#2338, #2468, #2473, #3764).

use tempfile::TempDir;

use super::tests::{current_project, session_for_project};
use super::*;

fn idle_session_in_project(dir: &Path, project: &str) -> (String, Task, Task) {
    let session_id = session_for_project(dir, project);
    let first =
        add_task_for_session(dir, "Step 1", ParentSpec::Detached, &session_id).expect("test");
    let second =
        add_task_for_session(dir, "Step 2", ParentSpec::Detached, &session_id).expect("test");
    (session_id, first, second)
}

#[test]
fn stop_hook_reminder_flags_open_tasks_when_none_is_wip() {
    let dir = TempDir::new().expect("test");
    let (session_id, first, second) = idle_session_in_project(dir.path(), &current_project());

    let reminder = stop_hook_reminder(&Stored::read(dir.path()), None, TurnEnd::Statement);
    assert!(
        reminder.contains(&session_id),
        "must name the session: {reminder}"
    );
    assert!(
        reminder.contains(&format!("llmenv task start {}", first.slug)),
        "must name the next open task to start: {reminder}"
    );
    assert!(
        !reminder.contains(&second.slug),
        "names the next task only: {reminder}"
    );
    assert!(reminder.contains("2 open task(s)"), "{reminder}");
    assert!(
        !reminder.contains("session finish"),
        "a session with open tasks must not be offered `session finish` (#2416): {reminder}"
    );
    assert!(
        reminder.contains("recognize"),
        "must not presume ownership: {reminder}"
    );
}

// #3764: an idle session whose only open task waits on an undone blocker names the reason, and
// it does not advise a start that the start refuses.
#[test]
fn an_idle_reminder_names_the_unmet_blocker_and_not_a_start() {
    let dir = TempDir::new().expect("test");
    let session_id = session_for_project(dir.path(), &current_project());
    let waiter = add_task_for_session(dir.path(), "Waiter", ParentSpec::Detached, &session_id)
        .expect("test");
    let mut blocker = add_task_for_session(
        dir.path(),
        "Elsewhere",
        ParentSpec::Detached,
        "other-session",
    )
    .expect("test");
    blocker.state = TaskState::Wip;
    save_task(dir.path(), &blocker).expect("test");
    block_task(dir.path(), &waiter.slug, &blocker.slug).expect("test");

    let reminder = stop_hook_reminder(&Stored::read(dir.path()), None, TurnEnd::Statement);
    assert!(
        reminder.contains(&format!("'{}' cannot start", waiter.slug)),
        "{reminder}"
    );
    assert!(
        reminder.contains(&format!("waits on '{}'", blocker.slug)),
        "{reminder}"
    );
    assert!(
        !reminder.contains(&format!("llmenv task start {}", waiter.slug)),
        "{reminder}"
    );
}

// #2608: an unreadable store must say so in the reminder, not print that no session is open.
#[test]
fn unreadable_store_reminder_warns_instead_of_reporting_no_work() {
    let dir = TempDir::new().expect("test");
    std::fs::write(dir.path().join("tasks"), "x").expect("test");
    let stored = Stored::read(dir.path());
    let reminder = idle_reminder_lines(&current_project(), None, &stored, TurnEnd::Statement);
    assert!(reminder.contains("task store cannot be read"), "{reminder}");
    assert!(!reminder.contains("has no tasks"), "{reminder}");
}

// #2468: the first idle reminder must already offer `wait`, for a step that needs the user.
#[test]
fn first_idle_reminder_offers_start_and_wait() {
    let dir = TempDir::new().expect("test");
    let (_, first, _) = idle_session_in_project(dir.path(), &current_project());

    let reminder = stop_hook_reminder(&Stored::read(dir.path()), None, TurnEnd::Statement);
    let start = format!("llmenv task start {}", first.slug);
    let wait = format!("llmenv task wait {} \"<reason>\"", first.slug);
    assert!(reminder.contains(&start), "{reminder}");
    assert!(reminder.contains(&wait), "{reminder}");
    assert!(
        reminder.contains("needs the user"),
        "must say when `wait` applies: {reminder}"
    );
}

// #2473: after a question to the user the `wait` command comes first, and otherwise the `start`
// command comes first.
#[test]
fn idle_reminder_leads_with_wait_after_a_question_and_with_start_otherwise() {
    let dir = TempDir::new().expect("test");
    let (_, first, _) = idle_session_in_project(dir.path(), &current_project());
    let start = format!("llmenv task start {}", first.slug);
    let wait = format!("llmenv task wait {} \"<reason>\"", first.slug);

    let after_question = stop_hook_reminder(&Stored::read(dir.path()), None, TurnEnd::Question);
    assert!(after_question.contains(&start), "{after_question}");
    assert!(
        after_question.find(&wait) < after_question.find(&start),
        "`wait` must come first after a question: {after_question}"
    );

    let after_statement = stop_hook_reminder(&Stored::read(dir.path()), None, TurnEnd::Statement);
    assert!(after_statement.contains(&wait), "{after_statement}");
    assert!(
        after_statement.find(&start) < after_statement.find(&wait),
        "`start` must come first otherwise: {after_statement}"
    );
}

proptest::proptest! {
    /// A message is a question exactly when its text, without trailing whitespace, ends with `?`.
    #[test]
    fn a_message_is_a_question_iff_it_ends_with_a_question_mark(s in "\\PC{0,60}") {
        let expected = if s.trim_end().ends_with('?') {
            TurnEnd::Question
        } else {
            TurnEnd::Statement
        };
        proptest::prop_assert_eq!(TurnEnd::of(Some(&s)), expected);
    }

    /// Trailing whitespace never changes the answer, and a trailing mark always makes a question.
    #[test]
    fn trailing_whitespace_does_not_change_the_end(s in "\\PC{0,60}", ws in "[ \t\n\r]{0,5}") {
        proptest::prop_assert_eq!(TurnEnd::of(Some(&format!("{s}{ws}"))), TurnEnd::of(Some(&s)));
        proptest::prop_assert_eq!(TurnEnd::of(Some(&format!("{s}?{ws}"))), TurnEnd::Question);
    }
}

#[test]
fn turn_end_reads_a_trailing_question_mark_after_trailing_whitespace() {
    assert_eq!(TurnEnd::of(Some("Which one?\n")), TurnEnd::Question);
    assert_eq!(TurnEnd::of(Some("Done.")), TurnEnd::Statement);
    assert_eq!(TurnEnd::of(Some("")), TurnEnd::Statement);
    assert_eq!(TurnEnd::of(None), TurnEnd::Statement);
}

#[test]
fn stop_hook_reminder_skips_idle_check_for_session_with_wip_task() {
    let dir = TempDir::new().expect("test");
    let (session_id, first, second) = idle_session_in_project(dir.path(), &current_project());
    start_task(dir.path(), &first.slug, false).expect("test");

    let reminder = stop_hook_reminder(&Stored::read(dir.path()), None, TurnEnd::Statement);
    assert!(
        reminder.contains(&first.slug),
        "wip reminder still fires: {reminder}"
    );
    assert!(!reminder.contains(&second.slug), "{reminder}");
    assert!(!reminder.contains("none in progress"), "{reminder}");
    assert!(reminder.contains(&session_id));
}

#[test]
fn stop_hook_reminder_skips_idle_check_for_session_with_waiting_task() {
    let dir = TempDir::new().expect("test");
    let (_, first, second) = idle_session_in_project(dir.path(), &current_project());
    start_task(dir.path(), &first.slug, false).expect("test");
    wait_task(dir.path(), &first.slug, "spec review").expect("test");

    // A `waiting` task means the session is paused on purpose; stay quiet.
    let reminder = stop_hook_reminder(&Stored::read(dir.path()), None, TurnEnd::Statement);
    assert!(!reminder.contains(&second.slug), "{reminder}");
}

#[test]
fn stop_hook_reminder_skips_idle_session_from_other_project() {
    let dir = TempDir::new().expect("test");
    let (_, first, _) = idle_session_in_project(dir.path(), "other-project-0000000000");
    assert!(
        !stop_hook_reminder(&Stored::read(dir.path()), None, TurnEnd::Statement)
            .contains(&first.slug)
    );
}

#[test]
fn idle_sessions_name_the_obstacle_when_a_blocker_and_the_queue_refuse_every_task() {
    let dir = TempDir::new().expect("test");
    let project = current_project();
    let (session_id, first, second) = idle_session_in_project(dir.path(), &project);
    block_task(dir.path(), &first.slug, &second.slug).expect("test");

    let open = session::open_sessions_strict(dir.path(), &project).expect("test");
    let idle = idle_sessions(open, &list_tasks(dir.path()));
    assert_eq!(idle.len(), 1);
    assert_eq!(idle[0].session.id, session_id);
    // The first task waits on the second, and the queue holds the second behind the first, so
    // the start refuses both. The reminder keeps the first task and names why it cannot
    // start (#3770), where it used to advise a start of the second that the queue refused.
    assert!(idle[0].next.is_none());
    let (slug, why) = idle[0].loop_error.as_ref().expect("an obstacle is named");
    assert_eq!(slug, &first.slug);
    assert!(
        why.contains(&format!("waits on '{}'", second.slug)),
        "{why}"
    );
    assert_eq!(idle[0].open_count, 2);
}
