use super::super::resume::ResumeContext;
use super::super::session::{EngineIdentity, StartDecision, StartOutcome, start_session};
use super::super::session::{StartRequest, start_session_as};
use super::super::{ParentSpec, SessionChoice, TaskState, add_task, load_task, save_task};
use super::{
    Caller, OverrideNote, SessionOverride, caller_session, ensure_parent_in_session,
    ensure_session_is_ours, ensure_tasks_are_ours, read_for_check,
};

/// The check for one name, as the commands with one id run it.
fn ensure_task_is_ours(
    state_dir: &std::path::Path,
    input: &str,
    caller: &Caller,
    other_session: Option<&str>,
) -> anyhow::Result<Option<OverrideNote>> {
    let tasks = read_for_check(state_dir, &[input])?;
    Ok(
        ensure_tasks_are_ours(state_dir, &tasks, &[input], caller, other_session)?
            .pop()
            .and_then(|(_, note)| note),
    )
}
use tempfile::TempDir;

const PROJECT: &str = "ownership-test";

fn open_task_in_session(dir: &std::path::Path) -> (String, String) {
    let outcome =
        start_session(dir, Some("a"), None, PROJECT, StartDecision::Auto).expect("start session");
    let StartOutcome::Created(session) = outcome else {
        panic!("expected a new session");
    };
    let task = add_task(
        dir,
        "Owned step",
        ParentSpec::Detached,
        SessionChoice::Named {
            id: &session.id,
            owner: &EngineIdentity::from_session_id(None),
            other_session: None,
        },
        PROJECT,
    )
    .expect("add task");
    (task.slug, session.id)
}

fn conversation(id: &str) -> EngineIdentity {
    EngineIdentity {
        session_id: Some(id.to_string()),
        pid: None,
    }
}

#[test]
fn other_sessions_task_is_refused_and_left_unchanged() {
    let dir = TempDir::new().expect("tempdir");
    let (slug, owner) = open_task_in_session(dir.path());
    let caller = Caller::Session("some-other-session".to_string());

    let err = ensure_task_is_ours(dir.path(), &slug, &caller, None)
        .expect_err("a task of another session must be refused");

    let msg = format!("{err:#}");
    assert!(
        msg.contains(&owner),
        "refusal must name the owning session: {msg}"
    );
    assert!(
        msg.contains("--other-session"),
        "refusal must name the flag: {msg}"
    );
    let task = load_task(dir.path(), &slug).expect("load");
    assert_eq!(
        task.state,
        TaskState::Open,
        "a refused task must not change"
    );
}

#[test]
fn unidentified_caller_is_refused_and_the_reason_is_named() {
    let dir = TempDir::new().expect("tempdir");
    let (slug, _owner) = open_task_in_session(dir.path());
    let caller = Caller::Unidentified("no session of yours is open in this project".into());

    let err = ensure_task_is_ours(dir.path(), &slug, &caller, None)
        .expect_err("an unidentified caller must not act on a session task");

    assert!(
        format!("{err:#}").contains("no session of yours"),
        "{err:#}"
    );
}

/// Several names resolve from one read, in order, to the slugs that the command then acts
/// on (#3477).
#[test]
fn several_names_resolve_in_order_to_the_checked_slugs() {
    let dir = TempDir::new().expect("tempdir");
    let (slug, owner) = open_task_in_session(dir.path());
    let prefix = &slug[..slug.len() - 1];
    let names = [slug.as_str(), prefix];
    let tasks = read_for_check(dir.path(), &names).expect("read");
    let checked = ensure_tasks_are_ours(dir.path(), &tasks, &names, &Caller::Session(owner), None)
        .expect("the owning session must pass");
    let slugs: Vec<&str> = checked.iter().map(|(s, _)| s.as_str()).collect();
    assert_eq!(slugs, [slug.as_str(), slug.as_str()]);
}

#[test]
fn owning_session_may_act_on_its_own_task() {
    let dir = TempDir::new().expect("tempdir");
    let (slug, owner) = open_task_in_session(dir.path());

    ensure_task_is_ours(dir.path(), &slug, &Caller::Session(owner), None)
        .expect("the owning session must pass");
}

