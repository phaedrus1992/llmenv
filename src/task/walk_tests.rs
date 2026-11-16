//! Tests of the blocker walk and the ancestor walk: loops, legacy cross-session parents, and the
//! next-task output (#3618, #3619).

use std::collections::HashMap;

use proptest::prelude::*;
use tempfile::TempDir;

use super::blockers::{blocker_walk_error, first_undone_in_subtree, is_actionable};
use super::*;

fn task(slug: &str, state: TaskState, parent: Option<&str>, blocked_on: &[&str]) -> Task {
    Task {
        slug: slug.to_string(),
        title: slug.to_string(),
        state,
        parent: parent.map(str::to_string),
        relation: Relation::Child,
        parallel: false,
        blocked_on: blocked_on.iter().map(|b| (*b).to_string()).collect(),
        notes: Vec::new(),
        detail: None,
        session: Some("s".to_string()),
        created_at: "2026-10-02T00:00:00Z".to_string(),
        updated_at: "2026-10-02T00:00:00Z".to_string(),
    }
}

fn index(tasks: &[Task]) -> HashMap<&str, &Task> {
    tasks.iter().map(|t| (t.slug.as_str(), t)).collect()
}

/// A task of a session, saved through the real writer, then linked as the test needs.
fn saved(dir: &Path, title: &str, session: &str) -> Task {
    add_task_for_session(dir, title, ParentSpec::Detached, session).expect("add task")
}

#[test]
fn a_start_refuses_a_legacy_parent_of_another_session() {
    let dir = TempDir::new().expect("tempdir");
    let parent = saved(dir.path(), "Parent", "session-a");
    let mut child = saved(dir.path(), "Child", "session-b");
    // A link made before the refusals of #3480 and #3593.
    child.parent = Some(parent.slug.clone());
    child.relation = Relation::Child;
    save_task(dir.path(), &child).expect("save legacy link");

    let err = start_task(dir.path(), &child.slug, false).expect_err("the start must refuse");

    let text = format!("{err:#}");
    assert!(text.contains("belongs to session 'session-a'"), "{text}");
    assert!(text.contains("--no-parent"), "{text}");
    assert_eq!(
        load_task(dir.path(), &parent.slug).expect("load").state,
        TaskState::Open,
        "the parent of the other session stays open"
    );
}

#[test]
fn a_start_still_promotes_an_open_parent_of_the_same_session() {
    let dir = TempDir::new().expect("tempdir");
    let parent = saved(dir.path(), "Parent", "session-a");
    let mut child = saved(dir.path(), "Child", "session-a");
    child.parent = Some(parent.slug.clone());
    child.relation = Relation::Child;
    save_task(dir.path(), &child).expect("save link");

    start_task(dir.path(), &child.slug, false).expect("the same session may start");

    assert_eq!(
        load_task(dir.path(), &parent.slug).expect("load").state,
        TaskState::Wip
    );
}

#[test]
fn a_loop_through_the_waiter_is_an_error_and_not_a_clear_walk() {
    // `w` waits on `blocker`, and `w` is in a loop with `p`.
    let tasks = [
        task("blocker", TaskState::Done, None, &[]),
        task("w", TaskState::Open, Some("p"), &["blocker"]),
        task("p", TaskState::Open, Some("w"), &[]),
    ];
    let by_slug = index(&tasks);
    let err = first_undone_in_subtree("blocker", "w", &by_slug).expect_err("the loop is reported");
    assert!(format!("{err:#}").contains("--no-parent"), "{err:#}");
    assert!(!is_actionable(&tasks[1], &by_slug));
    assert!(blocker_walk_error(&tasks[1], &by_slug).is_some());
}

