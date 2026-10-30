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
    let text = handle_stop(&TaskTracker::default(), &stop, Some("s1"), dir.path());
    assert_eq!(text, "");
}

#[test]
fn a_question_names_a_task_of_this_conversation() {
    let dir = TempDir::new().unwrap();
    let slug = wip_task_owned_by(dir.path(), "s1");
    let stop = serde_json::json!({ "last_assistant_message": "Which one do you want?" });
    let text = handle_stop(&TaskTracker::default(), &stop, Some("s1"), dir.path());
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
            waiting: Vec::new(),
            next: None
        }
    );
}