#[test]
fn other_session_naming_the_owner_lifts_the_refusal() {
    let dir = TempDir::new().expect("tempdir");
    let (slug, owner) = open_task_in_session(dir.path());
    let caller = Caller::Unidentified("no session".into());

    ensure_task_is_ours(dir.path(), &slug, &caller, Some(&owner))
        .expect("--other-session with the owner's id must allow the action");
}

#[test]
fn other_session_naming_another_session_is_refused() {
    let dir = TempDir::new().expect("tempdir");
    let (slug, owner) = open_task_in_session(dir.path());
    let caller = Caller::Session("some-other-session".to_string());

    let err = ensure_task_is_ours(dir.path(), &slug, &caller, Some("wrong-owner"))
        .expect_err("a non-owner id must not lift the refusal");

    let msg = format!("{err:#}");
    assert!(msg.contains(&owner), "must name the real owner: {msg}");
    assert!(msg.contains("wrong-owner"), "must name the given id: {msg}");
    let task = load_task(dir.path(), &slug).expect("load");
    assert!(
        task.notes.is_empty(),
        "a refused override must leave no note"
    );
}

#[test]
fn other_session_override_notes_the_caller_only_after_record() {
    let dir = TempDir::new().expect("tempdir");
    let (slug, owner) = open_task_in_session(dir.path());
    let caller = Caller::Session("some-other-session".to_string());

    let pending = ensure_task_is_ours(dir.path(), &slug, &caller, Some(&owner))
        .expect("the named owner must allow the action")
        .expect("an override returns the note to record");
    assert!(
        load_task(dir.path(), &slug).expect("load").notes.is_empty(),
        "the guard alone must not write a note"
    );

    pending.record(dir.path()).expect("record");

    let task = load_task(dir.path(), &slug).expect("load");
    let noted = task
        .notes
        .iter()
        .any(|n| n.text.contains("--other-session") && n.text.contains("some-other-session"));
    assert!(noted, "the override must note the caller: {:?}", task.notes);
}

#[test]
fn task_without_a_session_is_never_refused() {
    let dir = TempDir::new().expect("tempdir");
    let (slug, _owner) = open_task_in_session(dir.path());
    // Clear the tag directly: a task added outside any session has no owner.
    let mut task = load_task(dir.path(), &slug).expect("load");
    task.session = None;
    save_task(dir.path(), &task).expect("save");

    ensure_task_is_ours(
        dir.path(),
        &slug,
        &Caller::Unidentified("none".into()),
        None,
    )
    .expect("a session-less task must not be refused");
}

#[test]
fn session_owned_by_another_conversation_is_not_the_callers() {
    let dir = TempDir::new().expect("tempdir");
    let project_owner = conversation("conv-a");
    let request = StartRequest {
        name: Some("a"),
        description: None,
        project: PROJECT,
        owner: &project_owner,
        resume: &ResumeContext::default(),
    };
    start_session_as(dir.path(), &request, StartDecision::Auto).expect("start");

    let caller = caller_session(dir.path(), PROJECT, &conversation("conv-b")).expect("caller");

    assert!(
        matches!(&caller, Caller::Unidentified(reason) if reason.contains("another conversation")),
        "conv-b must not inherit conv-a's only session: {caller:?}"
    );
}

#[test]
fn a_caller_with_no_id_is_refused_for_the_only_owned_session() {
    let dir = TempDir::new().expect("tempdir");
    let project_owner = conversation("conv-a");
    let request = StartRequest {
        name: Some("owned"),
        description: None,
        project: PROJECT,
        owner: &project_owner,
        resume: &ResumeContext::default(),
    };
    start_session_as(dir.path(), &request, StartDecision::Auto).expect("start");

    let caller = caller_session(dir.path(), PROJECT, &EngineIdentity::default()).expect("ok");

    assert!(
        matches!(&caller, Caller::Unidentified(reason) if reason.contains("another conversation")),
        "a shell with no id must not act as the owner of the sole session: {caller:?}"
    );
}