#[test]
fn a_start_reports_a_loop_through_the_waiter() {
    let dir = TempDir::new().expect("tempdir");
    let blocker = saved(dir.path(), "Blocker", "s");
    done_task(dir.path(), &blocker.slug).expect("close");
    let mut waiter = saved(dir.path(), "Waiter", "s");
    let mut other = saved(dir.path(), "Other", "s");
    waiter.parent = Some(other.slug.clone());
    waiter.blocked_on = vec![blocker.slug.clone()];
    other.parent = Some(waiter.slug.clone());
    save_task(dir.path(), &waiter).expect("save");
    save_task(dir.path(), &other).expect("save");

    let err = start_task(dir.path(), &waiter.slug, false).expect_err("the loop blocks the start");

    assert!(format!("{err:#}").contains("loop"), "{err:#}");
}

#[test]
fn the_only_open_task_with_a_looping_blocker_is_not_offered() {
    // `a` is the only open task. It waits on `b`, and `b` and `c` are done and loop.
    let tasks = vec![
        task("a", TaskState::Open, None, &["b"]),
        task("b", TaskState::Done, Some("c"), &[]),
        task("c", TaskState::Done, Some("b"), &[]),
    ];
    let mut a_only = tasks.clone();
    for t in &mut a_only[1..] {
        t.session = Some("other".to_string());
    }
    let dir = TempDir::new().expect("tempdir");
    let session_id = {
        let project = project::current_tag().expect("project tag");
        match session::start_session(
            dir.path(),
            None,
            None,
            &project,
            session::StartDecision::New,
        )
        .expect("start session")
        {
            session::StartOutcome::Created(s) => s.id,
            other => panic!("expected a new session, got {other:?}"),
        }
    };
    a_only[0].session = Some(session_id.clone());
    let project = project::current_tag().expect("project tag");
    let open = session::open_sessions_strict(dir.path(), &project).expect("open sessions");

    let idle = idle_sessions(open, &a_only);

    assert_eq!(idle.len(), 1);
    assert_eq!(idle[0].next.slug, "a");
    let (looped, why) = idle[0].loop_error.as_ref().expect("the loop is named");
    assert_eq!(looped, "a");
    assert!(why.contains("--no-parent"), "{why}");
}

#[test]
fn next_task_loops_names_the_task_that_next_cannot_offer() {
    let tasks = vec![
        task("cur", TaskState::Wip, None, &[]),
        task("z", TaskState::Open, None, &["b"]),
        task("b", TaskState::Done, Some("c"), &[]),
        task("c", TaskState::Done, Some("b"), &[]),
    ];
    let current = tasks[0].clone();
    assert!(resolve_next_task(&tasks, &tasks, &current).is_none());
    let loops = next_task_loops(&tasks, &tasks, &current);
    assert_eq!(loops.len(), 1);
    assert_eq!(loops[0].0, "z");
    assert!(loops[0].1.contains("loop"), "{}", loops[0].1);
}

#[test]
fn next_task_loops_names_a_looped_task_even_when_a_later_task_is_offered() {
    let tasks = vec![
        task("cur", TaskState::Wip, None, &[]),
        task("looped", TaskState::Open, None, &["b"]),
        task("offered", TaskState::Open, None, &[]),
        task("b", TaskState::Done, Some("c"), &[]),
        task("c", TaskState::Done, Some("b"), &[]),
    ];
    let current = tasks[0].clone();
    assert_eq!(
        resolve_next_task(&tasks, &tasks, &current).map(|t| t.slug),
        Some("offered".to_string())
    );
    let loops = next_task_loops(&tasks, &tasks, &current);
    assert_eq!(
        loops.iter().map(|l| l.0.as_str()).collect::<Vec<_>>(),
        ["looped"]
    );
}

