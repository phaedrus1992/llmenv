use super::*;
use crate::task::session::{StartDecision, StartOutcome, start_session};
use crate::task::{ParentSpec, add_task_for_session};
use tempfile::TempDir;

const PROJECT: &str = "test-project-0000000000";
const NOBODY: EngineIdentity = EngineIdentity {
    session_id: None,
    pid: None,
};

fn open_session(dir: &Path, decision: StartDecision) -> String {
    match start_session(dir, None, None, PROJECT, decision).unwrap() {
        StartOutcome::Created(s)
        | StartOutcome::Resumed(s)
        | StartOutcome::Replaced { session: s, .. } => s.id,
    }
}

fn add(dir: &Path, title: &str, session_id: &str) -> String {
    add_task_for_session(dir, title, ParentSpec::Detached, session_id)
        .unwrap()
        .slug
}

/// An old finished session and a new open one, each with tasks titled "Plan task N".
/// Returns the new session's id and slugs for N = 1..=3. The new slugs carry a `-2`.
fn old_and_new(dir: &Path) -> (String, Vec<String>) {
    let old = open_session(dir, StartDecision::Auto);
    for n in 1..=3 {
        add(dir, &format!("Plan task {n}"), &old);
    }
    let new = open_session(dir, StartDecision::Replace);
    let slugs = (1..=3)
        .map(|n| add(dir, &format!("Plan task {n}"), &new))
        .collect();
    (new, slugs)
}

#[test]
fn bare_slug_reaches_the_open_session_task_not_the_finished_one() {
    let dir = TempDir::new().unwrap();
    let (_, new) = old_and_new(dir.path());
    assert_eq!(new, ["plan-task-1-2", "plan-task-2-2", "plan-task-3-2"]);
    assert_eq!(
        resolve_identifier_for(dir.path(), "plan-task-3", &NOBODY).unwrap(),
        "plan-task-3-2"
    );
}

#[test]
fn exact_slug_of_the_open_session_wins() {
    let dir = TempDir::new().unwrap();
    let (_, new) = old_and_new(dir.path());
    assert_eq!(
        resolve_identifier_for(dir.path(), &new[0], &NOBODY).unwrap(),
        new[0]
    );
}

#[test]
fn a_prefix_that_hits_several_open_session_tasks_is_ambiguous() {
    let dir = TempDir::new().unwrap();
    old_and_new(dir.path());
    let err = resolve_identifier_for(dir.path(), "plan-task", &NOBODY)
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("plan-task-1-2") && err.contains("plan-task-3-2"),
        "{err}"
    );
    assert!(
        !err.contains("plan-task-1,"),
        "old session slug leaked: {err}"
    );
}

#[test]
fn a_slug_only_a_finished_session_has_still_resolves() {
    let dir = TempDir::new().unwrap();
    let old = open_session(dir.path(), StartDecision::Auto);
    let slug = add(dir.path(), "Old only", &old);
    open_session(dir.path(), StartDecision::Replace);
    assert_eq!(
        resolve_identifier_for(dir.path(), &slug, &NOBODY).unwrap(),
        slug
    );
}

#[test]
fn with_no_open_session_resolution_is_project_wide() {
    let dir = TempDir::new().unwrap();
    let slug = add(dir.path(), "Fix login timeout", "gone");
    assert_eq!(
        resolve_identifier_for(dir.path(), "fix-log", &NOBODY).unwrap(),
        slug
    );
}

#[test]
fn unknown_identifier_is_an_error() {
    let dir = TempDir::new().unwrap();
    old_and_new(dir.path());
    let err = resolve_identifier_for(dir.path(), "nope", &NOBODY).unwrap_err();
    assert!(err.to_string().contains("no task found"), "{err}");
}

#[test]
fn with_several_open_sessions_and_no_owner_an_open_session_task_still_wins() {
    let dir = TempDir::new().unwrap();
    let old = open_session(dir.path(), StartDecision::Auto);
    let finished = add(dir.path(), "Shared", &old);
    let first = open_session(dir.path(), StartDecision::Replace);
    let second = open_session(dir.path(), StartDecision::New);
    assert_ne!(first, second);
    let live = add(dir.path(), "Shared", &second);
    assert_eq!((finished.as_str(), live.as_str()), ("shared", "shared-2"));
    assert_eq!(
        resolve_identifier_for(dir.path(), "shared", &NOBODY).unwrap(),
        "shared-2"
    );
}

#[test]
fn pick_prefers_an_exact_match_and_rejects_an_ambiguous_prefix() {
    assert_eq!(pick(&["ab", "abc"], "ab").unwrap().as_deref(), Some("ab"));
    assert_eq!(
        pick(&["abc", "abd"], "abc").unwrap().as_deref(),
        Some("abc")
    );
    assert!(pick(&["abc", "abd"], "ab").is_err());
    assert_eq!(pick(&["abc"], "x").unwrap(), None);
}