#[test]
fn several_open_sessions_and_no_identity_is_unidentified() {
    let dir = TempDir::new().expect("tempdir");
    start_session(dir.path(), Some("one"), None, PROJECT, StartDecision::Auto).expect("one");
    start_session(dir.path(), Some("two"), None, PROJECT, StartDecision::New).expect("two");

    let caller = caller_session(dir.path(), PROJECT, &EngineIdentity::default()).expect("ok");

    assert!(matches!(caller, Caller::Unidentified(_)), "{caller:?}");
}

#[test]
fn clearing_another_sessions_tasks_is_refused() {
    let dir = TempDir::new().expect("tempdir");
    let caller = Caller::Session("mine".to_string());

    let err = ensure_session_is_ours(dir.path(), "theirs", &caller, None)
        .expect_err("clear --session of another session must be refused");

    assert!(format!("{err:#}").contains("theirs"), "{err:#}");
    assert!(ensure_session_is_ours(dir.path(), "mine", &caller, None).is_ok());
}

#[test]
fn clearing_another_session_needs_its_own_id_as_the_flag_value() {
    let dir = TempDir::new().expect("tempdir");
    let caller = Caller::Session("mine".to_string());

    assert!(ensure_session_is_ours(dir.path(), "theirs", &caller, Some("theirs")).is_ok());
    assert!(ensure_session_is_ours(dir.path(), "theirs", &caller, Some("mine")).is_err());
}

proptest::proptest! {
    // Each case writes a session and a task file, so the case count stays low.
    #![proptest_config(proptest::prelude::ProptestConfig::with_cases(48))]

    // The check against an oracle over every owner, caller, and override: a task is allowed when
    // it has no session, when the caller owns it, or when --other-session names its owner. Only
    // the override path and an owner-less task give a note, and the check never writes the store.
    #[test]
    fn the_check_matches_the_ownership_rule(
        tag in proptest::option::of(proptest::sample::select(vec!["s-a", "s-b"])),
        caller in proptest::option::of(proptest::sample::select(vec!["s-a", "s-b"])),
        other in proptest::option::of(proptest::sample::select(vec!["s-a", "s-b"])),
    ) {
        let dir = TempDir::new().unwrap();
        let (slug, _) = open_task_in_session(dir.path());
        let mut task = load_task(dir.path(), &slug).unwrap();
        task.session = tag.map(str::to_string);
        save_task(dir.path(), &task).unwrap();
        let path = crate::task::tasks_dir(dir.path()).join(format!("{slug}.json"));
        let before = std::fs::read(&path).unwrap();
        let caller = caller.map_or_else(
            || Caller::Unidentified("no session".to_string()),
            |id| Caller::Session(id.to_string()),
        );
        let got = ensure_task_is_ours(dir.path(), &slug, &caller, other);
        let owns = tag.is_none() || matches!(&caller, Caller::Session(id) if Some(id.as_str()) == tag);
        let overridden = !owns && tag.is_some() && other == tag;
        match got {
            Ok(note) => {
                proptest::prop_assert!(owns || overridden, "allowed: {tag:?} {caller:?} {other:?}");
                proptest::prop_assert_eq!(note.is_some(), overridden || tag.is_none());
            }
            Err(e) => proptest::prop_assert!(!owns && !overridden, "refused: {e:#}"),
        }
        proptest::prop_assert_eq!(before, std::fs::read(&path).unwrap());
    }
}

#[test]
fn a_change_to_an_owner_less_task_records_a_note_naming_the_caller() {
    let dir = TempDir::new().expect("tempdir");
    let (slug, _owner) = open_task_in_session(dir.path());
    let mut task = load_task(dir.path(), &slug).expect("load task");
    task.session = None;
    save_task(dir.path(), &task).expect("save owner-less task");
    let caller = Caller::Session("some-conversation".to_string());

    let note = ensure_task_is_ours(dir.path(), &slug, &caller, None)
        .expect("an owner-less task is not refused")
        .expect("the change must record a note");

    let text = format!("{note:?}");
    assert!(
        text.contains("some-conversation"),
        "note must name the caller: {text}"
    );
}

#[test]
fn an_other_session_override_returns_a_session_note_naming_the_caller() {
    let dir = TempDir::new().expect("tempdir");
    let (_task, owner) = open_task_in_session(dir.path());
    let caller = Caller::Session("some-other-session".to_string());

    let note = ensure_session_is_ours(dir.path(), &owner, &caller, Some(&owner))
        .expect("the named owner may be acted on");

    let text = note
        .text
        .as_deref()
        .expect("the override must carry a note");
    assert!(
        text.contains("some-other-session"),
        "note must name the caller"
    );
    assert_eq!(note.session_id, owner);
}

