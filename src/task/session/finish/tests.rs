use proptest::prelude::*;
use tempfile::TempDir;

use crate::task::session::{StartDecision, StartOutcome, start_session};
use crate::task::{ParentSpec, Task, TaskState, add_task_for_session, load_task, save_task};

const PROJECT_A: &str = "finish-a";
const PROJECT_B: &str = "finish-b";

fn state() -> impl Strategy<Value = TaskState> {
    prop_oneof![
        Just(TaskState::Open),
        Just(TaskState::Wip),
        Just(TaskState::Waiting),
        Just(TaskState::Done),
    ]
}

fn start(dir: &std::path::Path, project: &str, decision: StartDecision) -> String {
    match start_session(dir, None, None, project, decision).unwrap() {
        StartOutcome::Created(s) => s.id,
        other => panic!("expected Created, got {other:?}"),
    }
}

/// Add a task to `session` and set its state directly, so any mix of states is possible.
fn add_in(dir: &std::path::Path, n: usize, session: &str, state: TaskState) -> Task {
    let mut task =
        add_task_for_session(dir, &format!("step {n}"), ParentSpec::Detached, session).unwrap();
    task.state = state;
    save_task(dir, &task).unwrap();
    task
}

proptest! {
    // Each case writes session and task files, so the case count stays low.
    #![proptest_config(ProptestConfig::with_cases(24))]

    // A finish with --abandon-open untags exactly the tasks that are not done, gives each one
    // note, and keeps the tag and the notes of a done task. The counts add up.
    #[test]
    fn finish_untags_exactly_the_unfinished_tasks(states in proptest::collection::vec(state(), 0..6)) {
        let dir = TempDir::new().unwrap();
        let id = start(dir.path(), PROJECT_A, StartDecision::Auto);
        let tasks: Vec<Task> =
            states.iter().enumerate().map(|(n, s)| add_in(dir.path(), n, &id, *s)).collect();
        let outcome = super::finish_session(dir.path(), &id, true).unwrap();
        let mut abandoned: Vec<&str> = outcome.abandoned.iter().map(|t| t.slug.as_str()).collect();
        abandoned.sort_unstable();
        let mut unfinished: Vec<&str> = tasks
            .iter()
            .filter(|t| t.state != TaskState::Done)
            .map(|t| t.slug.as_str())
            .collect();
        unfinished.sort_unstable();
        prop_assert_eq!(&abandoned, &unfinished);
        prop_assert_eq!(outcome.total, tasks.len() as u64);
        prop_assert_eq!(outcome.done + abandoned.len() as u64, outcome.total);
        for task in &tasks {
            let now = load_task(dir.path(), &task.slug).unwrap();
            if task.state == TaskState::Done {
                prop_assert_eq!(now.session.as_deref(), Some(id.as_str()));
                prop_assert!(now.notes.is_empty());
            } else {
                prop_assert!(now.session.is_none());
                prop_assert_eq!(now.notes.len(), 1);
            }
        }
    }

    // `session start --replace` reads the task store once for every session it abandons (#3479).
    // Whatever the mix, a task is untagged iff its session is an abandoned session of the project
    // and it is not done; every other task keeps its tag and gets no note.
    #[test]
    fn replace_untags_only_the_unfinished_tasks_of_abandoned_sessions(
        open in 1usize..=3,
        tasks in proptest::collection::vec((0usize..4, state()), 0..8),
    ) {
        let dir = TempDir::new().unwrap();
        let mut sessions = vec![start(dir.path(), PROJECT_A, StartDecision::Auto)];
        for _ in 1..open {
            sessions.push(start(dir.path(), PROJECT_A, StartDecision::New));
        }
        let other = start(dir.path(), PROJECT_B, StartDecision::Auto);
        let added: Vec<(Task, bool)> = tasks
            .iter()
            .enumerate()
            .map(|(n, (pick, s))| {
                let (session, abandoned) = sessions
                    .get(*pick)
                    .map_or((other.as_str(), false), |id| (id.as_str(), true));
                (add_in(dir.path(), n, session, *s), abandoned)
            })
            .collect();
        start_session(dir.path(), None, None, PROJECT_A, StartDecision::Replace).unwrap();
        for (task, in_abandoned) in &added {
            let now = load_task(dir.path(), &task.slug).unwrap();
            if *in_abandoned && task.state != TaskState::Done {
                prop_assert!(now.session.is_none(), "{} kept its tag", task.slug);
                prop_assert_eq!(now.notes.len(), 1);
            } else {
                prop_assert_eq!(&now.session, &task.session);
                prop_assert!(now.notes.is_empty());
            }
        }
    }
}