#[test]
fn the_idle_reminder_names_a_looped_task_that_is_not_the_first_open_task() {
    // `first` is blocked on an undone task, and `second` waits on a loop.
    let mut tasks = vec![
        task("first", TaskState::Open, None, &["undone"]),
        task("second", TaskState::Open, None, &["b"]),
        task("undone", TaskState::Wip, None, &[]),
        task("b", TaskState::Done, Some("c"), &[]),
        task("c", TaskState::Done, Some("b"), &[]),
    ];
    tasks[2].session = Some("other".to_string());
    let dir = TempDir::new().expect("tempdir");
    let project = project::current_tag().expect("project tag");
    let started = session::start_session(
        dir.path(),
        None,
        None,
        &project,
        session::StartDecision::New,
    )
    .expect("start session");
    let session::StartOutcome::Created(created) = started else {
        panic!("expected a new session");
    };
    for t in &mut tasks[..2] {
        t.session = Some(created.id.clone());
    }
    let open = session::open_sessions_strict(dir.path(), &project).expect("open sessions");
    let idle = idle_sessions(open, &tasks);
    let (looped, _) = idle[0].loop_error.as_ref().expect("the loop is named");
    assert_eq!(looped, "second");
}

#[test]
fn a_start_refuses_a_foreign_ancestor_above_an_intermediate_parent() {
    let dir = TempDir::new().expect("tempdir");
    let grand = saved(dir.path(), "Grand", "session-b");
    let mut middle = saved(dir.path(), "Middle", "session-b");
    let mut child = saved(dir.path(), "Child", "session-a");
    middle.parent = Some(grand.slug.clone());
    middle.relation = Relation::Child;
    middle.state = TaskState::Wip;
    child.parent = Some(middle.slug.clone());
    child.relation = Relation::Child;
    save_task(dir.path(), &middle).expect("save");
    save_task(dir.path(), &child).expect("save");

    let err = start_task(dir.path(), &child.slug, false).expect_err("a foreign grandparent");

    assert!(
        format!("{err:#}").contains("belongs to session 'session-b'"),
        "{err:#}"
    );
    assert_eq!(
        load_task(dir.path(), &grand.slug).expect("load").state,
        TaskState::Open
    );
}

#[test]
fn next_task_loops_is_empty_when_nothing_is_left() {
    let tasks = vec![
        task("cur", TaskState::Wip, None, &[]),
        task("d", TaskState::Done, None, &[]),
    ];
    let current = tasks[0].clone();
    assert!(next_task_loops(&tasks, &tasks, &current).is_empty());
}

/// A forest of up to eight tasks: each task has no parent or one earlier task as its parent, a
/// random state, and a random set of blockers (an earlier or later task, or a name that does not
/// exist).
fn forest() -> impl Strategy<Value = Vec<Task>> {
    let state = prop_oneof![
        Just(TaskState::Open),
        Just(TaskState::Wip),
        Just(TaskState::Waiting),
        Just(TaskState::Done),
    ];
    proptest::collection::vec(
        (
            state,
            proptest::option::of(0usize..8),
            proptest::collection::vec(0usize..10, 0..3),
        ),
        1..8,
    )
    .prop_map(|specs| {
        specs
            .iter()
            .enumerate()
            .map(|(i, (state, parent, blockers))| {
                let parent = parent.filter(|p| *p < i).map(|p| format!("t{p}"));
                let blocked: Vec<String> = blockers.iter().map(|b| format!("t{b}")).collect();
                let blocked: Vec<&str> = blocked.iter().map(String::as_str).collect();
                task(&format!("t{i}"), *state, parent.as_deref(), &blocked)
            })
            .collect()
    })
}

/// The slugs reachable from `slug` through child links without entering `waiter`. `None` when
/// `slug` itself is missing.
fn reachable<'a>(tasks: &'a [Task], slug: &'a str, waiter: &str) -> Option<Vec<&'a Task>> {
    let root = tasks.iter().find(|t| t.slug == slug)?;
    let mut out = vec![root];
    let mut next = 0;
    while next < out.len() {
        let at = out[next].slug.as_str();
        next += 1;
        out.extend(
            tasks
                .iter()
                .filter(|t| t.parent.as_deref() == Some(at) && t.slug != waiter),
        );
    }
    Some(out)
}