#[test]
fn the_caller_own_session_returns_no_note() {
    let dir = TempDir::new().expect("tempdir");
    let (_task, owner) = open_task_in_session(dir.path());
    let caller = Caller::Session(owner.clone());

    let note =
        ensure_session_is_ours(dir.path(), &owner, &caller, None).expect("own session is allowed");

    assert!(note.text.is_none(), "no override applied, so no note");
}

/// A parent with no session is refused for a child of a session, the same as `child_of` refuses
/// it, because starting the child would change a task that no session owns (#3593).
#[test]
fn an_explicit_parent_with_no_session_is_refused_like_child_of() {
    let dir = TempDir::new().expect("tempdir");
    let (slug, owner) = open_task_in_session(dir.path());
    let mut task = load_task(dir.path(), &slug).expect("load task");
    task.session = None;
    save_task(dir.path(), &task).expect("save owner-less task");
    let parent = load_task(dir.path(), &slug).expect("load parent");

    let err = ensure_parent_in_session(Some(&parent), &slug, Some(&owner))
        .expect_err("a session-less parent must be refused for a child in a session");

    assert!(format!("{err:#}").contains("has no session"), "{err:#}");
    assert!(
        ensure_parent_in_session(Some(&parent), &slug, None).is_err(),
        "a session-less parent is refused for a session-less child too"
    );
}

// #3749: a command that failed after it changed tasks still records its audit note.
#[test]
fn record_after_failure_writes_the_note_only_when_a_task_changed() {
    use crate::task::partial::PartialWrite;
    use crate::task::session::load_existing_session;
    let dir = TempDir::new().expect("tempdir");
    let (slug, session) = open_task_in_session(dir.path());
    let changed = load_task(dir.path(), &slug).expect("task");
    let audit = |text: &str| SessionOverride {
        session_id: session.clone(),
        text: Some(text.to_owned()),
        pending: None,
    };

    let plain = audit("not written").record_after_failure(dir.path(), anyhow::anyhow!("boom"));
    assert_eq!(format!("{plain:#}"), "boom");
    let stored = load_existing_session(dir.path(), &session).expect("session");
    assert_eq!(stored.resume.context, None, "no task changed, so no note");

    let partial = PartialWrite::error(vec![changed], anyhow::anyhow!("second save failed"));
    let err = audit("override note").record_after_failure(dir.path(), partial);
    assert!(format!("{err:#}").contains("second save failed"));
    let stored = load_existing_session(dir.path(), &session).expect("session");
    assert_eq!(stored.resume.context.as_deref(), Some("override note"));
}

proptest::proptest! {
    #![proptest_config(proptest::prelude::ProptestConfig::with_cases(12))]

    // The audit note is written exactly when the error holds a changed task, under any number of
    // context layers, and the error keeps its message (#3749).
    #[test]
    fn the_note_is_written_exactly_when_a_task_changed(changed in 0usize..3, layers in 0usize..4) {
        use crate::task::partial::PartialWrite;
        use crate::task::session::load_existing_session;
        let dir = TempDir::new().expect("tempdir");
        let (slug, session) = open_task_in_session(dir.path());
        let task = load_task(dir.path(), &slug).expect("task");
        let mut error = if changed == 0 {
            anyhow::anyhow!("save failed")
        } else {
            PartialWrite::error(vec![task; changed], anyhow::anyhow!("save failed"))
        };
        for n in 0..layers {
            error = error.context(format!("layer {n}"));
        }
        let audit = SessionOverride { session_id: session.clone(), text: Some("note".to_owned()), pending: None };
        let err = audit.record_after_failure(dir.path(), error);
        let text = format!("{err:#}");
        proptest::prop_assert!(text.contains("save failed"), "{}", text);
        let stored = load_existing_session(dir.path(), &session).expect("session");
        let want = (changed > 0).then_some("note");
        proptest::prop_assert_eq!(stored.resume.context.as_deref(), want);
    }
}

