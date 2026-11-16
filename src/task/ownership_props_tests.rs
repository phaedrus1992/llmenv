//! Property tests of the session ownership rules (#3590): which conversation may add a task to a
//! session, and which parent a task may have.

use proptest::prelude::*;
use tempfile::TempDir;

use super::ownership::{check_child_parent, check_takes_tasks, ensure_parent_in_session};
use super::resume::ResumeContext;
use super::session::start_session_as;
use super::session::{EngineIdentity, Session, StartDecision, StartOutcome, StartRequest};
use super::session_choice::resolve_session_for_add;
use super::{ParentSpec, SessionChoice, Task, TaskState, add_task_for_session};

const PROJECT: &str = "props-project-0000";

fn conversation(id: Option<u8>) -> EngineIdentity {
    EngineIdentity::from_session_id(id.map(|n| format!("conv-{n}")).as_deref())
}

/// A new open session of `owner`, made through the real starter, which records the owner.
fn session_owned_by(dir: &std::path::Path, name: &str, owner: Option<u8>) -> Session {
    let identity = conversation(owner);
    let request = StartRequest {
        name: Some(name),
        description: None,
        project: PROJECT,
        owner: &identity,
        resume: &ResumeContext::default(),
    };
    match start_session_as(dir, &request, StartDecision::New).expect("start session") {
        StartOutcome::Created(session) => session,
        other => panic!("expected a new session, got {other:?}"),
    }
}

/// A task of `session`, made through the real writer and then given the state under test.
fn task_in(session: Option<u8>, state: TaskState) -> Task {
    let dir = TempDir::new().expect("tempdir");
    let mut task =
        add_task_for_session(dir.path(), "Parent", ParentSpec::Detached, "base").expect("add task");
    task.session = session.map(|n| format!("s{n}"));
    task.state = state;
    task
}