proptest! {
    /// In a forest a task is actionable exactly when it is neither done nor waiting and every
    /// blocker exists with its whole subtree (the subtree of the task itself left out) done.
    #[test]
    fn a_task_is_actionable_exactly_when_its_blockers_and_their_subtrees_are_done(tasks in forest()) {
        let by_slug = index(&tasks);
        for task in &tasks {
            let gate = !matches!(task.state, TaskState::Done | TaskState::Waiting);
            let blockers_done = task.blocked_on.iter().all(|b| {
                reachable(&tasks, b, &task.slug)
                    .is_some_and(|nodes| nodes.iter().all(|n| n.state == TaskState::Done))
            });
            prop_assert_eq!(is_actionable(task, &by_slug), gate && blockers_done, "{}", task.slug);
        }
    }

    /// In a forest the walk never fails, and it names an undone task exactly when one is reachable.
    #[test]
    fn the_walk_of_a_forest_ends_and_finds_an_undone_task_when_there_is_one(tasks in forest()) {
        let by_slug = index(&tasks);
        for task in &tasks {
            for blocker in &task.blocked_on {
                let found = first_undone_in_subtree(blocker, &task.slug, &by_slug);
                let found = found.expect("a forest has no loop");
                let undone = reachable(&tasks, blocker, &task.slug)
                    .is_none_or(|nodes| nodes.iter().any(|n| n.state != TaskState::Done));
                prop_assert_eq!(found.is_some(), undone, "{} on {}", task.slug, blocker);
            }
        }
    }

    /// A ring of done tasks is a loop: the walk from any member reports it, and so does the walk
    /// of a waiter that is a member.
    #[test]
    fn a_ring_of_parent_links_is_reported_by_the_walk(size in 2usize..6, member in 0usize..6) {
        let member = member % size;
        let mut tasks: Vec<Task> = (0..size)
            .map(|i| task(&format!("r{i}"), TaskState::Done, Some(&format!("r{}", (i + 1) % size)), &[]))
            .collect();
        tasks.push(task("outsider", TaskState::Open, None, &[]));
        let by_slug = index(&tasks);
        let member_slug = format!("r{member}");
        prop_assert!(first_undone_in_subtree(&member_slug, "outsider", &by_slug).is_err());
        prop_assert!(first_undone_in_subtree("r0", &member_slug, &by_slug).is_err());
    }
}

#[test]
fn a_start_names_a_missing_parent_and_an_unreadable_parent_apart() {
    let dir = TempDir::new().expect("tempdir");
    let mut child = saved(dir.path(), "Child", "session-a");
    child.parent = Some("ghost".to_string());
    child.relation = Relation::Child;
    save_task(dir.path(), &child).expect("save link");
    let missing = start_task(dir.path(), &child.slug, false).expect_err("a missing parent");
    let text = format!("{missing:#}");
    assert!(text.contains("missing from the task store"), "{text}");

    let parent = saved(dir.path(), "Parent", "session-a");
    child.parent = Some(parent.slug.clone());
    save_task(dir.path(), &child).expect("save link");
    std::fs::write(
        tasks_dir(dir.path()).join(format!("{}.json", parent.slug)),
        "{ not json",
    )
    .expect("corrupt the parent file");
    let unreadable = start_task(dir.path(), &child.slug, false).expect_err("an unreadable parent");
    let text = format!("{unreadable:#}");
    assert!(!text.contains("missing from the task store"), "{text}");
}

/// Tasks `t0..tN` whose parent is any task, itself included, so the links can loop or lead into a
/// loop.
fn parent_graph() -> impl Strategy<Value = Vec<Task>> {
    proptest::collection::vec(
        (
            proptest::option::of(0usize..6),
            proptest::collection::vec(0usize..6, 0..3),
            proptest::bool::ANY,
        ),
        1..6,
    )
    .prop_map(|specs| {
        let count = specs.len();
        specs
            .iter()
            .enumerate()
            .map(|(i, (parent, blockers, done))| {
                let parent = parent.map(|p| format!("t{}", p % count));
                let blocked: Vec<String> =
                    blockers.iter().map(|b| format!("t{}", b % count)).collect();
                let blocked: Vec<&str> = blocked.iter().map(String::as_str).collect();
                let state = if *done {
                    TaskState::Done
                } else {
                    TaskState::Open
                };
                task(&format!("t{i}"), state, parent.as_deref(), &blocked)
            })
            .collect()
    })
}

