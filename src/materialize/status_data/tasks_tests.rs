//! The task fields of the status line: sessions, counts, titles, and unreadable stores.

use super::*;
use proptest::prelude::*;

// --- collect_tasks (mandatory-sessions rework) ---

use crate::task::session::{StartDecision, StartOutcome, start_session};

const PROJECT: &str = "proj-0000000000";
const OTHER: &str = "other-1111111111";

fn created(outcome: StartOutcome) -> crate::task::session::Session {
    match outcome {
        StartOutcome::Created(s) => s,
        other => panic!("expected Created, got {other:?}"),
    }
}

#[test]
fn collect_tasks_an_unreadable_session_store_shows_the_marker_not_no_work() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    created(
        crate::task::session::start_session(
            dir.path(),
            None,
            None,
            PROJECT,
            crate::task::session::StartDecision::Auto,
        )
        .unwrap(),
    );
    let sessions = crate::task::tasks_dir(dir.path()).join("sessions");
    std::fs::set_permissions(&sessions, std::fs::Permissions::from_mode(0o000)).unwrap();
    let readable_anyway = std::fs::read_dir(&sessions).is_ok();
    let data = collect_tasks_from_state_dir(dir.path(), PROJECT);
    std::fs::set_permissions(&sessions, std::fs::Permissions::from_mode(0o700)).unwrap();
    if readable_anyway {
        return; // running as root: the permission cannot be enforced
    }
    assert!(
        data.unreadable,
        "an unreadable store must not look like no work"
    );
    assert!(data.session.is_none());
}

fn write_bad_task_file(dir: &std::path::Path) {
    let tasks = crate::task::tasks_dir(dir);
    std::fs::create_dir_all(&tasks).unwrap();
    std::fs::write(tasks.join("broken.json"), "not json").unwrap();
}

#[test]
fn collect_tasks_with_no_open_session_does_not_read_a_bad_task_file() {
    let dir = tempfile::tempdir().unwrap();
    write_bad_task_file(dir.path());
    let data = collect_tasks_from_state_dir(dir.path(), PROJECT);
    assert!(!data.unreadable, "no open session has nothing to count");
    assert!(data.session.is_none());
}

#[test]
fn collect_tasks_with_an_open_session_shows_a_bad_task_file_as_unreadable() {
    let dir = tempfile::tempdir().unwrap();
    created(
        crate::task::session::start_session(
            dir.path(),
            None,
            None,
            PROJECT,
            crate::task::session::StartDecision::Auto,
        )
        .unwrap(),
    );
    write_bad_task_file(dir.path());
    let data = collect_tasks_from_state_dir(dir.path(), PROJECT);
    assert!(
        data.unreadable,
        "a bad task file must not look like no work"
    );
}

#[test]
fn collect_tasks_zero_open_sessions_for_project_is_none() {
    let dir = tempfile::tempdir().unwrap();
    let data = collect_tasks_from_state_dir(dir.path(), PROJECT);
    assert!(data.session.is_none());
    assert!(data.current.is_none());
}

#[test]
fn collect_tasks_one_open_session_reports_done_over_total() {
    let dir = tempfile::tempdir().unwrap();
    let session =
        created(start_session(dir.path(), Some("s"), None, PROJECT, StartDecision::Auto).unwrap());
    let t1 = crate::task::add_task_for_session(
        dir.path(),
        "one",
        crate::task::ParentSpec::Detached,
        &session.id,
    )
    .unwrap();
    crate::task::add_task_for_session(
        dir.path(),
        "two",
        crate::task::ParentSpec::Detached,
        &session.id,
    )
    .unwrap();
    crate::task::complete_task(dir.path(), &t1.slug, true).unwrap();

    let data = collect_tasks_from_state_dir(dir.path(), PROJECT);
    assert_eq!(data.session, Some(SessionProgress { done: 1, total: 2 }));
}

#[test]
fn collect_tasks_two_open_sessions_sums_across_project_only() {
    let dir = tempfile::tempdir().unwrap();
    let s1 = created(
        start_session(
            dir.path(),
            Some("first"),
            None,
            PROJECT,
            StartDecision::Auto,
        )
        .unwrap(),
    );
    let s2 = created(
        start_session(
            dir.path(),
            Some("second"),
            None,
            PROJECT,
            StartDecision::New,
        )
        .unwrap(),
    );
    start_session(
        dir.path(),
        Some("unrelated"),
        None,
        OTHER,
        StartDecision::Auto,
    )
    .unwrap();

    let t1 = crate::task::add_task_for_session(
        dir.path(),
        "a",
        crate::task::ParentSpec::Detached,
        &s1.id,
    )
    .unwrap();
    crate::task::complete_task(dir.path(), &t1.slug, true).unwrap();
    crate::task::add_task_for_session(dir.path(), "b", crate::task::ParentSpec::Detached, &s2.id)
        .unwrap();

    let data = collect_tasks_from_state_dir(dir.path(), PROJECT);
    assert_eq!(data.session, Some(SessionProgress { done: 1, total: 2 }));
}