fn any_state() -> impl Strategy<Value = TaskState> {
    prop_oneof![
        Just(TaskState::Open),
        Just(TaskState::Wip),
        Just(TaskState::Waiting),
        Just(TaskState::Done),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]
    /// An owned session takes a task only from its owner. A session with no owner takes any
    /// task. The strict rule never allows what the loose reminder rule refuses (#3480).
    #[test]
    fn a_session_takes_a_task_only_from_its_owner(
        owner in proptest::option::of(0u8..3),
        caller in proptest::option::of(0u8..3),
    ) {
        let dir = TempDir::new().expect("tempdir");
        let session = session_owned_by(dir.path(), "owned", owner);
        let identity = conversation(caller);
        let want = match (owner, caller) {
            (None, _) => true,
            (Some(o), Some(c)) => o == c,
            (Some(_), None) => false,
        };
        prop_assert_eq!(check_takes_tasks(&session, &identity).is_ok(), want);
        if want {
            prop_assert!(session.visible_to(identity.session_id.as_deref()));
        }
    }

    /// A parent link is accepted only for a parent of the child's own session. A missing parent
    /// and a parent with no session are refused (#3480, #3593).
    #[test]
    fn a_parent_must_belong_to_the_session_of_the_child(
        parent_session in proptest::option::of(0u8..3),
        child_session in proptest::option::of(0u8..3),
        parent_present in proptest::bool::ANY,
    ) {
        let parent = task_in(parent_session, TaskState::Open);
        let child = child_session.map(|n| format!("s{n}"));
        let given = parent_present.then_some(&parent);
        let result = ensure_parent_in_session(given, &parent.slug, child.as_deref());
        let want = parent_present && parent_session.is_some() && parent_session == child_session;
        prop_assert_eq!(result.is_ok(), want, "{:?}", result);
    }

    /// A sub-task needs a parent that is not done and belongs to its own session.
    #[test]
    fn a_subtask_needs_an_unfinished_parent_of_its_own_session(
        parent_session in proptest::option::of(0u8..3),
        child_session in 0u8..3,
        state in any_state(),
    ) {
        let parent = task_in(parent_session, state);
        let child = format!("s{child_session}");
        let result = check_child_parent(&parent, &child);
        let want = state != TaskState::Done && parent_session == Some(child_session);
        prop_assert_eq!(result.is_ok(), want, "{:?}", result);
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(24))]
    /// The picker of a new task never returns a session that the caller may not add to, whatever
    /// the open sessions and the caller identity are.
    #[test]
    fn the_picker_never_returns_a_session_the_caller_may_not_use(
        owners in proptest::collection::vec(proptest::option::of(0u8..3), 0..5),
        caller in proptest::option::of(0u8..3),
    ) {
        let dir = TempDir::new().expect("tempdir");
        let mut made: Vec<Session> = Vec::new();
        for (i, owner) in owners.iter().enumerate() {
            made.push(session_owned_by(dir.path(), &format!("session {i}"), *owner));
        }
        let identity = conversation(caller);
        let picked = resolve_session_for_add(dir.path(), SessionChoice::Resolve(&identity), PROJECT);
        if let Ok(id) = &picked {
            let session = made.iter().find(|s| &s.id == id).expect("the id names an open session");
            prop_assert!(session.may_take_tasks(identity.session_id.as_deref()), "{}", id);
        }
        // One open session with no owner is always the answer.
        if let [None] = owners.as_slice() {
            let id = picked.expect("one open session with no owner is picked");
            prop_assert_eq!(id, made[0].id.clone());
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(24))]
    /// The caller of a pick is the picked session exactly when that session takes tasks from this
    /// conversation. Every other pick gives an unidentified caller that names the cause (#3743).
    #[test]
    fn the_caller_of_a_pick_is_the_picked_session_only_when_it_takes_the_task(
        owner in proptest::option::of(0u8..3),
        caller in proptest::option::of(0u8..3),
        pick_failed in proptest::bool::ANY,
    ) {
        let dir = TempDir::new().expect("tempdir");
        let session = session_owned_by(dir.path(), "picked", owner);
        let identity = conversation(caller);
        let pick = if pick_failed {
            Err(super::session::PickError::NoneOpen)
        } else {
            Ok(session.clone())
        };
        let got = super::ownership::caller_for_pick(&pick, &identity);
        let takes = session.may_take_tasks(identity.session_id.as_deref());
        match (pick_failed, takes) {
            (false, true) => prop_assert_eq!(got, super::ownership::Caller::Session(session.id)),
            (false, false) => {
                let super::ownership::Caller::Unidentified(why) = got else {
                    return Err(TestCaseError::fail("a refused pick must be unidentified"));
                };
                prop_assert!(why.contains("belongs to another conversation"), "{}", why);
                // The text names the missing conversation id only for a caller that has none.
                prop_assert_eq!(why.contains("CLAUDE_CODE_SESSION_ID"), caller.is_none(), "{}", why);
            }
            (true, _) => prop_assert!(
                matches!(got, super::ownership::Caller::Unidentified(ref why) if !why.is_empty()),
                "{:?}", got
            ),
        }
    }
}

proptest! {
    /// A command on a session passes only for the session of the caller, or for the named owner
    /// of that session. The note exists exactly for the second case and names the caller.
    #[test]
    fn a_session_command_passes_only_for_the_callers_session_or_the_named_owner(
        caller_session in proptest::option::of(0u8..3),
        session in 0u8..3,
        other in proptest::option::of(0u8..3),
    ) {
        use super::ownership::{Caller, ensure_session_is_callers, ensure_session_is_ours};
        let caller = match caller_session {
            Some(n) => Caller::Session(format!("s{n}")),
            None => Caller::Unidentified("no identity".to_string()),
        };
        let id = format!("s{session}");
        let other = other.map(|n| format!("s{n}"));
        let result = ensure_session_is_ours(&id, &caller, other.as_deref());
        let own = caller_session == Some(session);
        let named = other.as_deref() == Some(id.as_str());
        prop_assert_eq!(result.is_ok(), own || named, "{:?}", result);
        if let Ok(note) = result {
            // Own session: no note. Another session named by its id: a note naming the caller.
            prop_assert_eq!(note.text().is_some(), !own);
            if let Some(text) = note.text() {
                prop_assert!(text.contains(&caller.describe()), "{}", text);
            }
        }
        prop_assert_eq!(ensure_session_is_callers(&id, &caller).is_ok(), own);
    }
}
