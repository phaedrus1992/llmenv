use super::*;
use serde_json::json;

const PROJECT: &str = "proj-x";

/// Two open sessions of two conversations, with one task in each. Returns the slugs, mine first.
fn two_conversations(dir: &Path) -> (String, String) {
    let start = |name: &str, conv: &str| {
        let owner = session::EngineIdentity::from_session_id(Some(conv));
        let request = session::StartRequest {
            name: Some(name),
            description: None,
            project: PROJECT,
            owner: &owner,
            resume: &crate::task::resume::ResumeContext::default(),
        };
        match session::start_session_as(dir, &request, session::StartDecision::New).unwrap() {
            session::StartOutcome::Created(s) => s.id,
            other => panic!("expected Created, got {other:?}"),
        }
    };
    let theirs = start("theirs", "conv-other");
    let mine = start("mine", "conv-me");
    let add = |title: &str, session: &str| {
        task::add_task_for_session(dir, title, task::ParentSpec::Detached, session)
            .unwrap()
            .slug
    };
    (add("my step", &mine), add("their step", &theirs))
}

/// True when the task has no progress note. `Task` keeps its notes private, so the test reads
/// the saved JSON.
fn no_notes(task: &task::Task) -> bool {
    serde_json::to_value(task).unwrap()["notes"]
        .as_array()
        .is_none_or(Vec::is_empty)
}

fn update_as(dir: &Path, conv: &str, input: &Value) -> String {
    let owner = session::EngineIdentity::from_session_id(Some(conv));
    update(Some(input), dir, PROJECT, &owner)
}

#[test]
fn a_start_of_a_task_of_another_conversation_is_refused_and_saves_no_note() {
    let dir = tempfile::tempdir().unwrap();
    let (_, theirs) = two_conversations(dir.path());
    let out = update_as(
        dir.path(),
        "conv-me",
        &json!({ "taskId": theirs, "status": "in_progress", "description": "mine now" }),
    );
    assert!(
        out.contains(&format!("couldn't update task '{theirs}'")),
        "{out}"
    );
    assert!(out.contains("belongs to session"), "{out}");
    assert!(out.contains("--other-session"), "{out}");
    assert!(!out.contains("note saved"), "{out}");
    let task = task::load_task(dir.path(), &theirs).unwrap();
    assert_eq!(task.state, task::TaskState::Open);
    assert!(no_notes(&task), "{task:?}");
}

#[test]
fn a_delete_of_a_task_of_another_conversation_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let (_, theirs) = two_conversations(dir.path());
    let out = update_as(
        dir.path(),
        "conv-me",
        &json!({ "taskId": theirs, "status": "deleted" }),
    );
    assert!(out.contains("belongs to session"), "{out}");
    assert!(task::load_task(dir.path(), &theirs).is_ok());
}

#[test]
fn a_status_free_update_of_a_task_of_another_conversation_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let (_, theirs) = two_conversations(dir.path());
    let out = update_as(
        dir.path(),
        "conv-me",
        &json!({ "taskId": theirs, "description": "a note" }),
    );
    assert!(out.contains("belongs to session"), "{out}");
    assert!(no_notes(&task::load_task(dir.path(), &theirs).unwrap()));
}

#[test]
fn the_conversation_that_owns_the_task_can_start_it() {
    let dir = tempfile::tempdir().unwrap();
    let (mine, _) = two_conversations(dir.path());
    let out = update_as(
        dir.path(),
        "conv-me",
        &json!({ "taskId": mine, "status": "in_progress", "description": "begun" }),
    );
    assert!(out.contains(&format!("started '{mine}'")), "{out}");
    assert!(out.contains(&format!("note saved to '{mine}'")), "{out}");
    assert_eq!(
        task::load_task(dir.path(), &mine).unwrap().state,
        task::TaskState::Wip
    );
}

#[test]
fn a_task_with_no_session_is_never_refused() {
    let dir = tempfile::tempdir().unwrap();
    two_conversations(dir.path());
    let loose = task::add_task_for_session(dir.path(), "loose", task::ParentSpec::Detached, "gone")
        .unwrap();
    let mut untagged = task::load_task(dir.path(), &loose.slug).unwrap();
    untagged.session = None;
    task::save_task(dir.path(), &untagged).unwrap();
    let out = update_as(
        dir.path(),
        "conv-me",
        &json!({ "taskId": loose.slug, "status": "in_progress" }),
    );
    assert!(out.contains(&format!("started '{}'", loose.slug)), "{out}");
}