proptest! {
    /// The check above a waiter fails exactly when the parent links, followed from the waiter,
    /// revisit a task, and it always returns.
    #[test]
    fn the_waiter_chain_check_fails_exactly_when_the_links_revisit_a_task(tasks in parent_graph()) {
        let by_slug = index(&tasks);
        for waiter in &tasks {
            let mut seen = Vec::new();
            let mut at = Some(waiter.slug.as_str());
            let mut loops = false;
            while let Some(slug) = at {
                if seen.contains(&slug) {
                    loops = true;
                    break;
                }
                seen.push(slug);
                at = by_slug.get(slug).and_then(|t| t.parent.as_deref());
            }
            prop_assert_eq!(
                super::blockers::ensure_waiter_chain_ends_for_test(&waiter.slug, &by_slug).is_err(),
                loops,
                "{}", waiter.slug
            );
        }
    }

    /// A task has a blocker walk error exactly when one of its blocker walks fails, on any graph.
    #[test]
    fn a_task_has_a_walk_error_exactly_when_one_of_its_walks_fails(tasks in parent_graph()) {
        let by_slug = index(&tasks);
        for task in &tasks {
            let any_fails = task
                .blocked_on
                .iter()
                .any(|b| first_undone_in_subtree(b, &task.slug, &by_slug).is_err());
            prop_assert_eq!(blocker_walk_error(task, &by_slug).is_some(), any_fails, "{}", task.slug);
        }
    }

    /// `next_task_loops` names only tasks that are neither done nor waiting and have a walk error,
    /// and never a task that `resolve_next_task` returns.
    #[test]
    fn next_task_loops_names_only_blocked_open_tasks(tasks in parent_graph()) {
        let by_slug = index(&tasks);
        let current = tasks[0].clone();
        let loops = next_task_loops(&tasks, &tasks, &current);
        let next = resolve_next_task(&tasks, &tasks, &current);
        for (slug, _) in &loops {
            let task = tasks.iter().find(|t| &t.slug == slug).expect("a named task exists");
            prop_assert!(!matches!(task.state, TaskState::Done | TaskState::Waiting));
            prop_assert!(blocker_walk_error(task, &by_slug).is_some());
            prop_assert_ne!(next.as_ref().map(|t| &t.slug), Some(slug));
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(24))]
    /// A start changes an open ancestor only when that ancestor belongs to the session of the
    /// task that starts. Another session's ancestor stays as it was, and the error names it.
    #[test]
    fn a_start_promotes_an_open_parent_only_of_its_own_session(
        parent_session in proptest::option::of(0u8..2),
        child_session in 0u8..2,
    ) {
        let dir = TempDir::new().expect("tempdir");
        let mut parent = saved(dir.path(), "Parent", "base");
        parent.session = parent_session.map(|n| format!("session-{n}"));
        save_task(dir.path(), &parent).expect("save parent");
        let mut child = saved(dir.path(), "Child", &format!("session-{child_session}"));
        child.parent = Some(parent.slug.clone());
        child.relation = Relation::Child;
        save_task(dir.path(), &child).expect("save child");

        let result = start_task(dir.path(), &child.slug, false);

        let foreign = parent_session.is_some_and(|p| p != child_session);
        let after = load_task(dir.path(), &parent.slug).expect("load parent");
        if foreign {
            let err = result.expect_err("a foreign open parent must refuse the start");
            prop_assert!(format!("{err:#}").contains("belongs to session"), "{err:#}");
            prop_assert_eq!(after.state, TaskState::Open);
        } else {
            result.expect("an own or session-less open parent is promoted");
            prop_assert_eq!(after.state, TaskState::Wip);
        }
    }
}