proptest::proptest! {
    // Each case writes session and task files, so the case count stays low.
    #![proptest_config(proptest::prelude::ProptestConfig::with_cases(24))]

    // Whatever the titles, a bare slug never resolves to a task of the finished session
    // while the open session has tasks, and each open-session slug resolves to itself.
    #[test]
    fn open_session_tasks_win_over_finished_session_tasks(
        titles in proptest::collection::btree_set("[a-z]{1,6}", 1..6),
    ) {
        let dir = TempDir::new().unwrap();
        let old = open_session(dir.path(), StartDecision::Auto);
        for t in &titles {
            add(dir.path(), t, &old);
        }
        let new = open_session(dir.path(), StartDecision::Replace);
        let mine: Vec<String> = titles.iter().map(|t| add(dir.path(), t, &new)).collect();
        for t in &titles {
            let bare = crate::task::slugify(t);
            if let Ok(slug) = resolve_identifier_for(dir.path(), &bare, &NOBODY) {
                proptest::prop_assert!(mine.contains(&slug), "{bare} -> {slug}");
            }
        }
        for slug in &mine {
            proptest::prop_assert_eq!(
                &resolve_identifier_for(dir.path(), slug, &NOBODY).unwrap(),
                slug
            );
        }
    }
    // `pick` gives the same answer for any order of the slugs.
    #[test]
    fn pick_ignores_the_order_of_the_slugs(
        slugs in proptest::collection::btree_set("[a-c]{1,4}", 0..8),
        input in "[a-c]{1,3}",
    ) {
        let forward: Vec<&str> = slugs.iter().map(String::as_str).collect();
        let mut backward = forward.clone();
        backward.reverse();
        let a = pick(&forward, &input).map_err(|e| e.to_string());
        let b = pick(&backward, &input).map_err(|e| e.to_string());
        proptest::prop_assert_eq!(a, b);
        if slugs.contains(&input) {
            proptest::prop_assert_eq!(pick(&forward, &input).unwrap(), Some(input));
        }
    }
}

fn break_one_task_file(dir: &Path) {
    std::fs::write(crate::task::tasks_dir(dir).join("broken.json"), "not json").unwrap();
}

/// A corrupt task file is a read error, not "no task found" (#1112, #3471).
#[test]
fn one_corrupt_task_file_is_a_read_error_not_a_missing_task() {
    let dir = TempDir::new().unwrap();
    let session = open_session(dir.path(), StartDecision::Auto);
    let slug = add(dir.path(), "Step", &session);
    break_one_task_file(dir.path());
    let err = resolve_identifier_for(dir.path(), &slug, &NOBODY).unwrap_err();
    assert!(format!("{err:#}").contains("broken.json"), "{err:#}");
}

/// A corrupt task file can be an open sub-task, so it stops the parent from closing (#3471).
#[test]
fn one_corrupt_task_file_stops_a_task_from_closing() {
    let dir = TempDir::new().unwrap();
    let session = open_session(dir.path(), StartDecision::Auto);
    let slug = add(dir.path(), "Step", &session);
    crate::task::start_task(dir.path(), &slug, false).unwrap();
    break_one_task_file(dir.path());
    let err = crate::task::complete_task(dir.path(), &slug, false).unwrap_err();
    assert!(format!("{err:#}").contains("broken.json"), "{err:#}");
}

/// A bad name gets its own error before the store is read, even when the store is broken.
#[test]
fn a_bad_name_is_refused_before_a_broken_store_is_read() {
    let dir = TempDir::new().unwrap();
    let session = open_session(dir.path(), StartDecision::Auto);
    add(dir.path(), "Step", &session);
    break_one_task_file(dir.path());
    let err = crate::task::start_task(dir.path(), "../x", false).unwrap_err();
    assert!(
        format!("{err:#}").contains("not a valid task identifier"),
        "{err:#}"
    );
}

/// A checked session that is no longer open is refused, not written to (#3474).
#[test]
fn a_checked_session_that_closed_is_refused() {
    let dir = TempDir::new().unwrap();
    let id = open_session(dir.path(), StartDecision::Auto);
    let read = session::list_sessions_strict(dir.path()).unwrap();
    let checked = read.iter().find(|s| s.id == id).unwrap();
    session::finish_session(dir.path(), &id, true).unwrap();
    let closed = session::list_sessions_strict(dir.path()).unwrap();
    let closed = closed.iter().find(|s| s.id == id).unwrap();
    let err = crate::task::add_task(
        dir.path(),
        "Late",
        crate::task::ParentSpec::Detached,
        crate::task::SessionChoice::Checked(closed),
        PROJECT,
    )
    .unwrap_err();
    assert!(
        format!("{err:#}").contains("not an open session"),
        "{err:#}"
    );
    // The read from before the close still names an open session of the project.
    assert!(checked.is_open());
}

/// A bad `--after` name is refused before a broken store is read (#3477).
#[test]
fn a_bad_after_name_is_refused_before_a_broken_store_is_read() {
    let dir = TempDir::new().unwrap();
    let session = open_session(dir.path(), StartDecision::Auto);
    add(dir.path(), "Step", &session);
    break_one_task_file(dir.path());
    let new = crate::task::NewTask {
        title: "Next",
        after: Some("../x"),
        ..crate::task::NewTask::default()
    };
    let err =
        crate::task::add_task_for_session_with(dir.path(), &new, ParentSpec::Detached, &session)
            .unwrap_err();
    assert!(
        format!("{err:#}").contains("not a valid task identifier"),
        "{err:#}"
    );
}