// #3755: one rule covers every command that records an audit note after its change. When the
// note fails, the error says the change is saved, and it says what to do instead of a retry.
#[test]
fn every_failed_note_says_the_change_is_saved() {
    let dir = TempDir::new().expect("tempdir");
    let missing = "no-such-session";
    let audit = SessionOverride {
        session_id: missing.to_owned(),
        text: Some("note".to_owned()),
        pending: None,
    };
    let notes: Vec<(&str, anyhow::Error)> = vec![
        (
            "session command",
            audit
                .record(dir.path())
                .expect_err("no session to hold the note"),
        ),
        (
            "task clear",
            super::record_cleared_override(dir.path(), missing, "caller", &["a".to_owned()])
                .expect_err("no session to hold the note"),
        ),
        (
            "task command",
            OverrideNote {
                slug: "gone-task".to_owned(),
                text: "note".to_owned(),
                owner_less: false,
                pending: super::super::pending_note::Pending::for_task(
                    dir.path(),
                    "gone-task",
                    "note",
                )
                .expect("journal"),
            }
            .record(dir.path())
            .expect_err("no task to hold the note"),
        ),
    ];
    for (command, err) in notes {
        let text = format!("{err:#}");
        assert!(
            text.contains("saved") || text.contains("cleared") || text.contains("deleted"),
            "{command}: the error must say the change happened: {text}"
        );
        assert!(
            text.contains("do not retry") || text.contains("Fix:"),
            "{command}: the error must say what to do: {text}"
        );
    }
}

// #3762: the guard journals the note before the change. A crash between the change and the note
// leaves the entry, and the next command writes the note.
#[test]
fn a_crash_after_the_guard_leaves_the_note_for_recovery() {
    let dir = TempDir::new().expect("tempdir");
    let (slug, owner) = open_task_in_session(dir.path());
    let caller = Caller::Session("some-other-session".to_string());
    let note = ensure_task_is_ours(dir.path(), &slug, &caller, Some(&owner))
        .expect("the named owner may be acted on")
        .expect("an override needs a note");
    note.pending.crash();

    let lost = super::super::pending_note::recover(dir.path());

    assert!(lost.is_empty(), "{lost:?}");
    let task = load_task(dir.path(), &slug).expect("task");
    assert_eq!(task.notes.len(), 1, "{:?}", task.notes);
    assert!(
        task.notes[0].text.contains("some-other-session"),
        "{:?}",
        task.notes
    );
}

#[test]
fn a_refused_command_leaves_no_journal() {
    let dir = TempDir::new().expect("tempdir");
    let (slug, owner) = open_task_in_session(dir.path());
    let caller = Caller::Session("some-other-session".to_string());
    drop(ensure_task_is_ours(dir.path(), &slug, &caller, Some(&owner)).expect("allowed"));

    let lost = super::super::pending_note::recover(dir.path());

    assert!(lost.is_empty(), "{lost:?}");
    assert!(load_task(dir.path(), &slug).expect("task").notes.is_empty());
}

// #3762: a delete of an owner-less task notes the caller's session, so the journal of that note
// names the session and not the task that is about to be gone.
#[test]
fn a_crash_after_a_delete_leaves_the_note_on_the_callers_session() {
    let dir = TempDir::new().expect("tempdir");
    let (slug, session) = open_task_in_session(dir.path());
    let mut task = load_task(dir.path(), &slug).expect("task");
    task.session = None;
    save_task(dir.path(), &task).expect("save");
    let caller = Caller::Session(session.clone());
    let note = ensure_task_is_ours(dir.path(), &slug, &caller, None)
        .expect("an owner-less task is allowed")
        .expect("an owner-less task needs a note");
    let journal = note
        .journal_for_delete(dir.path(), &caller)
        .expect("journal")
        .expect("a session journal");
    journal.crash();
    drop(note);

    let lost = super::super::pending_note::recover(dir.path());

    assert!(lost.is_empty(), "{lost:?}");
    let stored =
        super::super::session::load_existing_session(dir.path(), &session).expect("session");
    let context = stored.resume.context.unwrap_or_default();
    assert!(
        context.contains("Deleted tasks with no session"),
        "{context}"
    );
}
