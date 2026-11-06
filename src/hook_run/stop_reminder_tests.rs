//! Stop-hook reminder tests: the tracker text and the critique share one reminder (#317).

use super::*;

// #317: the checklist is appended to whatever the Stop hook already had to
// say, and neither piece may swallow the other.
#[test]
fn stop_reminder_joins_the_tracker_text_and_the_critique() {
    let state_dir = tempfile::tempdir().expect("test");
    let with_critique = crate::config::Config {
        features: Some(crate::config::Features {
            slippage: Some(crate::config::SlippageControl {
                enabled: true,
                self_critique: true,
                ..Default::default()
            }),
            repeat_detect: Some(crate::config::RepeatDetect {
                enabled: false,
                threshold: 1,
            }),
            ..Default::default()
        }),
        ..Default::default()
    };
    let text = resolve_stop_reminder(
        state_dir.path(),
        Some("s1"),
        &with_critique,
        &serde_json::Value::Null,
    );
    assert!(text.contains("tests"), "the critique is present: {text:?}");
    assert!(
        !text.starts_with('\n'),
        "no separator before the first part: {text:?}"
    );

    let without = crate::config::Config {
        features: Some(crate::config::Features {
            repeat_detect: Some(crate::config::RepeatDetect {
                enabled: false,
                threshold: 1,
            }),
            ..Default::default()
        }),
        ..Default::default()
    };
    let plain = resolve_stop_reminder(
        state_dir.path(),
        Some("s1"),
        &without,
        &serde_json::Value::Null,
    );
    assert!(
        !plain.contains("tests"),
        "no critique when the layer is off: {plain:?}"
    );
    // The tracker's own text survives either way — appending must not
    // replace what was already there.
    assert!(
        text.starts_with(plain.as_str()) || plain.is_empty(),
        "the critique is appended, not substituted: {text:?} vs {plain:?}"
    );
}

// The separator only matters when the tracker actually said something, so
// an empty-tracker fixture can't tell a correct join from a missing or
// misplaced one. Seed a task first, then require both parts and a blank
// line between them.
/// Start a session that the conversation `conversation` owns, as `llmenv task session start`
/// does inside an engine.
fn start_owned_session(state_dir: &std::path::Path, project: &str, conversation: &str) {
    let owner = crate::task::session::EngineIdentity::from_session_id(Some(conversation));
    let request = crate::task::session::StartRequest {
        name: None,
        description: None,
        project,
        owner: &owner,
        resume: &crate::task::resume::ResumeContext::default(),
    };
    crate::task::session::start_session_as(
        state_dir,
        &request,
        crate::task::session::StartDecision::Auto,
    )
    .expect("test");
}

#[test]
fn stop_reminder_separates_tracker_text_from_the_critique() {
    let state_dir = tempfile::tempdir().expect("test");
    // The reminder only lists `wip` tasks belonging to the *current*
    // project, resolved from cwd — a fixture project string would be
    // filtered straight back out.
    let project = crate::task::project::current_tag().expect("test");
    start_owned_session(state_dir.path(), &project, "s1");
    let task = crate::task::add_task(
        state_dir.path(),
        "finish the parser",
        crate::task::ParentSpec::Detached,
        crate::task::SessionChoice::Resolve(
            &crate::task::session::EngineIdentity::from_session_id(Some("s1")),
        ),
        &project,
    )
    .expect("test");
    crate::task::start_task(state_dir.path(), &task.slug, false).expect("test");

    let config = crate::config::Config {
        features: Some(crate::config::Features {
            slippage: Some(crate::config::SlippageControl {
                enabled: true,
                self_critique: true,
                ..Default::default()
            }),
            repeat_detect: Some(crate::config::RepeatDetect {
                enabled: false,
                threshold: 1,
            }),
            ..Default::default()
        }),
        ..Default::default()
    };
    let text = resolve_stop_reminder(
        state_dir.path(),
        Some("s1"),
        &config,
        &serde_json::Value::Null,
    );
    let tracker_only =
        crate::task::stop_hook_reminder(&crate::task::Stored::read(state_dir.path()), None);
    assert!(
        !tracker_only.is_empty(),
        "fixture must produce tracker text, or this proves nothing"
    );
    assert!(
        text.contains(tracker_only.trim()),
        "tracker text kept: {text:?}"
    );
    assert!(text.contains("tests"), "critique kept: {text:?}");
    assert!(
        text.contains("\n\n"),
        "the two are separated by a blank line: {text:?}"
    );
    assert!(
        !text.starts_with('\n'),
        "no leading separator when the tracker already spoke: {text:?}"
    );
}

// #2456: a turn that ends with a question adds the waiting reminder after the tracker text,
// and adds it alone when the tracker said nothing.
#[test]
fn stop_reminder_appends_the_waiting_reminder_after_a_question() {
    let state_dir = tempfile::tempdir().expect("test");
    let project = crate::task::project::current_tag().expect("test");
    let config = crate::config::Config {
        features: Some(crate::config::Features {
            task_tracker: Some(crate::config::TaskTracker {
                enabled: true,
                ..Default::default()
            }),
            repeat_detect: Some(crate::config::RepeatDetect {
                enabled: false,
                threshold: 1,
            }),
            ..Default::default()
        }),
        ..Default::default()
    };
    let question = serde_json::json!({ "last_assistant_message": "Which one?" });
    // No task: nothing to say, and no stray separator.
    let empty = resolve_stop_reminder(state_dir.path(), Some("s1"), &config, &question);
    assert_eq!(empty, "");
    start_owned_session(state_dir.path(), &project, "s1");
    let task = crate::task::add_task(
        state_dir.path(),
        "finish the parser",
        crate::task::ParentSpec::Detached,
        crate::task::SessionChoice::Resolve(
            &crate::task::session::EngineIdentity::from_session_id(Some("s1")),
        ),
        &project,
    )
    .expect("test");
    crate::task::start_task(state_dir.path(), &task.slug, false).expect("test");
    let text = resolve_stop_reminder(state_dir.path(), Some("s1"), &config, &question);
    let tracker_only =
        crate::task::stop_hook_reminder(&crate::task::Stored::read(state_dir.path()), None);
    assert!(
        !tracker_only.is_empty(),
        "the fixture must produce tracker text"
    );
    let expected_tail = format!(
        "\n\nllmenv task tracker: you asked the user a question while '{}'",
        task.slug
    );
    assert!(text.starts_with(tracker_only.as_str()), "{text:?}");
    assert!(
        text[tracker_only.len()..].starts_with(&expected_tail),
        "{text:?}"
    );
    let statement = serde_json::json!({ "last_assistant_message": "Done." });
    let quiet = resolve_stop_reminder(state_dir.path(), Some("s1"), &config, &statement);
    assert_eq!(quiet, tracker_only);
}
