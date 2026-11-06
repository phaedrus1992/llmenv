//! #3185: a task of a session that another conversation owns is not this conversation's work.

use super::*;
use crate::task::session::{EngineIdentity, StartRequest, start_session_as};

fn open_owned_session(dir: &Path, conversation: &str) -> String {
    let owner = EngineIdentity::from_session_id(Some(conversation));
    let request = StartRequest {
        name: Some("work"),
        description: None,
        project: &project(),
        owner: &owner,
        resume: &crate::task::resume::ResumeContext::default(),
    };
    let StartOutcome::Created(s) = start_session_as(dir, &request, StartDecision::Auto).unwrap()
    else {
        panic!("expected Created");
    };
    s.id
}

fn wip_task_owned_by(dir: &Path, conversation: &str) -> String {
    let session = open_owned_session(dir, conversation);
    let slug = add(dir, &session, "Step one");
    start_task(dir, &slug, false).unwrap();
    slug
}

#[test]
fn a_question_does_not_name_a_task_of_another_conversation() {
    let dir = TempDir::new().unwrap();
    wip_task_owned_by(dir.path(), "other");
    let stop = serde_json::json!({ "last_assistant_message": "Which one do you want?" });
    let text = handle_stop_read(&TaskTracker::default(), &stop, Some("s1"), dir.path());
    assert_eq!(text, "");
}

#[test]
fn a_question_names_a_task_of_this_conversation() {
    let dir = TempDir::new().unwrap();
    let slug = wip_task_owned_by(dir.path(), "s1");
    let stop = serde_json::json!({ "last_assistant_message": "Which one do you want?" });
    let text = handle_stop_read(&TaskTracker::default(), &stop, Some("s1"), dir.path());
    assert!(text.contains(&format!("llmenv task wait {slug}")), "{text}");
}

#[test]
fn ask_user_question_does_not_name_a_task_of_another_conversation() {
    let dir = TempDir::new().unwrap();
    wip_task_owned_by(dir.path(), "other");
    let ask = serde_json::json!({ "tool_name": "AskUserQuestion" });
    let text = handle_post_tool_use(&TaskTracker::default(), &ask, Some("s1"), dir.path());
    assert!(!text.contains("llmenv task wait"), "{text}");
}

#[test]
fn a_task_of_another_conversation_does_not_count_as_tracked_work() {
    let dir = TempDir::new().unwrap();
    let slug = wip_task_owned_by(dir.path(), "other");
    assert_eq!(
        crate::task::tracking(dir.path(), Some("s1")),
        Tracking::NoSession
    );
    assert_eq!(
        crate::task::tracking(dir.path(), Some("other")),
        Tracking::Tracked {
            wip: Some(slug),
            wip_ownerless: false,
            waiting: Vec::new(),
            next: None,
            queued: 0,
        }
    );
}

/// #3187: a task of a session with no recorded owner is named once per conversation, at its first
/// question, by the Stop reminder and the AskUserQuestion reminder alike.
#[test]
fn an_ownerless_task_is_named_once_per_conversation() {
    let dir = TempDir::new().unwrap();
    let slug = wip_task_owned_by_nobody(dir.path());
    let stop = serde_json::json!({ "last_assistant_message": "Which one do you want?" });
    let ask = serde_json::json!({ "tool_name": "AskUserQuestion" });
    let tracker = TaskTracker::default();
    let wait = format!("llmenv task wait {slug}");

    let first = handle_stop_read(&tracker, &stop, Some("s1"), dir.path());
    assert!(first.contains(&wait), "{first}");
    let second = handle_stop_read(&tracker, &stop, Some("s1"), dir.path());
    assert!(!second.contains(&wait), "{second}");
    let third = handle_post_tool_use(&tracker, &ask, Some("s1"), dir.path());
    assert!(!third.contains(&wait), "{third}");

    let other = handle_post_tool_use(&tracker, &ask, Some("s2"), dir.path());
    assert!(
        other.contains(&wait),
        "a second conversation still hears once: {other}"
    );
}

/// A task of this conversation's own session keeps the reminder at each question.
#[test]
fn an_owned_task_is_named_at_each_question() {
    let dir = TempDir::new().unwrap();
    let slug = wip_task_owned_by(dir.path(), "s1");
    let stop = serde_json::json!({ "last_assistant_message": "Which one do you want?" });
    let wait = format!("llmenv task wait {slug}");
    for _ in 0..2 {
        let text = handle_stop_read(&TaskTracker::default(), &stop, Some("s1"), dir.path());
        assert!(text.contains(&wait), "{text}");
    }
}

fn wip_task_owned_by_nobody(dir: &Path) -> String {
    let owner = EngineIdentity::from_session_id(None);
    let request = StartRequest {
        name: Some("ownerless"),
        description: None,
        project: &project(),
        owner: &owner,
        resume: &crate::task::resume::ResumeContext::default(),
    };
    let StartOutcome::Created(s) = start_session_as(dir, &request, StartDecision::Auto).unwrap()
    else {
        panic!("expected Created");
    };
    let slug = add(dir, &s.id, "Ownerless step");
    start_task(dir, &slug, false).unwrap();
    slug
}

proptest::proptest! {
    #![proptest_config(proptest::prelude::ProptestConfig::with_cases(16))]
    /// Over any order of questions from two conversations, each conversation hears about an
    /// ownerless task once: at its first question, and never after it (#3187).
    #[test]
    fn each_conversation_hears_once_in_any_order(
        events in proptest::collection::vec((proptest::bool::ANY, proptest::bool::ANY), 1..8),
    ) {
        let dir = TempDir::new().unwrap();
        let slug = wip_task_owned_by_nobody(dir.path());
        let wait = format!("llmenv task wait {slug}");
        let tracker = TaskTracker::default();
        let mut heard = std::collections::BTreeMap::new();
        for (second, by_tool) in &events {
            let caller = if *second { "s2" } else { "s1" };
            let text = if *by_tool {
                let ask = serde_json::json!({ "tool_name": "AskUserQuestion" });
                handle_post_tool_use(&tracker, &ask, Some(caller), dir.path())
            } else {
                let stop = serde_json::json!({ "last_assistant_message": "Which one?" });
                handle_stop_read(&tracker, &stop, Some(caller), dir.path())
            };
            let first = !heard.contains_key(caller);
            proptest::prop_assert_eq!(text.contains(&wait), first, "{} {}", caller, text);
            heard.insert(caller, ());
        }
    }
}