#[test]
fn collect_tasks_lists_each_open_session_with_its_owner() {
    use crate::task::session::{EngineIdentity, StartRequest, start_session_as};
    let dir = tempfile::tempdir().unwrap();
    let start = |conv: &str| {
        let owner = EngineIdentity::from_session_id(Some(conv));
        let request = StartRequest {
            name: Some(conv),
            description: None,
            project: PROJECT,
            owner: &owner,
            resume: &crate::task::resume::ResumeContext::default(),
        };
        created(start_session_as(dir.path(), &request, StartDecision::New).unwrap())
    };
    let a = start("conv-a");
    let b = start("conv-b");
    for (session, done_title, wip_title) in [(&a, "a done", "task A"), (&b, "b open", "task B")] {
        let first = crate::task::add_task_for_session(
            dir.path(),
            done_title,
            crate::task::ParentSpec::Detached,
            &session.id,
        )
        .unwrap();
        if done_title == "a done" {
            crate::task::complete_task(dir.path(), &first.slug, true).unwrap();
        }
        let wip = crate::task::add_task_for_session(
            dir.path(),
            wip_title,
            crate::task::ParentSpec::Detached,
            &session.id,
        )
        .unwrap();
        // Forced: the in-progress task runs beside the open task of the same session.
        crate::task::start_task(dir.path(), &wip.slug, true).unwrap();
    }

    let data = collect_tasks_from_state_dir(dir.path(), PROJECT);

    let owners: Vec<(Option<&str>, SessionProgress, Option<&str>)> = data
        .sessions
        .iter()
        .map(|s| (s.owner.as_deref(), s.progress, s.current.as_deref()))
        .collect();
    let mut expected = vec![
        (
            Some("conv-a"),
            SessionProgress { done: 1, total: 2 },
            Some("task A"),
        ),
        (
            Some("conv-b"),
            SessionProgress { done: 0, total: 2 },
            Some("task B"),
        ),
    ];
    expected.sort_by_key(|e| e.0);
    let mut got = owners;
    got.sort_by_key(|e| e.0);
    assert_eq!(
        got, expected,
        "each session keeps its own owner, count, and title"
    );
}

#[test]
fn collect_tasks_current_is_scoped_to_the_projects_open_sessions() {
    let dir = tempfile::tempdir().unwrap();
    let session =
        created(start_session(dir.path(), Some("s"), None, PROJECT, StartDecision::Auto).unwrap());
    let task = crate::task::add_task_for_session(
        dir.path(),
        "In progress",
        crate::task::ParentSpec::Detached,
        &session.id,
    )
    .unwrap();
    crate::task::start_task(dir.path(), &task.slug, false).unwrap();

    let data = collect_tasks_from_state_dir(dir.path(), PROJECT);
    assert_eq!(data.current.as_deref(), Some("In progress"));
}

// #3393: the summed progress equals the sum of the per-session progress, and it is `None`
// exactly when no session is open.
proptest! {
    #![proptest_config(ProptestConfig::with_cases(16))]
    #[test]
    fn the_session_total_is_the_sum_of_each_open_session(
        sessions in prop::collection::vec((0usize..4, 0usize..4), 0..4),
    ) {
        let dir = tempfile::tempdir().unwrap();
        let mut expected = Vec::new();
        for (i, (total, done)) in sessions.iter().enumerate() {
            let done = (*done).min(*total);
            let decision = if i == 0 { StartDecision::Auto } else { StartDecision::New };
            let session = created(
                start_session(dir.path(), Some(&format!("s{i}")), None, PROJECT, decision).unwrap(),
            );
            for t in 0..*total {
                let task = crate::task::add_task_for_session(
                    dir.path(),
                    &format!("task {t}"),
                    crate::task::ParentSpec::Detached,
                    &session.id,
                )
                .unwrap();
                if t < done {
                    crate::task::complete_task(dir.path(), &task.slug, true).unwrap();
                }
            }
            expected.push((session.id, SessionProgress { done: u64::try_from(done).unwrap(), total: u64::try_from(*total).unwrap() }));
        }
        expected.sort_by(|a, b| a.0.cmp(&b.0));

        let data = collect_tasks_from_state_dir(dir.path(), PROJECT);

        prop_assert!(!data.unreadable);
        let per_session: Vec<SessionProgress> = data.sessions.iter().map(|s| s.progress).collect();
        let want: Vec<SessionProgress> = expected.iter().map(|(_, p)| *p).collect();
        prop_assert_eq!(&per_session, &want);
        let summed = want.iter().fold(SessionProgress { done: 0, total: 0 }, |a, p| SessionProgress {
            done: a.done + p.done,
            total: a.total + p.total,
        });
        prop_assert_eq!(data.session, (!sessions.is_empty()).then_some(summed));
    }
}
