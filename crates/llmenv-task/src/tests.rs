use super::blockers::is_done_including_descendants;
use super::session::{StartDecision, StartOutcome, open_sessions_strict, start_session};
use super::*;
use tempfile::TempDir;

const PROJECT: &str = "test-project-0000000000";

/// A caller with no engine identity, as from a plain terminal.
static NO_OWNER: session::EngineIdentity = session::EngineIdentity {
    session_id: None,
    pid: None,
};
const ANYONE: SessionChoice<'static> = SessionChoice::Resolve(&NO_OWNER);

/// Create a task tagged to a fixed test session id, exercising the same
/// creation logic (`add_task_for_session`) the mandatory-session path
/// resolves down to — lets the store-behavior tests below (slug/parent/
/// nesting) skip the session-resolution dance, which has its own tests.
/// `None` maps to `ParentSpec::Detached`: a task with no display parent.
fn mk(dir: &Path, title: &str, parent: Option<&str>) -> anyhow::Result<Task> {
    let parent_spec = match parent {
        Some(p) => ParentSpec::Explicit(p),
        None => ParentSpec::Detached,
    };
    add_task_for_session(dir, title, parent_spec, "test-session")
}

#[test]
fn try_list_tasks_missing_store_is_empty_not_error() {
    let dir = TempDir::new().unwrap();
    assert_eq!(try_list_tasks(dir.path()).unwrap(), Vec::new());
}

#[cfg(unix)]
#[test]
fn fresh_store_dirs_and_lock_are_owner_only_from_creation() {
    use std::os::unix::fs::PermissionsExt;

    let dir = TempDir::new().unwrap();
    start_session(
        dir.path(),
        None,
        None,
        &current_project(),
        StartDecision::Auto,
    )
    .expect("test");
    mk(dir.path(), "seed task", None).expect("test");

    let mode_of = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
    assert_eq!(
        mode_of(&tasks_dir(dir.path())),
        0o700,
        "tasks/ must be owner-only from creation"
    );
    assert_eq!(
        mode_of(&tasks_dir(dir.path()).join("sessions")),
        0o700,
        "tasks/sessions/ must be owner-only from creation"
    );
    assert_eq!(
        mode_of(&tasks_dir(dir.path()).join(".lock")),
        0o600,
        "the store lock file must be owner-only from creation"
    );
}

#[cfg(unix)]
#[test]
fn try_list_tasks_unreadable_store_errors_instead_of_empty() {
    use std::os::unix::fs::PermissionsExt;

    let dir = TempDir::new().unwrap();
    let tasks = tasks_dir(dir.path());
    std::fs::create_dir_all(&tasks).unwrap();
    mk(dir.path(), "before making unreadable", None).unwrap();
    std::fs::set_permissions(&tasks, std::fs::Permissions::from_mode(0o000)).unwrap();

    let readable_anyway = std::fs::read_dir(&tasks).is_ok();
    let result = try_list_tasks(dir.path());
    // The tolerant wrapper must degrade to empty (not panic/propagate) while
    // the store is still unreadable — check before restoring permissions.
    let tolerant_result = list_tasks(dir.path());

    std::fs::set_permissions(&tasks, std::fs::Permissions::from_mode(0o700)).unwrap();
    if readable_anyway {
        return; // running as root / FS ignores perms — can't exercise EACCES
    }
    assert!(
        result.is_err(),
        "an unreadable tasks dir must be a genuine error, not an empty Vec: {result:?}"
    );
    assert_eq!(tolerant_result, Vec::new());
}

#[cfg(unix)]
#[test]
fn try_list_tasks_skips_both_corrupt_and_unreadable_individual_files() {
    use std::os::unix::fs::PermissionsExt;

    let dir = TempDir::new().unwrap();
    let good = mk(dir.path(), "a real task", None).unwrap();
    let tasks = tasks_dir(dir.path());
    std::fs::write(tasks.join("corrupt.json"), b"not json").unwrap();
    let unreadable_file = tasks.join("unreadable.json");
    std::fs::write(&unreadable_file, b"{}").unwrap();
    std::fs::set_permissions(&unreadable_file, std::fs::Permissions::from_mode(0o000)).unwrap();

    let readable_anyway = std::fs::read_dir(&unreadable_file).is_ok()
        || std::fs::read_to_string(&unreadable_file).is_ok();
    let result = try_list_tasks(dir.path());

    std::fs::set_permissions(&unreadable_file, std::fs::Permissions::from_mode(0o600)).unwrap();
    if readable_anyway {
        return; // running as root / FS ignores perms — can't exercise EACCES
    }
    let tasks = result.expect("per-file errors must not fail the whole listing");
    assert_eq!(
        tasks.iter().map(|t| &t.slug).collect::<Vec<_>>(),
        vec![&good.slug],
        "corrupt and unreadable files are both skipped, the real task is kept"
    );
}

/// The real project tag for this test process's cwd (#949 reminder
/// scoping resolves it via [`project::current_tag`], which reads actual
/// cwd/`$HOME` — there's no injection point, so tests that need a task
/// attributed to "the current project" must resolve the same real tag
/// the code under test will use).
fn current_project() -> String {
    project::current_tag().expect("test")
}

/// Create a fresh, real session tagged to `project` (unlike `mk`'s fixed
/// "test-session" id, this session actually exists in the store — needed
/// for the #949 project-scoping tests, which filter against real session
/// records).
fn session_for_project(dir: &Path, project: &str) -> String {
    let outcome = start_session(dir, None, None, project, StartDecision::New).expect("test");
    let StartOutcome::Created(session) = outcome else {
        panic!("expected Created");
    };
    session.id
}

/// Create a `wip` task tagged to a real session in `project`.
fn wip_task_in_project(dir: &Path, title: &str, project: &str) -> Task {
    let session_id = session_for_project(dir, project);
    let task = add_task_for_session(dir, title, ParentSpec::Detached, &session_id).expect("test");
    start_task(dir, &task.slug, false).expect("test")
}

/// Create a `waiting` task tagged to a real session in `project`.
fn waiting_task_in_project(dir: &Path, title: &str, project: &str, reason: &str) -> Task {
    let session_id = session_for_project(dir, project);
    let task = add_task_for_session(dir, title, ParentSpec::Detached, &session_id).expect("test");
    start_task(dir, &task.slug, false).expect("test");
    wait_task(dir, &task.slug, reason).expect("test")
}

#[test]
fn task_state_default_is_open() {
    assert_eq!(TaskState::default(), TaskState::Open);
}

#[test]
fn task_state_serializes_snake_case() {
    assert_eq!(serde_json::to_string(&TaskState::Wip).unwrap(), "\"wip\"");
}

#[test]
fn slugify_basic_title() {
    assert_eq!(slugify("Fix login timeout"), "fix-login-timeout");
}

#[test]
fn slugify_truncates_to_six_words() {
    assert_eq!(
        slugify("one two three four five six seven eight"),
        "one-two-three-four-five-six"
    );
}

#[test]
fn slugify_strips_punctuation() {
    assert_eq!(slugify("Fix: login/timeout bug!"), "fix-login-timeout-bug");
}

#[test]
fn slugify_collapses_whitespace_and_trims_hyphens() {
    assert_eq!(slugify("  --weird   title--  "), "weird-title");
}

#[test]
fn add_task_creates_file_with_open_state() {
    let dir = TempDir::new().expect("test");
    let task = mk(dir.path(), "Fix login timeout", None).expect("test");
    assert_eq!(task.slug, "fix-login-timeout");
    assert_eq!(task.state, TaskState::Open);
    assert!(task.parent.is_none());

    let loaded = load_task(dir.path(), "fix-login-timeout").expect("test");
    assert_eq!(loaded, task);
}

/// `slug` is `pub`, so a caller with a `Task` in hand could mutate it to
/// a path-traversal payload before calling `save_task`/`load_task` — both
/// must reject it rather than build a path outside `tasks_dir` (security-
/// audit, #1465).
#[test]
fn load_task_rejects_a_path_traversal_slug() {
    let dir = TempDir::new().expect("test");
    let err = load_task(dir.path(), "../../etc/passwd").expect_err("must reject");
    assert!(err.to_string().contains("not a valid task slug"));
}

#[test]
fn save_task_rejects_a_task_with_a_path_traversal_slug() {
    let dir = TempDir::new().expect("test");
    let mut task = mk(dir.path(), "Fix login timeout", None).expect("test");
    task.slug = "../../etc/passwd".to_string();
    let err = save_task(dir.path(), &task).expect_err("must reject");
    assert!(err.to_string().contains("not a valid task slug"));
}

#[test]
fn add_task_uniquifies_slug_on_collision() {
    let dir = TempDir::new().expect("test");
    let t1 = mk(dir.path(), "Fix login timeout", None).expect("test");
    let t2 = mk(dir.path(), "Fix login timeout", None).expect("test");
    assert_eq!(t1.slug, "fix-login-timeout");
    assert_eq!(t2.slug, "fix-login-timeout-2");
}

/// Concurrency regression: multiple threads racing `add_task` with the
/// *same* title (the exact scenario multiple Claude Code sessions on one
/// project would hit) must never lose a task — the store lock serializes
/// each read-modify-write so `unique_slug` always sees every prior
/// writer's file before picking a suffix.
#[test]
fn concurrent_add_task_same_title_never_loses_a_task() {
    let dir = TempDir::new().expect("test");
    let dir_path = dir.path().to_path_buf();
    let threads: Vec<_> = (0..8)
        .map(|_| {
            let dir_path = dir_path.clone();
            std::thread::spawn(move || mk(&dir_path, "Race condition task", None).expect("test"))
        })
        .collect();
    let mut slugs: Vec<String> = threads
        .into_iter()
        .map(|h| h.join().expect("thread panicked").slug)
        .collect();
    slugs.sort();
    slugs.dedup();
    assert_eq!(
        slugs.len(),
        8,
        "every concurrent add_task must produce a distinct task"
    );
    assert_eq!(
        list_tasks(dir.path()).len(),
        8,
        "no task file was lost to a lost update"
    );
}

/// Concurrency regression: two threads racing `start_task` on the *same*
/// task must not lose the transition or corrupt the file — the lock
/// serializes the whole load-mutate-save cycle per operation.
#[test]
fn concurrent_start_task_same_task_is_serialized_not_corrupted() {
    let dir = TempDir::new().expect("test");
    let task = mk(dir.path(), "Shared task", None).expect("test");
    let dir_path = dir.path().to_path_buf();
    let slug = task.slug.clone();
    let threads: Vec<_> = (0..8)
        .map(|_| {
            let dir_path = dir_path.clone();
            let slug = slug.clone();
            std::thread::spawn(move || start_task(&dir_path, &slug, false))
        })
        .collect();
    let results: Vec<_> = threads
        .into_iter()
        .map(|h| h.join().expect("test"))
        .collect();
    assert!(
        results.iter().all(std::result::Result::is_ok),
        "start_task on an open task must always succeed, even racing itself"
    );
    let reloaded = load_task(dir.path(), &slug).expect("test");
    assert_eq!(reloaded.state, TaskState::Wip);
}

#[test]
fn add_task_with_parent() {
    let dir = TempDir::new().expect("test");
    let parent = mk(dir.path(), "Parent task", None).expect("test");
    let child = mk(dir.path(), "Child task", Some(&parent.slug)).expect("test");
    assert_eq!(child.parent, Some(parent.slug));
}

#[test]
fn add_task_with_unknown_parent_errors() {
    let dir = TempDir::new().expect("test");
    assert!(mk(dir.path(), "Orphan", Some("no-such-parent")).is_err());
}

#[test]
fn add_task_parent_accepts_unambiguous_prefix() {
    let dir = TempDir::new().expect("test");
    let parent = mk(dir.path(), "Umbrella project", None).expect("test");
    let child = mk(dir.path(), "Sub piece", Some("umbrella")).expect("test");
    assert_eq!(child.parent, Some(parent.slug));
}

#[test]
fn nested_chain_of_three_levels_resolves_correctly() {
    let dir = TempDir::new().expect("test");
    let grandparent = mk(dir.path(), "Epic", None).expect("test");
    let parent = mk(dir.path(), "Story", Some(&grandparent.slug)).expect("test");
    let child = mk(dir.path(), "Subtask", Some(&parent.slug)).expect("test");

    assert_eq!(grandparent.parent, None);
    assert_eq!(parent.parent, Some(grandparent.slug.clone()));
    assert_eq!(child.parent, Some(parent.slug.clone()));

    // Walk the chain back up via load_task, as a consumer would.
    let loaded_parent = load_task(dir.path(), child.parent.as_ref().expect("test")).expect("test");
    assert_eq!(loaded_parent.slug, parent.slug);
    let loaded_grandparent =
        load_task(dir.path(), loaded_parent.parent.as_ref().expect("test")).expect("test");
    assert_eq!(loaded_grandparent.slug, grandparent.slug);
    assert_eq!(loaded_grandparent.parent, None);
}

#[test]
fn multiple_children_under_one_parent_all_listed() {
    let dir = TempDir::new().expect("test");
    let parent = mk(dir.path(), "Parent with many kids", None).expect("test");
    let child_a = mk(dir.path(), "Child A", Some(&parent.slug)).expect("test");
    let child_b = mk(dir.path(), "Child B", Some(&parent.slug)).expect("test");
    let child_c = mk(dir.path(), "Child C", Some(&parent.slug)).expect("test");

    let children: Vec<Task> = list_tasks(dir.path())
        .into_iter()
        .filter(|t| t.parent.as_deref() == Some(parent.slug.as_str()))
        .collect();
    assert_eq!(children.len(), 3);
    let mut slugs: Vec<&str> = children.iter().map(|t| t.slug.as_str()).collect();
    slugs.sort();
    let mut expected = [
        child_a.slug.as_str(),
        child_b.slug.as_str(),
        child_c.slug.as_str(),
    ];
    expected.sort_unstable();
    assert_eq!(slugs, expected);
}

#[test]
fn list_tasks_skips_corrupt_files_with_warning() {
    let dir = TempDir::new().expect("test");
    mk(dir.path(), "Good task", None).expect("test");
    std::fs::create_dir_all(tasks_dir(dir.path())).expect("test");
    std::fs::write(tasks_dir(dir.path()).join("bad.json"), b"not json").expect("test");

    let tasks = list_tasks(dir.path());
    assert_eq!(tasks.len(), 1, "corrupt file must be skipped, not crash ls");
    assert_eq!(tasks[0].title, "Good task");
}

#[test]
fn list_tasks_empty_dir_returns_empty() {
    let dir = TempDir::new().expect("test");
    assert!(list_tasks(dir.path()).is_empty());
}

#[test]
fn resolve_identifier_exact_slug() {
    let dir = TempDir::new().expect("test");
    let task = mk(dir.path(), "Fix login timeout", None).expect("test");
    assert_eq!(
        resolve_identifier(dir.path(), &task.slug).expect("test"),
        task.slug
    );
}

#[test]
fn resolve_identifier_unambiguous_prefix() {
    let dir = TempDir::new().expect("test");
    let task = mk(dir.path(), "Fix login timeout", None).expect("test");
    assert_eq!(
        resolve_identifier(dir.path(), "fix-log").expect("test"),
        task.slug
    );
}

#[test]
fn resolve_identifier_ambiguous_prefix_errors_listing_candidates() {
    let dir = TempDir::new().expect("test");
    mk(dir.path(), "Fix login timeout", None).expect("test");
    mk(dir.path(), "Fix logout crash", None).expect("test");
    let err = resolve_identifier(dir.path(), "fix-log")
        .unwrap_err()
        .to_string();
    assert!(err.contains("fix-login-timeout"));
    assert!(err.contains("fix-logout-crash"));
}

#[test]
fn resolve_identifier_rejects_path_traversal() {
    let dir = TempDir::new().expect("test");
    let outside = dir.path().parent().expect("test").join("escaped.json");
    assert!(
        resolve_identifier(dir.path(), "../escaped").is_err(),
        "must reject a '..' identifier before touching the filesystem"
    );
    assert!(
        !outside.exists(),
        "must never create a file outside tasks_dir"
    );
}

#[test]
fn resolve_identifier_rejects_absolute_path() {
    let dir = TempDir::new().expect("test");
    assert!(resolve_identifier(dir.path(), "/etc/passwd").is_err());
}

#[test]
fn resolve_identifier_no_match_errors() {
    let dir = TempDir::new().expect("test");
    mk(dir.path(), "Fix login timeout", None).expect("test");
    assert!(resolve_identifier(dir.path(), "nope").is_err());
}

#[test]
fn start_task_transitions_open_to_wip() {
    let dir = TempDir::new().expect("test");
    let task = mk(dir.path(), "Do thing", None).expect("test");
    let started = start_task(dir.path(), &task.slug, false).expect("test");
    assert_eq!(started.state, TaskState::Wip);
}

#[test]
fn start_task_on_done_errors() {
    let dir = TempDir::new().expect("test");
    let task = mk(dir.path(), "Do thing", None).expect("test");
    done_task(dir.path(), &task.slug).expect("test");
    assert!(start_task(dir.path(), &task.slug, false).is_err());
}

#[test]
fn done_task_from_open_allowed() {
    let dir = TempDir::new().expect("test");
    let task = mk(dir.path(), "Do thing", None).expect("test");
    let done = done_task(dir.path(), &task.slug).expect("test");
    assert_eq!(done.state, TaskState::Done);
}

// #1164: blocked_on is a hard-block (an explicit dependency the user
// configured on purpose), unlike parent (soft-block, see cli/mod.rs's
// Start handler). Starting a task blocked on a not-done task must error.
/// A loop of done parent links is a store fault, not a finished blocker: the start names it
/// with the fix (#3601).
#[test]
fn start_task_names_a_loop_of_done_parent_links_with_its_fix() {
    let dir = TempDir::new().expect("test");
    let a = mk(dir.path(), "Loop a", None).expect("test");
    let b = mk(dir.path(), "Loop b", None).expect("test");
    let waiter = mk(dir.path(), "Waiter", None).expect("test");
    for slug in [&a.slug, &b.slug] {
        let mut t = load_task(dir.path(), slug).expect("test");
        t.state = TaskState::Done;
        save_task(dir.path(), &t).expect("test");
    }
    let mut ta = load_task(dir.path(), &a.slug).expect("test");
    ta.parent = Some(b.slug.clone());
    save_task(dir.path(), &ta).expect("test");
    let mut tb = load_task(dir.path(), &b.slug).expect("test");
    tb.parent = Some(a.slug.clone());
    save_task(dir.path(), &tb).expect("test");
    block_task(dir.path(), &waiter.slug, &a.slug).expect("test");

    let err = start_task(dir.path(), &waiter.slug, false).unwrap_err();

    let msg = format!("{err:#}");
    assert!(
        msg.contains("loop"),
        "error should say the links loop: {msg}"
    );
    assert!(
        msg.contains("--no-parent"),
        "error should give the fix: {msg}"
    );
}

#[test]
fn start_task_errors_when_blocked_on_open_task() {
    let dir = TempDir::new().expect("test");
    let blocker = mk(dir.path(), "Blocker task", None).expect("test");
    let task = mk(dir.path(), "Blocked task", None).expect("test");
    block_task(dir.path(), &task.slug, &blocker.slug).expect("test");
    let err = start_task(dir.path(), &task.slug, false).unwrap_err();
    assert!(
        err.to_string().contains(&blocker.slug),
        "error should name the unmet blocker: {err}"
    );
}

#[test]
fn start_task_force_overrides_unmet_blocker() {
    let dir = TempDir::new().expect("test");
    let blocker = mk(dir.path(), "Blocker task", None).expect("test");
    let task = mk(dir.path(), "Blocked task", None).expect("test");
    block_task(dir.path(), &task.slug, &blocker.slug).expect("test");
    let started = start_task(dir.path(), &task.slug, true).expect("test");
    assert_eq!(started.state, TaskState::Wip);
}

// #1909: repro for the reported hard-block — a blocker that passed through
// `waiting` before `done` must resolve exactly like a blocker that went
// straight from open to done.
#[test]
fn start_task_allows_when_blocker_went_through_waiting_before_done() {
    let dir = TempDir::new().expect("test");
    let blocker = mk(dir.path(), "Blocker task", None).expect("test");
    let task = mk(dir.path(), "Blocked task", None).expect("test");
    block_task(dir.path(), &task.slug, &blocker.slug).expect("test");
    start_task(dir.path(), &blocker.slug, false).expect("test");
    wait_task(dir.path(), &blocker.slug, "needs input").expect("test");
    done_task(dir.path(), &blocker.slug).expect("test");
    let started = start_task(dir.path(), &task.slug, false).expect("test");
    assert_eq!(started.state, TaskState::Wip);
}

#[test]
fn start_task_allows_when_blocker_is_done() {
    let dir = TempDir::new().expect("test");
    let blocker = mk(dir.path(), "Blocker task", None).expect("test");
    let task = mk(dir.path(), "Blocked task", None).expect("test");
    block_task(dir.path(), &task.slug, &blocker.slug).expect("test");
    done_task(dir.path(), &blocker.slug).expect("test");
    let started = start_task(dir.path(), &task.slug, false).expect("test");
    assert_eq!(started.state, TaskState::Wip);
}

// #1164 fan-out design: blocking on a parent task with children means
// "blocked until the parent AND all its children are done" -- so a
// downstream task can hard-block on the whole set via one edge to the
// parent, without hand-wiring a block edge per sibling.
#[test]
fn start_task_errors_when_blocker_has_undone_child() {
    let dir = TempDir::new().expect("test");
    let parent = mk(dir.path(), "Parent step", None).expect("test");
    let child = mk(dir.path(), "Sibling task", Some(&parent.slug)).expect("test");
    done_task(dir.path(), &parent.slug).expect("test");
    let downstream = mk(dir.path(), "Downstream", None).expect("test");
    block_task(dir.path(), &downstream.slug, &parent.slug).expect("test");
    let err = start_task(dir.path(), &downstream.slug, false).unwrap_err();
    assert!(
        err.to_string().contains(&parent.slug),
        "error should name the unmet blocker: {err}"
    );
    // Confirm the setup actually holds: child is the reason the parent
    // isn't considered fully done yet.
    assert_ne!(
        load_task(dir.path(), &child.slug).expect("test").state,
        TaskState::Done
    );
}

#[test]
fn start_task_allows_when_blocker_and_all_children_done() {
    let dir = TempDir::new().expect("test");
    let parent = mk(dir.path(), "Parent step", None).expect("test");
    let child = mk(dir.path(), "Sibling task", Some(&parent.slug)).expect("test");
    done_task(dir.path(), &child.slug).expect("test");
    done_task(dir.path(), &parent.slug).expect("test");
    let downstream = mk(dir.path(), "Downstream", None).expect("test");
    block_task(dir.path(), &downstream.slug, &parent.slug).expect("test");
    let started = start_task(dir.path(), &downstream.slug, false).expect("test");
    assert_eq!(started.state, TaskState::Wip);
}

// #2300: `task add` without `--parent` chains the new task under the one
// added before it, so a task blocked on its own parent is inside the
// blocker's subtree. That subtree cannot be done before the task starts.
#[test]
fn start_task_allows_when_blocked_on_own_done_parent() {
    let dir = TempDir::new().expect("test");
    let a = mk(dir.path(), "setup: A", None).expect("test");
    let b = mk(dir.path(), "setup: B", Some(&a.slug)).expect("test");
    block_task(dir.path(), &b.slug, &a.slug).expect("test");
    start_task(dir.path(), &a.slug, false).expect("test");
    done_task(dir.path(), &a.slug).expect("test");
    let started = start_task(dir.path(), &b.slug, false).expect("test");
    assert_eq!(started.state, TaskState::Wip);
}

#[test]
fn start_task_allows_every_link_of_an_implicit_chain() {
    let dir = TempDir::new().expect("test");
    let mut prev: Option<Task> = None;
    let mut chain = Vec::new();
    for title in ["step A", "step B", "step C", "step D"] {
        let t = mk(dir.path(), title, prev.as_ref().map(|p| p.slug.as_str())).expect("test");
        if let Some(p) = &prev {
            block_task(dir.path(), &t.slug, &p.slug).expect("test");
        }
        chain.push(t.clone());
        prev = Some(t);
    }
    for t in &chain {
        start_task(dir.path(), &t.slug, false).expect("each link must start");
        done_task(dir.path(), &t.slug).expect("test");
    }
}

#[test]
fn start_task_still_blocks_on_undone_sibling_under_done_parent() {
    let dir = TempDir::new().expect("test");
    let a = mk(dir.path(), "Parent A", None).expect("test");
    let b = mk(dir.path(), "Child B", Some(&a.slug)).expect("test");
    let c = mk(dir.path(), "Child C", Some(&a.slug)).expect("test");
    block_task(dir.path(), &b.slug, &a.slug).expect("test");
    done_task(dir.path(), &a.slug).expect("test");
    let err = start_task(dir.path(), &b.slug, false)
        .unwrap_err()
        .to_string();
    // A is done, so the error must name the descendant that holds it open.
    let want = format!("{} (its descendant '{}' is not done)", a.slug, c.slug);
    assert!(err.contains(&want), "{err}");
}

#[test]
fn is_actionable_ignores_own_subtree_under_done_blocker() {
    let dir = TempDir::new().expect("test");
    let a = mk(dir.path(), "Parent A", None).expect("test");
    let b = mk(dir.path(), "Child B", Some(&a.slug)).expect("test");
    mk(dir.path(), "Grandchild", Some(&b.slug)).expect("test");
    block_task(dir.path(), &b.slug, &a.slug).expect("test");
    done_task(dir.path(), &a.slug).expect("test");
    let tasks = list_tasks(dir.path());
    let by_slug: HashMap<&str, &Task> = tasks.iter().map(|t| (t.slug.as_str(), t)).collect();
    assert!(blockers::unmet_blocker(by_slug[b.slug.as_str()], &by_slug).is_none());
}

#[test]
fn start_task_still_blocks_on_own_parent_that_is_not_done() {
    let dir = TempDir::new().expect("test");
    let a = mk(dir.path(), "Parent A", None).expect("test");
    let b = mk(dir.path(), "Child B", Some(&a.slug)).expect("test");
    block_task(dir.path(), &b.slug, &a.slug).expect("test");
    assert!(start_task(dir.path(), &b.slug, false).is_err());
}

#[test]
fn start_task_under_a_deleted_parent_still_starts() {
    let dir = TempDir::new().expect("test");
    let a = mk(dir.path(), "Parent A", None).expect("test");
    let b = mk(dir.path(), "Child B", Some(&a.slug)).expect("test");
    std::fs::remove_file(task_path(dir.path(), &a.slug)).expect("test");
    let started = start_task(dir.path(), &b.slug, false).expect("dangling link tolerated");
    assert_eq!(started.state, TaskState::Wip);
}

#[test]
fn start_task_under_a_corrupt_parent_fails_and_names_the_parent() {
    let dir = TempDir::new().expect("test");
    let a = mk(dir.path(), "Parent A", None).expect("test");
    let b = mk(dir.path(), "Child B", Some(&a.slug)).expect("test");
    std::fs::write(task_path(dir.path(), &a.slug), "{ not json").expect("test");
    let err = start_task(dir.path(), &b.slug, false).expect_err("corrupt parent refused");
    let text = format!("{err:#}");
    assert!(text.contains(&a.slug), "error must name the parent: {text}");
    let on_disk = load_task(dir.path(), &b.slug).expect("test");
    assert_eq!(on_disk.state, TaskState::Open, "child must not be started");
}

#[test]
fn note_task_appends_note() {
    let dir = TempDir::new().expect("test");
    let task = mk(dir.path(), "Do thing", None).expect("test");
    let updated = note_task(dir.path(), &task.slug, "made progress").expect("test");
    assert_eq!(updated.notes.len(), 1);
    assert_eq!(updated.notes[0].text, "made progress");
}

#[test]
fn wait_task_transitions_to_waiting_and_notes_reason() {
    let dir = TempDir::new().expect("test");
    let task = mk(dir.path(), "Do thing", None).expect("test");
    start_task(dir.path(), &task.slug, false).expect("test");
    let waiting = wait_task(dir.path(), &task.slug, "need spec review").expect("test");
    assert_eq!(waiting.state, TaskState::Waiting);
    assert_eq!(waiting.notes.len(), 1);
    assert!(waiting.notes[0].text.contains("need spec review"));
}

#[test]
fn wait_task_on_done_errors() {
    let dir = TempDir::new().expect("test");
    let task = mk(dir.path(), "Do thing", None).expect("test");
    done_task(dir.path(), &task.slug).expect("test");
    assert!(wait_task(dir.path(), &task.slug, "too late").is_err());
}

#[test]
fn start_task_resumes_from_waiting() {
    let dir = TempDir::new().expect("test");
    let task = mk(dir.path(), "Do thing", None).expect("test");
    wait_task(dir.path(), &task.slug, "blocked").expect("test");
    let resumed = start_task(dir.path(), &task.slug, false).expect("test");
    assert_eq!(resumed.state, TaskState::Wip);
}

#[test]
fn block_task_records_dependency() {
    let dir = TempDir::new().expect("test");
    let blocker = mk(dir.path(), "Blocker", None).expect("test");
    let task = mk(dir.path(), "Blocked", None).expect("test");
    let updated = block_task(dir.path(), &task.slug, &blocker.slug).expect("test");
    assert_eq!(updated.blocked_on, vec![blocker.slug]);
}

#[test]
fn block_task_on_unknown_target_errors() {
    let dir = TempDir::new().expect("test");
    let task = mk(dir.path(), "Blocked", None).expect("test");
    assert!(block_task(dir.path(), &task.slug, "no-such-task").is_err());
}

#[test]
fn block_task_on_self_errors() {
    let dir = TempDir::new().expect("test");
    let task = mk(dir.path(), "Solo task", None).expect("test");
    assert!(block_task(dir.path(), &task.slug, &task.slug).is_err());
}

#[test]
fn edit_task_retitles() {
    let dir = TempDir::new().expect("test");
    let task = mk(dir.path(), "Original title", None).expect("test");
    let edit = TaskEdit {
        title: Some("New title"),
        ..Default::default()
    };
    let updated = edit_task(dir.path(), &task.slug, &edit).expect("test");
    assert_eq!(updated.title, "New title");
}

#[test]
fn a_task_added_with_a_detail_stores_it_in_the_same_save() {
    let dir = TempDir::new().expect("test");
    let session = session::start_session(
        dir.path(),
        Some("s"),
        None,
        "p",
        session::StartDecision::Auto,
    )
    .expect("start");
    let session::StartOutcome::Created(session) = session else {
        panic!("expected Created");
    };
    let new = NewTask {
        title: "With detail",
        detail: Some("files: a.rs"),
        ..NewTask::default()
    };
    let task = add_task_for_session_with(dir.path(), &new, ParentSpec::Detached, &session.id)
        .expect("add");
    assert_eq!(task.detail.as_deref(), Some("files: a.rs"));
    assert_eq!(
        load_task(dir.path(), &task.slug)
            .expect("load")
            .detail
            .as_deref(),
        Some("files: a.rs")
    );
}

#[test]
fn an_empty_detail_on_a_new_task_means_no_detail() {
    let dir = TempDir::new().expect("test");
    let task = mk(dir.path(), "Plain", None).expect("test");
    assert_eq!(task.detail, None);
    let new = NewTask {
        title: "Empty detail",
        detail: Some(""),
        ..NewTask::default()
    };
    let session_id = task.session.clone().expect("session");
    let added = add_task_for_session_with(dir.path(), &new, ParentSpec::Detached, &session_id)
        .expect("add");
    assert_eq!(added.detail, None);
}

#[test]
fn edit_task_sets_replaces_and_clears_the_detail() {
    let dir = TempDir::new().expect("test");
    let task = mk(dir.path(), "Has detail", None).expect("test");
    assert_eq!(task.detail, None);
    let set = |text| TaskEdit {
        detail: Some(text),
        ..Default::default()
    };
    let first = edit_task(dir.path(), &task.slug, &set("files: a.rs")).expect("test");
    assert_eq!(first.detail.as_deref(), Some("files: a.rs"));
    let second = edit_task(dir.path(), &task.slug, &set("files: b.rs")).expect("test");
    assert_eq!(second.detail.as_deref(), Some("files: b.rs"));
    let cleared = edit_task(dir.path(), &task.slug, &set("")).expect("test");
    assert_eq!(cleared.detail, None);
}

#[test]
fn an_edit_without_detail_keeps_the_detail() {
    let dir = TempDir::new().expect("test");
    let task = mk(dir.path(), "Keeps detail", None).expect("test");
    let set = TaskEdit {
        detail: Some("keep me"),
        ..Default::default()
    };
    edit_task(dir.path(), &task.slug, &set).expect("test");
    let retitle = TaskEdit {
        title: Some("Retitled"),
        ..Default::default()
    };
    let after = edit_task(dir.path(), &task.slug, &retitle).expect("test");
    assert_eq!(after.detail.as_deref(), Some("keep me"));
}

#[test]
fn a_task_file_written_before_detail_existed_still_loads() {
    let old = r#"{"slug":"old","title":"Old","created_at":"2026-01-01T00:00:00Z",
        "updated_at":"2026-01-01T00:00:00Z"}"#;
    let task: Task = serde_json::from_str(old).expect("old file loads");
    assert_eq!(task.detail, None);
}

#[test]
fn a_task_without_detail_does_not_write_the_field() {
    let dir = TempDir::new().expect("test");
    let task = mk(dir.path(), "Plain", None).expect("test");
    assert!(
        !serde_json::to_string(&task)
            .expect("test")
            .contains("detail")
    );
}

#[test]
fn edit_task_sets_and_clears_parent() {
    let dir = TempDir::new().expect("test");
    let parent = mk(dir.path(), "Parent", None).expect("test");
    let task = mk(dir.path(), "Child", None).expect("test");

    let edit = TaskEdit {
        parent: Some(&parent.slug),
        ..Default::default()
    };
    let updated = edit_task(dir.path(), &task.slug, &edit).expect("test");
    assert_eq!(updated.parent, Some(parent.slug.clone()));

    let edit = TaskEdit {
        no_parent: true,
        ..Default::default()
    };
    let updated = edit_task(dir.path(), &task.slug, &edit).expect("test");
    assert_eq!(updated.parent, None);
}

#[test]
fn edit_task_parent_to_unknown_id_errors() {
    let dir = TempDir::new().expect("test");
    let task = mk(dir.path(), "Task", None).expect("test");
    let edit = TaskEdit {
        parent: Some("no-such-task"),
        ..Default::default()
    };
    assert!(edit_task(dir.path(), &task.slug, &edit).is_err());
}

#[test]
fn edit_task_parent_to_self_errors() {
    let dir = TempDir::new().expect("test");
    let task = mk(dir.path(), "Task", None).expect("test");
    let edit = TaskEdit {
        parent: Some(&task.slug),
        ..Default::default()
    };
    assert!(edit_task(dir.path(), &task.slug, &edit).is_err());
}

#[test]
fn edit_task_parent_creating_a_cycle_errors() {
    let dir = TempDir::new().expect("test");
    let a = mk(dir.path(), "A", None).expect("test");
    let b = mk(dir.path(), "B", Some(&a.slug)).expect("test");
    // A -> B already; making A's parent B would make A its own ancestor.
    let edit = TaskEdit {
        parent: Some(&b.slug),
        ..Default::default()
    };
    assert!(edit_task(dir.path(), &a.slug, &edit).is_err());
}

#[test]
fn edit_task_adds_and_removes_blocked_on() {
    let dir = TempDir::new().expect("test");
    let blocker = mk(dir.path(), "Blocker", None).expect("test");
    let task = mk(dir.path(), "Blocked", None).expect("test");

    let block_on = vec![blocker.slug.clone()];
    let edit = TaskEdit {
        block_on: &block_on,
        ..Default::default()
    };
    let updated = edit_task(dir.path(), &task.slug, &edit).expect("test");
    assert_eq!(updated.blocked_on, vec![blocker.slug.clone()]);

    let unblock = vec![blocker.slug.clone()];
    let edit = TaskEdit {
        unblock: &unblock,
        ..Default::default()
    };
    let updated = edit_task(dir.path(), &task.slug, &edit).expect("test");
    assert!(updated.blocked_on.is_empty());
}

#[test]
fn edit_task_block_on_is_idempotent() {
    let dir = TempDir::new().expect("test");
    let blocker = mk(dir.path(), "Blocker", None).expect("test");
    let task = mk(dir.path(), "Blocked", None).expect("test");
    let block_on = vec![blocker.slug.clone(), blocker.slug.clone()];
    let edit = TaskEdit {
        block_on: &block_on,
        ..Default::default()
    };
    let updated = edit_task(dir.path(), &task.slug, &edit).expect("test");
    assert_eq!(updated.blocked_on, vec![blocker.slug]);
}

#[test]
fn edit_task_unblock_absent_id_is_a_noop() {
    let dir = TempDir::new().expect("test");
    let task = mk(dir.path(), "Task", None).expect("test");
    let other = mk(dir.path(), "Other", None).expect("test");
    let unblock = vec![other.slug];
    let edit = TaskEdit {
        unblock: &unblock,
        ..Default::default()
    };
    assert!(edit_task(dir.path(), &task.slug, &edit).is_ok());
}

#[test]
fn edit_task_block_on_self_errors() {
    let dir = TempDir::new().expect("test");
    let task = mk(dir.path(), "Task", None).expect("test");
    let block_on = vec![task.slug.clone()];
    let edit = TaskEdit {
        block_on: &block_on,
        ..Default::default()
    };
    assert!(edit_task(dir.path(), &task.slug, &edit).is_err());
}

#[test]
fn edit_task_block_on_unknown_id_errors() {
    let dir = TempDir::new().expect("test");
    let task = mk(dir.path(), "Task", None).expect("test");
    let block_on = vec!["no-such-task".to_string()];
    let edit = TaskEdit {
        block_on: &block_on,
        ..Default::default()
    };
    assert!(edit_task(dir.path(), &task.slug, &edit).is_err());
}

#[test]
fn edit_task_adds_a_note() {
    let dir = TempDir::new().expect("test");
    let task = mk(dir.path(), "Task", None).expect("test");
    let edit = TaskEdit {
        add_note: Some("progress note"),
        ..Default::default()
    };
    let updated = edit_task(dir.path(), &task.slug, &edit).expect("test");
    assert_eq!(updated.notes.len(), 1);
    assert_eq!(updated.notes[0].text, "progress note");
}

#[test]
fn edit_task_deletes_a_note_by_index() {
    let dir = TempDir::new().expect("test");
    let task = mk(dir.path(), "Task", None).expect("test");
    note_task(dir.path(), &task.slug, "first").expect("test");
    note_task(dir.path(), &task.slug, "second").expect("test");
    let edit = TaskEdit {
        delete_note: Some("0"),
        ..Default::default()
    };
    let updated = edit_task(dir.path(), &task.slug, &edit).expect("test");
    assert_eq!(updated.notes.len(), 1);
    assert_eq!(updated.notes[0].text, "second");
}

#[test]
fn edit_task_deletes_a_note_by_timestamp() {
    let dir = TempDir::new().expect("test");
    let task = mk(dir.path(), "Task", None).expect("test");
    let with_note = note_task(dir.path(), &task.slug, "only note").expect("test");
    let at = with_note.notes[0].at.clone();
    let edit = TaskEdit {
        delete_note: Some(&at),
        ..Default::default()
    };
    let updated = edit_task(dir.path(), &task.slug, &edit).expect("test");
    assert!(updated.notes.is_empty());
}

#[test]
fn edit_task_delete_note_out_of_range_errors() {
    let dir = TempDir::new().expect("test");
    let task = mk(dir.path(), "Task", None).expect("test");
    let edit = TaskEdit {
        delete_note: Some("0"),
        ..Default::default()
    };
    assert!(edit_task(dir.path(), &task.slug, &edit).is_err());
}

#[test]
fn edit_task_delete_note_unknown_timestamp_errors() {
    let dir = TempDir::new().expect("test");
    let task = mk(dir.path(), "Task", None).expect("test");
    note_task(dir.path(), &task.slug, "only note").expect("test");
    let edit = TaskEdit {
        delete_note: Some("2020-01-01T00:00:00Z"),
        ..Default::default()
    };
    assert!(edit_task(dir.path(), &task.slug, &edit).is_err());
}

#[test]
fn edit_task_with_no_fields_is_a_noop_but_bumps_updated_at() {
    let dir = TempDir::new().expect("test");
    let task = mk(dir.path(), "Task", None).expect("test");
    let original_updated_at = task.updated_at.clone();
    let edit = TaskEdit::default();
    let updated = edit_task(dir.path(), &task.slug, &edit).expect("test");
    assert_eq!(updated.title, task.title);
    assert_eq!(updated.parent, task.parent);
    assert_eq!(updated.blocked_on, task.blocked_on);
    assert_eq!(updated.notes, task.notes);
    assert_ne!(updated.updated_at, original_updated_at);
}

#[test]
fn edit_task_on_unknown_task_errors() {
    let dir = TempDir::new().expect("test");
    let edit = TaskEdit::default();
    assert!(edit_task(dir.path(), "no-such-task", &edit).is_err());
}

#[test]
fn add_task_with_no_alphanumeric_title_falls_back_to_timestamp_slug() {
    let dir = TempDir::new().expect("test");
    let task = mk(dir.path(), "!!!", None).expect("test");
    assert!(!task.slug.is_empty());
    assert!(task.slug.starts_with("task-"));
    let loaded = load_task(dir.path(), &task.slug).expect("test");
    assert_eq!(loaded, task);
}

// A dangling blocked_on reference (deleted/corrupt blocker file) fails
// closed -- same rationale as unmet_blocker's dangling-blocker handling
// for `task show --next` -- rather than silently treating it as resolved.
#[test]
fn start_task_errors_when_blocked_on_deleted_task() {
    let dir = TempDir::new().expect("test");
    let blocker = mk(dir.path(), "Blocker", None).expect("test");
    let task = mk(dir.path(), "Blocked", None).expect("test");
    block_task(dir.path(), &task.slug, &blocker.slug).expect("test");
    std::fs::remove_file(
        dir.path()
            .join("tasks")
            .join(format!("{}.json", blocker.slug)),
    )
    .expect("test");
    let err = start_task(dir.path(), &task.slug, false).unwrap_err();
    assert!(err.to_string().contains(&blocker.slug));
}

#[test]
fn start_task_force_overrides_dangling_blocker() {
    let dir = TempDir::new().expect("test");
    let blocker = mk(dir.path(), "Blocker", None).expect("test");
    let task = mk(dir.path(), "Blocked", None).expect("test");
    block_task(dir.path(), &task.slug, &blocker.slug).expect("test");
    std::fs::remove_file(
        dir.path()
            .join("tasks")
            .join(format!("{}.json", blocker.slug)),
    )
    .expect("test");
    let started = start_task(dir.path(), &task.slug, true).expect("test");
    assert_eq!(started.state, TaskState::Wip);
}

#[test]
fn session_start_reminder_empty_when_no_wip_tasks() {
    let dir = TempDir::new().expect("test");
    mk(dir.path(), "Open task", None).expect("test");
    assert!(session_start_reminder(dir.path()).is_empty());
}

#[test]
fn session_start_reminder_lists_wip_tasks() {
    let dir = TempDir::new().expect("test");
    let task = wip_task_in_project(dir.path(), "In progress task", &current_project());
    let reminder = session_start_reminder(dir.path());
    assert!(reminder.contains("In progress task"));
    assert!(reminder.contains(&task.slug));
}

#[test]
fn session_start_reminder_includes_resume_context_for_an_open_session() {
    let dir = TempDir::new().expect("test");
    let project = project::current_tag().expect("project tag");
    let owner = session::EngineIdentity::default();
    let resume = resume::ResumeContext {
        context: Some("pick up at step 4".to_string()),
        issues: vec![2339],
        ..Default::default()
    };
    let request = session::StartRequest {
        name: Some("s"),
        description: None,
        project: &project,
        owner: &owner,
        resume: &resume,
    };
    session::start_session_as(dir.path(), &request, session::StartDecision::Auto).expect("start");
    let reminder = session_start_reminder(dir.path());
    assert!(reminder.contains("pick up at step 4"), "{reminder}");
    assert!(reminder.contains("gh issue view 2339"), "{reminder}");
}

fn start_session_here(dir: &Path, resume: &resume::ResumeContext) -> session::Session {
    let project = project::current_tag().expect("project tag");
    let owner = session::EngineIdentity::default();
    let request = session::StartRequest {
        name: Some("s"),
        description: None,
        project: &project,
        owner: &owner,
        resume,
    };
    match session::start_session_as(dir, &request, session::StartDecision::Auto).expect("start") {
        session::StartOutcome::Created(s) => s,
        other => panic!("expected Created, got {other:?}"),
    }
}

#[test]
fn stop_reminder_names_a_session_with_open_tasks_and_no_resume_context() {
    let dir = TempDir::new().expect("test");
    let created = start_session_here(dir.path(), &resume::ResumeContext::default());
    add_task_for_session(dir.path(), "Step", ParentSpec::Detached, &created.id).expect("add");
    let reminder = stop_hook_reminder(&Stored::read(dir.path()), None);
    assert!(reminder.contains("no resume context"), "{reminder}");
    assert!(reminder.contains("task session edit"), "{reminder}");
    assert!(
        reminder.contains("recognize"),
        "must not presume ownership: {reminder}"
    );
}

#[test]
fn stop_reminder_stays_quiet_about_context_when_it_is_set_or_no_task_is_open() {
    let with_context = TempDir::new().expect("test");
    let resume = resume::ResumeContext {
        issues: vec![1],
        ..Default::default()
    };
    let created = start_session_here(with_context.path(), &resume);
    add_task_for_session(
        with_context.path(),
        "Step",
        ParentSpec::Detached,
        &created.id,
    )
    .expect("add");
    assert!(
        !stop_hook_reminder(&Stored::read(with_context.path()), None).contains("no resume context")
    );

    let no_tasks = TempDir::new().expect("test");
    start_session_here(no_tasks.path(), &resume::ResumeContext::default());
    assert!(
        !stop_hook_reminder(&Stored::read(no_tasks.path()), None).contains("no resume context")
    );

    let all_done = TempDir::new().expect("test");
    let created = start_session_here(all_done.path(), &resume::ResumeContext::default());
    let task = add_task_for_session(all_done.path(), "Step", ParentSpec::Detached, &created.id)
        .expect("add");
    complete_task(all_done.path(), &task.slug, true).expect("done");
    assert!(
        !stop_hook_reminder(&Stored::read(all_done.path()), None).contains("no resume context")
    );
}

#[test]
fn session_start_reminder_empty_on_missing_state_dir() {
    let dir = TempDir::new().expect("test");
    let missing = dir.path().join("does-not-exist");
    assert!(session_start_reminder(&missing).is_empty());
}

#[test]
fn stop_hook_reminder_empty_when_store_is_empty() {
    let dir = TempDir::new().expect("test");
    assert!(stop_hook_reminder(&Stored::read(dir.path()), None).is_empty());
}

// --- open tasks, nothing started (#2338) ---

/// A real session in `project` holding two `open` tasks, chained the way
/// `task add` chains them by default. Returns (session id, first, second).
fn idle_session_in_project(dir: &Path, project: &str) -> (String, Task, Task) {
    let session_id = session_for_project(dir, project);
    let first =
        add_task_for_session(dir, "Step 1", ParentSpec::Detached, &session_id).expect("test");
    let second =
        add_task_for_session(dir, "Step 2", ParentSpec::Detached, &session_id).expect("test");
    (session_id, first, second)
}

#[test]
fn stop_hook_reminder_flags_open_tasks_when_none_is_wip() {
    let dir = TempDir::new().expect("test");
    let (session_id, first, second) = idle_session_in_project(dir.path(), &current_project());

    let reminder = stop_hook_reminder(&Stored::read(dir.path()), None);
    assert!(
        reminder.contains(&session_id),
        "must name the session: {reminder}"
    );
    assert!(
        reminder.contains(&format!("llmenv task start {}", first.slug)),
        "must name the next open task to start: {reminder}"
    );
    assert!(
        !reminder.contains(&second.slug),
        "names the next task only: {reminder}"
    );
    assert!(reminder.contains("2 open task(s)"), "{reminder}");
    assert!(
        !reminder.contains("session finish"),
        "a session with open tasks must not be offered `session finish` (#2416): {reminder}"
    );
    assert!(
        reminder.contains("recognize"),
        "must not presume ownership: {reminder}"
    );
}

// #3764: an idle session whose only open task waits on an undone blocker names the reason, and
// it does not advise a start that the start refuses.
#[test]
fn an_idle_reminder_names_the_unmet_blocker_and_not_a_start() {
    let dir = TempDir::new().expect("test");
    let session_id = session_for_project(dir.path(), &current_project());
    let waiter = add_task_for_session(dir.path(), "Waiter", ParentSpec::Detached, &session_id)
        .expect("test");
    let mut blocker = add_task_for_session(
        dir.path(),
        "Elsewhere",
        ParentSpec::Detached,
        "other-session",
    )
    .expect("test");
    blocker.state = TaskState::Wip;
    save_task(dir.path(), &blocker).expect("test");
    block_task(dir.path(), &waiter.slug, &blocker.slug).expect("test");

    let reminder = stop_hook_reminder(&Stored::read(dir.path()), None);
    assert!(
        reminder.contains(&format!("'{}' cannot start", waiter.slug)),
        "{reminder}"
    );
    assert!(
        reminder.contains(&format!("waits on '{}'", blocker.slug)),
        "{reminder}"
    );
    assert!(
        !reminder.contains(&format!("llmenv task start {}", waiter.slug)),
        "{reminder}"
    );
}

// #2608: an unreadable store must say so in the reminder, not print that no session is open.
#[test]
fn unreadable_store_reminder_warns_instead_of_reporting_no_work() {
    let dir = TempDir::new().expect("test");
    std::fs::write(dir.path().join("tasks"), "x").expect("test");
    let stored = Stored::read(dir.path());
    let reminder = idle_reminder_lines(&current_project(), None, &stored);
    assert!(reminder.contains("task store cannot be read"), "{reminder}");
    assert!(!reminder.contains("has no tasks"), "{reminder}");
}

// #2468: the first idle reminder must already offer `wait`, for a step that needs the user.
#[test]
fn first_idle_reminder_offers_start_and_wait() {
    let dir = TempDir::new().expect("test");
    let (_, first, _) = idle_session_in_project(dir.path(), &current_project());

    let reminder = stop_hook_reminder(&Stored::read(dir.path()), None);
    let start = format!("llmenv task start {}", first.slug);
    let wait = format!("llmenv task wait {} \"<reason>\"", first.slug);
    assert!(reminder.contains(&start), "{reminder}");
    assert!(reminder.contains(&wait), "{reminder}");
    assert!(
        reminder.contains("needs the user"),
        "must say when `wait` applies: {reminder}"
    );
}

#[test]
fn stop_hook_reminder_skips_idle_check_for_session_with_wip_task() {
    let dir = TempDir::new().expect("test");
    let (session_id, first, second) = idle_session_in_project(dir.path(), &current_project());
    start_task(dir.path(), &first.slug, false).expect("test");

    let reminder = stop_hook_reminder(&Stored::read(dir.path()), None);
    assert!(
        reminder.contains(&first.slug),
        "wip reminder still fires: {reminder}"
    );
    assert!(!reminder.contains(&second.slug), "{reminder}");
    assert!(!reminder.contains("none in progress"), "{reminder}");
    assert!(reminder.contains(&session_id));
}

#[test]
fn stop_hook_reminder_skips_idle_check_for_session_with_waiting_task() {
    let dir = TempDir::new().expect("test");
    let (_, first, second) = idle_session_in_project(dir.path(), &current_project());
    start_task(dir.path(), &first.slug, false).expect("test");
    wait_task(dir.path(), &first.slug, "spec review").expect("test");

    // A `waiting` task means the session is paused on purpose; stay quiet.
    let reminder = stop_hook_reminder(&Stored::read(dir.path()), None);
    assert!(!reminder.contains(&second.slug), "{reminder}");
}

#[test]
fn stop_hook_reminder_skips_idle_session_from_other_project() {
    let dir = TempDir::new().expect("test");
    let (_, first, _) = idle_session_in_project(dir.path(), "other-project-0000000000");
    assert!(!stop_hook_reminder(&Stored::read(dir.path()), None).contains(&first.slug));
}

#[test]
fn idle_sessions_name_the_obstacle_when_a_blocker_and_the_queue_refuse_every_task() {
    let dir = TempDir::new().expect("test");
    let project = current_project();
    let (session_id, first, second) = idle_session_in_project(dir.path(), &project);
    block_task(dir.path(), &first.slug, &second.slug).expect("test");

    let open = session::open_sessions_strict(dir.path(), &project).expect("test");
    let idle = idle_sessions(open, &list_tasks(dir.path()));
    assert_eq!(idle.len(), 1);
    assert_eq!(idle[0].session.id, session_id);
    // The first task waits on the second, and the queue holds the second behind the first, so
    // the start refuses both. The reminder keeps the first task and names why it cannot
    // start (#3770), where it used to advise a start of the second that the queue refused.
    assert!(idle[0].next.is_none());
    let (slug, why) = idle[0].loop_error.as_ref().expect("an obstacle is named");
    assert_eq!(slug, &first.slug);
    assert!(
        why.contains(&format!("waits on '{}'", second.slug)),
        "{why}"
    );
    assert_eq!(idle[0].open_count, 2);
}

fn child_new<'a>(title: &'a str, parent: &'a str) -> NewTask<'a> {
    NewTask {
        title,
        placement: Placement::Child(parent),
        ..NewTask::default()
    }
}

#[test]
fn the_stop_reminder_names_a_parent_with_only_unstarted_sub_tasks() {
    let dir = TempDir::new().expect("test");
    let session_id = session_for_project(dir.path(), &current_project());
    let parent = add_task_for_session(dir.path(), "Review", ParentSpec::Detached, &session_id)
        .expect("test");
    let scan = add_task_for_session_with(
        dir.path(),
        &child_new("Scan", &parent.slug),
        ParentSpec::Detached,
        &session_id,
    )
    .expect("test");
    let audit = add_task_for_session_with(
        dir.path(),
        &child_new("Audit", &parent.slug),
        ParentSpec::Detached,
        &session_id,
    )
    .expect("test");
    start_task(dir.path(), &scan.slug, false).expect("test");
    complete_task(dir.path(), &scan.slug, false).expect("test");

    let reminder = stop_hook_reminder(&Stored::read(dir.path()), None);
    assert!(reminder.contains("1 of 2 sub-tasks done"), "{reminder}");
    assert!(
        reminder.contains(&format!("llmenv task start {}", audit.slug)),
        "{reminder}"
    );
}

#[test]
fn the_wip_reminder_shows_how_far_the_sub_tasks_have_come() {
    let dir = TempDir::new().expect("test");
    let session_id = session_for_project(dir.path(), &current_project());
    let parent = add_task_for_session(dir.path(), "Review", ParentSpec::Detached, &session_id)
        .expect("test");
    let scan = add_task_for_session_with(
        dir.path(),
        &child_new("Scan", &parent.slug),
        ParentSpec::Detached,
        &session_id,
    )
    .expect("test");
    start_task(dir.path(), &scan.slug, false).expect("test");

    let reminder = stop_hook_reminder(&Stored::read(dir.path()), None);
    assert!(
        reminder.contains("0 of 1 sub-tasks done, 1 in progress"),
        "{reminder}"
    );
}

#[test]
fn a_sub_task_must_belong_to_the_session_of_its_parent() {
    let dir = TempDir::new().expect("test");
    let session_a = session_for_project(dir.path(), "project-a-0000000000");
    let session_b = session_for_project(dir.path(), "project-b-0000000000");
    let parent =
        add_task_for_session(dir.path(), "Review", ParentSpec::Detached, &session_a).expect("test");
    let err = add_task_for_session_with(
        dir.path(),
        &child_new("Scan", &parent.slug),
        ParentSpec::Detached,
        &session_b,
    )
    .expect_err("a sub-task of another session is refused");
    assert!(
        err.to_string().contains("belongs to another session"),
        "{err}"
    );
}

#[test]
fn after_writes_a_blocked_on_edge_and_an_unknown_task_is_an_error() {
    let dir = TempDir::new().expect("test");
    let first = mk(dir.path(), "First", None).expect("test");
    let new = NewTask {
        title: "Second",
        after: Some(&first.slug),
        ..NewTask::default()
    };
    let second = add_task_for_session_with(dir.path(), &new, ParentSpec::Detached, "test-session")
        .expect("test");
    assert_eq!(second.blocked_on, std::slice::from_ref(&first.slug));
    let bad = NewTask {
        title: "Third",
        after: Some("no-such-task"),
        ..NewTask::default()
    };
    assert!(
        add_task_for_session_with(dir.path(), &bad, ParentSpec::Detached, "test-session").is_err()
    );
}

#[test]
fn starting_a_grandchild_puts_every_open_ancestor_in_progress() {
    let dir = TempDir::new().expect("test");
    let top = mk(dir.path(), "Top", None).expect("test");
    let mid = add_task_for_session_with(
        dir.path(),
        &child_new("Mid", &top.slug),
        ParentSpec::Detached,
        "test-session",
    )
    .expect("test");
    let leaf = add_task_for_session_with(
        dir.path(),
        &child_new("Leaf", &mid.slug),
        ParentSpec::Detached,
        "test-session",
    )
    .expect("test");
    start_task(dir.path(), &leaf.slug, false).expect("test");
    for slug in [&top.slug, &mid.slug, &leaf.slug] {
        assert_eq!(
            load_task(dir.path(), slug).expect("test").state,
            TaskState::Wip
        );
    }
}

#[test]
fn a_sub_task_of_a_queued_parent_waits_for_the_queue() {
    let dir = TempDir::new().expect("test");
    let first = mk(dir.path(), "First", None).expect("test");
    let second = mk(dir.path(), "Second", None).expect("test");
    let child = add_task_for_session_with(
        dir.path(),
        &child_new("Child", &second.slug),
        ParentSpec::Detached,
        "test-session",
    )
    .expect("test");
    let err = start_task(dir.path(), &child.slug, false).expect_err("first is open");
    assert!(
        err.to_string()
            .contains("cannot start: task 'second' is queued behind 'first'"),
        "{err}"
    );
    assert_eq!(
        load_task(dir.path(), &child.slug).expect("test").state,
        TaskState::Open,
        "the refusal leaves the sub-task open"
    );
    start_task(dir.path(), &first.slug, false).expect("test");
    complete_task(dir.path(), &first.slug, false).expect("test");
    start_task(dir.path(), &child.slug, false).expect("test");
    assert_eq!(
        load_task(dir.path(), &second.slug).expect("test").state,
        TaskState::Wip
    );
}

#[test]
fn a_sub_task_of_a_done_parent_refuses_unless_forced() {
    let dir = TempDir::new().expect("test");
    let parent = mk(dir.path(), "Review", None).expect("test");
    let child = add_task_for_session_with(
        dir.path(),
        &child_new("Scan", &parent.slug),
        ParentSpec::Detached,
        "test-session",
    )
    .expect("test");
    complete_task(dir.path(), &parent.slug, true).expect("test");
    // The sub-task is open, so `--force` closed the parent past it.
    let err = start_task(dir.path(), &child.slug, false).expect_err("parent is done");
    assert!(err.to_string().contains("is done"), "{err}");
    start_task(dir.path(), &child.slug, true).expect("test");
}

#[test]
fn done_on_a_parent_refuses_until_its_sub_tasks_are_done() {
    let dir = TempDir::new().expect("test");
    let parent = mk(dir.path(), "Review", None).expect("test");
    let child = add_task_for_session_with(
        dir.path(),
        &child_new("Scan", &parent.slug),
        ParentSpec::Detached,
        "test-session",
    )
    .expect("test");
    start_task(dir.path(), &child.slug, false).expect("test");
    let err = complete_task(dir.path(), &parent.slug, false).expect_err("sub-task is wip");
    assert!(err.to_string().contains("'scan' (wip)"), "{err}");
    let forced = complete_task(dir.path(), &parent.slug, true).expect("test");
    assert!(
        forced
            .undone_children_note()
            .expect("note")
            .contains("scan")
    );
    complete_task(dir.path(), &child.slug, false).expect("test");
    // `done` stays idempotent once everything is done.
    let again = complete_task(dir.path(), &parent.slug, false).expect("test");
    assert_eq!(again.undone_children_note(), None);
}

proptest::proptest! {
    #![proptest_config(proptest::prelude::ProptestConfig::with_cases(32))]

    // For any mix of task states per session: a session is idle exactly
    // when it has an open task and no wip or waiting task, and its next
    // task is open (#2338).
    #[test]
    fn idle_sessions_invariants(
        plan in proptest::collection::vec(
            proptest::collection::vec(0u8..4, 0..5),
            1..4,
        ),
    ) {
        let dir = TempDir::new().unwrap();
        let mut expected = std::collections::BTreeMap::new();
        for states in &plan {
            let sid = session_for_project(dir.path(), PROJECT);
            for (i, state) in states.iter().enumerate() {
                let t = add_task_for_session(
                    dir.path(),
                    &format!("t{i}"),
                    ParentSpec::Detached,
                    &sid,
                )
                .unwrap();
                match state {
                    1 => drop(start_task(dir.path(), &t.slug, true).unwrap()),
                    2 => {
                        start_task(dir.path(), &t.slug, true).unwrap();
                        wait_task(dir.path(), &t.slug, "r").unwrap();
                    }
                    3 => drop(done_task(dir.path(), &t.slug).unwrap()),
                    _ => {}
                }
            }
            let open = states.iter().filter(|s| **s == 0).count();
            let busy = states.iter().any(|s| matches!(s, 1 | 2));
            if open > 0 && !busy {
                expected.insert(sid, open);
            }
        }
        let open = session::open_sessions_strict(dir.path(), PROJECT).expect("test");
        let idle = idle_sessions(open, &list_tasks(dir.path()));
        let got: std::collections::BTreeMap<String, usize> = idle
            .iter()
            .map(|i| (i.session.id.clone(), i.open_count))
            .collect();
        proptest::prop_assert_eq!(got, expected);
        for i in &idle {
            // Either a task to start, or the reason that none can start.
            proptest::prop_assert_eq!(i.next.is_some(), i.loop_error.is_none());
            if let Some(next) = &i.next {
                proptest::prop_assert_eq!(next.state, TaskState::Open);
                proptest::prop_assert_eq!(next.session.as_deref(), Some(i.session.id.as_str()));
            }
        }
    }
}

// --- done without start, reopen (#2338) ---

#[test]
fn forced_close_of_an_open_task_keeps_the_prior_state_in_a_note() {
    let dir = TempDir::new().expect("test");
    let task = mk(dir.path(), "Never started", None).expect("test");
    let caller = ownership::Caller::Unidentified("test".into());
    complete_task_by(dir.path(), &task.slug, true, &caller).expect("test");
    let stored = load_task(dir.path(), &task.slug).expect("test");
    let note = stored.notes.last().expect("forced close leaves a note");
    assert!(
        note.text.contains("from open"),
        "prior state missing: {}",
        note.text
    );
    assert!(
        note.text.contains("unidentified"),
        "caller missing: {}",
        note.text
    );
}

#[test]
fn plain_done_leaves_no_forced_close_note() {
    let dir = TempDir::new().expect("test");
    let task = mk(dir.path(), "Started", None).expect("test");
    start_task(dir.path(), &task.slug, false).expect("test");
    let caller = ownership::Caller::Unidentified("test".into());
    complete_task_by(dir.path(), &task.slug, false, &caller).expect("test");
    assert!(
        load_task(dir.path(), &task.slug)
            .expect("test")
            .notes
            .is_empty()
    );
}

#[test]
fn repeat_done_keeps_updated_at_and_adds_no_note() {
    let dir = TempDir::new().expect("test");
    let task = mk(dir.path(), "Closed once", None).expect("test");
    done_task(dir.path(), &task.slug).expect("test");
    // Age the stored time, so a repeat close that rewrites it is visible even within one second.
    let mut stored = load_task(dir.path(), &task.slug).expect("test");
    "2000-01-01T00:00:00Z".clone_into(&mut stored.updated_at);
    save_task(dir.path(), &stored).expect("test");
    let caller = ownership::Caller::Unidentified("test".into());
    let completed = complete_task_by(dir.path(), &task.slug, true, &caller).expect("test");
    assert_eq!(completed.task.state, TaskState::Done);
    let after = load_task(dir.path(), &task.slug).expect("test");
    assert_eq!(after.updated_at, "2000-01-01T00:00:00Z");
    assert!(after.notes.is_empty(), "repeat done must not add a note");
}

#[test]
fn complete_task_refuses_an_open_task_and_names_the_fix() {
    let dir = TempDir::new().expect("test");
    let task = mk(dir.path(), "Never started", None).expect("test");
    let err = complete_task(dir.path(), &task.slug, false).expect_err("open must refuse");
    let text = err.to_string();
    assert!(
        text.contains(&format!("llmenv task start {}", task.slug)),
        "{text}"
    );
    assert!(text.contains("--force"), "{text}");
    assert_eq!(
        load_task(dir.path(), &task.slug).expect("test").state,
        TaskState::Open,
        "a refused done must leave the task open"
    );
}

#[test]
fn complete_task_force_completes_an_open_task_and_notes_the_skip() {
    let dir = TempDir::new().expect("test");
    let task = mk(dir.path(), "Never started", None).expect("test");
    let completed = complete_task(dir.path(), &task.slug, true).expect("test");
    assert_eq!(completed.prior, TaskState::Open);
    assert_eq!(completed.task.state, TaskState::Done);
    let note = completed
        .skipped_start_note()
        .expect("forced open -> done notes");
    assert!(note.contains(&format!("llmenv task start --reopen {}", task.slug)));
}

#[test]
fn complete_task_after_start_needs_no_force() {
    let dir = TempDir::new().expect("test");
    let task = mk(dir.path(), "Started", None).expect("test");
    start_task(dir.path(), &task.slug, false).expect("test");
    let completed = complete_task(dir.path(), &task.slug, false).expect("test");
    assert_eq!(completed.prior, TaskState::Wip);
    assert!(completed.skipped_start_note().is_none());
}

#[test]
fn complete_task_waiting_and_done_need_no_force() {
    let dir = TempDir::new().expect("test");
    let task = mk(dir.path(), "Waits", None).expect("test");
    start_task(dir.path(), &task.slug, false).expect("test");
    wait_task(dir.path(), &task.slug, "review").expect("test");
    complete_task(dir.path(), &task.slug, false).expect("waiting completes");
    let again = complete_task(dir.path(), &task.slug, false).expect("done is idempotent");
    assert_eq!(again.prior, TaskState::Done);
}

#[test]
fn start_task_on_done_task_points_at_reopen() {
    let dir = TempDir::new().expect("test");
    let task = mk(dir.path(), "Closed early", None).expect("test");
    done_task(dir.path(), &task.slug).expect("test");
    let err = start_task(dir.path(), &task.slug, false)
        .unwrap_err()
        .to_string();
    assert!(err.contains("--reopen"), "{err}");
}

#[test]
fn reopen_task_moves_done_task_back_to_open_with_note() {
    let dir = TempDir::new().expect("test");
    let task = mk(dir.path(), "Closed early", None).expect("test");
    done_task(dir.path(), &task.slug).expect("test");

    let reopened = reopen_task(dir.path(), &task.slug).expect("test");
    assert_eq!(reopened.state, TaskState::Open);
    assert!(reopened.notes.iter().any(|n| n.text.contains("Reopened")));
    let started = start_task(dir.path(), &task.slug, false).expect("test");
    assert_eq!(started.state, TaskState::Wip);
}

#[test]
fn reopen_task_leaves_non_done_task_unchanged() {
    let dir = TempDir::new().expect("test");
    let task = mk(dir.path(), "Working", None).expect("test");
    let started = start_task(dir.path(), &task.slug, false).expect("test");
    let after = reopen_task(dir.path(), &task.slug).expect("test");
    assert_eq!(after.state, TaskState::Wip);
    assert_eq!(after.notes, started.notes);
}

#[test]
fn stop_hook_reminder_flags_wip_tasks() {
    let dir = TempDir::new().expect("test");
    let task = wip_task_in_project(dir.path(), "Left in progress", &current_project());
    let reminder = stop_hook_reminder(&Stored::read(dir.path()), None);
    assert!(reminder.contains(&task.slug));
}

#[test]
fn stop_hook_reminder_silent_when_only_waiting_tasks() {
    let dir = TempDir::new().expect("test");
    let task = mk(dir.path(), "Blocked on review", None).expect("test");
    wait_task(dir.path(), &task.slug, "spec review").expect("test");
    // `waiting` is meant to be quiet — Stop must not re-inject its FYI.
    assert!(stop_hook_reminder(&Stored::read(dir.path()), None).is_empty());
}

#[test]
fn stop_hook_reminder_shows_wip_but_not_waiting() {
    let dir = TempDir::new().expect("test");
    let project = current_project();
    let wip = wip_task_in_project(dir.path(), "Actively working", &project);
    let waiting = waiting_task_in_project(dir.path(), "Blocked on review", &project, "spec review");

    let reminder = stop_hook_reminder(&Stored::read(dir.path()), None);
    assert!(reminder.contains(&wip.slug));
    assert!(reminder.contains("exhaust safe autonomous remediation"));
    // The waiting task and its FYI must stay silent on Stop.
    assert!(!reminder.contains(&waiting.slug));
    assert!(!reminder.contains("no action needed"));
}

// --- cross-session ownership framing (#1028) ---

#[test]
fn stop_hook_reminder_shows_session_tag_and_never_presumes_ownership() {
    // The bug (#1028): a WIP task belonging to a *different*, concurrently
    // running session must never be framed as a command to resume it —
    // the reminder can't tell whose session started this conversation, so
    // it must name the owning session and let the agent judge for itself
    // rather than asserting "you have tasks in progress."
    let dir = TempDir::new().expect("test");
    let task = wip_task_in_project(dir.path(), "Someone's task", &current_project());

    let reminder = stop_hook_reminder(&Stored::read(dir.path()), None);
    assert!(
        reminder.contains(task.session.as_deref().expect("test")),
        "reminder must name the owning session so the agent can tell whether it's its \
         own: {reminder}"
    );
    assert!(
        reminder.contains("recognize"),
        "must condition continuing the task on the agent recognizing it as its own: \
         {reminder}"
    );
}

#[test]
fn session_start_reminder_shows_session_tag_and_never_presumes_ownership() {
    let dir = TempDir::new().expect("test");
    let task = wip_task_in_project(dir.path(), "Someone's task", &current_project());

    let reminder = session_start_reminder(dir.path());
    assert!(
        reminder.contains(task.session.as_deref().expect("test")),
        "reminder must name the owning session: {reminder}"
    );
    assert!(
        reminder.contains("recognize"),
        "must condition resuming the task on the agent recognizing it as its own: \
         {reminder}"
    );
}

#[test]
fn session_finish_reminder_never_presumes_ownership_of_a_done_session() {
    // Variant of #1028: a fully-done session belonging to a different,
    // concurrently running conversation must not be framed as a bare
    // command to close it out — closing someone else's session mutates
    // their bookkeeping.
    let dir = TempDir::new().expect("test");
    let task = wip_task_in_project(dir.path(), "Someone's task", &current_project());
    done_task(dir.path(), &task.slug).expect("test");

    let reminder = session_start_reminder(dir.path());
    assert!(
        reminder.contains(task.session.as_deref().expect("test")),
        "reminder must name the owning session: {reminder}"
    );
    assert!(
        reminder.contains("recognize"),
        "must condition closing the session on the agent recognizing it as its own: \
         {reminder}"
    );
}

#[test]
fn session_start_reminder_lists_waiting_tasks() {
    let dir = TempDir::new().expect("test");
    let task = waiting_task_in_project(
        dir.path(),
        "Blocked on review",
        &current_project(),
        "spec review",
    );
    let reminder = session_start_reminder(dir.path());
    // At session start (resume/wake) the waiting FYI is useful, unlike Stop.
    assert!(reminder.contains(&task.slug));
    assert!(reminder.contains("no action needed"));
    // Still a plain FYI, never the action-pushing wip footer.
    assert!(!reminder.contains("exhaust safe autonomous remediation"));
}

#[test]
fn session_start_reminder_shows_both_wip_and_waiting() {
    let dir = TempDir::new().expect("test");
    let project = current_project();
    let wip = wip_task_in_project(dir.path(), "Actively working", &project);
    let waiting = waiting_task_in_project(dir.path(), "Blocked on review", &project, "spec review");

    let reminder = session_start_reminder(dir.path());
    assert!(reminder.contains(&wip.slug));
    assert!(reminder.contains(&waiting.slug));
    assert!(reminder.contains("no action needed"));
}

// --- reminder project scoping (#949) ---

#[test]
fn stop_hook_reminder_does_not_leak_wip_task_from_other_project() {
    let dir = TempDir::new().expect("test");
    let other_project = "other-project-9999999999";
    let leaked = wip_task_in_project(dir.path(), "Other project's task", other_project);
    // No session/task tagged to the real current project at all — a
    // `wip` task belonging to a different project must not surface.
    let reminder = stop_hook_reminder(&Stored::read(dir.path()), None);
    assert!(!reminder.contains(&leaked.slug));
    assert!(reminder.is_empty());
}

#[test]
fn session_start_reminder_does_not_leak_waiting_task_from_other_project() {
    let dir = TempDir::new().expect("test");
    let other_project = "other-project-9999999999";
    let leaked =
        waiting_task_in_project(dir.path(), "Other project's task", other_project, "blocked");
    let reminder = session_start_reminder(dir.path());
    assert!(!reminder.contains(&leaked.slug));
    assert!(reminder.is_empty());
}

#[test]
fn stop_hook_reminder_shows_only_current_projects_wip_task() {
    let dir = TempDir::new().expect("test");
    let project = current_project();
    let mine = wip_task_in_project(dir.path(), "My task", &project);
    let leaked = wip_task_in_project(dir.path(), "Other project's task", "other-project-999");

    let reminder = stop_hook_reminder(&Stored::read(dir.path()), None);
    assert!(reminder.contains(&mine.slug), "own-project task must show");
    assert!(
        !reminder.contains(&leaked.slug),
        "other-project task must not leak in"
    );
}

#[test]
fn legacy_task_with_no_session_is_not_surfaced_cross_project() {
    let dir = TempDir::new().expect("test");
    // Predates mandatory sessions (`#[serde(default)]` on `Task::session`
    // keeps such a file loadable) — no session to attribute it to any
    // project, so it must stay silent rather than surface everywhere.
    let mut legacy = mk(dir.path(), "Legacy wip task", None).expect("test");
    legacy.session = None;
    legacy.state = TaskState::Wip;
    save_task(dir.path(), &legacy).expect("test");

    assert!(!stop_hook_reminder(&Stored::read(dir.path()), None).contains(&legacy.slug));
    assert!(!session_start_reminder(dir.path()).contains(&legacy.slug));
}

// --- task ls ordering + filtering (#926) ---

fn t(slug: &str, state: TaskState, parent: Option<&str>, session: Option<&str>) -> Task {
    Task {
        slug: slug.to_string(),
        title: format!("Title for {slug}"),
        state,
        parent: parent.map(str::to_string),
        blocked_on: Vec::new(),
        notes: Vec::new(),
        detail: None,
        session: session.map(str::to_string),
        // Identical timestamps on purpose: exercises the slug tiebreak.
        created_at: "2026-01-01T00:00:00Z".to_string(),
        updated_at: "2026-01-01T00:00:00Z".to_string(),
        relation: Relation::Queued,
        parallel: false,
    }
}

fn slugs(rows: &[DisplayRow]) -> Vec<(usize, String)> {
    rows.iter()
        .map(|r| (r.depth, r.task.slug.clone()))
        .collect()
}

#[test]
fn filter_by_state_empty_keeps_all() {
    let tasks = vec![
        t("a", TaskState::Open, None, None),
        t("b", TaskState::Done, None, None),
    ];
    assert_eq!(filter_by_state(tasks, &[]).len(), 2);
}

#[test]
fn filter_by_state_keeps_only_listed_states() {
    let tasks = vec![
        t("a", TaskState::Open, None, None),
        t("b", TaskState::Wip, None, None),
        t("c", TaskState::Waiting, None, None),
        t("d", TaskState::Done, None, None),
    ];
    let kept = filter_by_state(tasks, &[TaskState::Wip, TaskState::Waiting]);
    assert_eq!(
        kept.iter().map(|x| x.slug.as_str()).collect::<Vec<_>>(),
        ["b", "c"]
    );
}

// --- task ls --current-project / task show --current/--next (#1117) ---

/// Like `t`, but with an explicit `updated_at` — needed to exercise
/// [`resolve_current_task`]'s "most recently updated" tiebreak, which
/// `t`'s fixed timestamp can't.
fn t_updated(slug: &str, state: TaskState, session: Option<&str>, updated_at: &str) -> Task {
    Task {
        updated_at: updated_at.to_string(),
        ..t(slug, state, None, session)
    }
}

#[test]
fn filter_tasks_for_project_keeps_only_tasks_in_that_projects_sessions() {
    let dir = TempDir::new().expect("test");
    let session_id = session_for_project(dir.path(), "proj-a");
    let in_project =
        add_task_for_session(dir.path(), "In proj-a", ParentSpec::Detached, &session_id)
            .expect("test");
    let other_session = t("other", TaskState::Open, None, Some("other-session"));
    let no_session = t("legacy", TaskState::Open, None, None);
    let tasks = vec![in_project.clone(), other_session, no_session];

    let sessions = session::list_sessions_strict(dir.path()).expect("test");
    let kept = filter_tasks_for_project(&sessions, "proj-a", tasks);
    assert_eq!(
        kept.iter().map(|x| x.slug.as_str()).collect::<Vec<_>>(),
        [in_project.slug.as_str()]
    );
}

#[test]
fn resolve_current_task_prefers_wip_over_open() {
    let tasks = vec![
        t_updated("open", TaskState::Open, Some("s"), "2026-01-01T00:00:02Z"),
        t_updated("wip", TaskState::Wip, Some("s"), "2026-01-01T00:00:01Z"),
    ];
    let current = resolve_current_task(&tasks).expect("test");
    assert_eq!(current.slug, "wip");
}

#[test]
fn resolve_current_task_falls_back_to_most_recently_updated_non_done_task() {
    let tasks = vec![
        t_updated("older", TaskState::Open, Some("s"), "2026-01-01T00:00:01Z"),
        t_updated(
            "newer",
            TaskState::Waiting,
            Some("s"),
            "2026-01-01T00:00:02Z",
        ),
        t_updated("done", TaskState::Done, Some("s"), "2026-01-01T00:00:03Z"),
    ];
    let current = resolve_current_task(&tasks).expect("test");
    assert_eq!(current.slug, "newer");
}

#[test]
fn resolve_current_task_none_when_everything_is_done() {
    let tasks = vec![t("done", TaskState::Done, None, Some("s"))];
    assert!(resolve_current_task(&tasks).is_none());
}

#[test]
fn resolve_next_task_skips_done_and_blocked_tasks() {
    let current = t("a", TaskState::Wip, None, Some("s"));
    let mut blocked = t("b", TaskState::Open, None, Some("s"));
    blocked.blocked_on = vec!["a".to_string()]; // "a" (current) isn't done yet
    let done = t("c", TaskState::Done, None, Some("s"));
    let actionable = t("d", TaskState::Open, None, Some("s"));
    let session_tasks = vec![current.clone(), blocked, done, actionable.clone()];

    let next = resolve_next_task(&session_tasks, &session_tasks, &current).expect("test");
    assert_eq!(next.slug, "d");
}

#[test]
fn resolve_next_task_resolves_blockers_against_all_tasks_not_just_the_session() {
    let current = t("a", TaskState::Wip, None, Some("s"));
    // Blocker lives in a different session than the blocked task, and
    // isn't done yet — "b" stays non-actionable.
    let mut blocked = t("b", TaskState::Open, None, Some("s"));
    blocked.blocked_on = vec!["elsewhere".to_string()];
    let blocker = t("elsewhere", TaskState::Open, None, Some("other-session"));
    let session_tasks = vec![current.clone(), blocked];
    let all_tasks = vec![current.clone(), session_tasks[1].clone(), blocker];

    assert!(resolve_next_task(&all_tasks, &session_tasks, &current).is_none());
}

#[test]
fn resolve_next_task_returns_a_task_once_its_cross_session_blocker_is_done() {
    // Same shape as the test above, but the blocker is `done` — pins the
    // `all_tasks`/`session_tasks` argument order: swapping them would
    // make the blocker unresolvable (not present in `session_tasks`) and
    // this would wrongly return `None` too.
    let current = t("a", TaskState::Wip, None, Some("s"));
    let mut blocked = t("b", TaskState::Open, None, Some("s"));
    blocked.blocked_on = vec!["elsewhere".to_string()];
    let blocker = t("elsewhere", TaskState::Done, None, Some("other-session"));
    let session_tasks = vec![current.clone(), blocked.clone()];
    let all_tasks = vec![current.clone(), blocked, blocker];

    let next = resolve_next_task(&all_tasks, &session_tasks, &current).expect("test");
    assert_eq!(next.slug, "b");
}

#[test]
fn resolve_next_task_skips_waiting_tasks() {
    // `waiting` is paused on something outside the agent's control, not
    // a legitimate "next" step — distinct from `resolve_current_task`'s
    // fallback, which does treat `waiting` as a valid "in progress" hit.
    let current = t("a", TaskState::Wip, None, Some("s"));
    let waiting = t("b", TaskState::Waiting, None, Some("s"));
    let actionable = t("c", TaskState::Open, None, Some("s"));
    let session_tasks = vec![current.clone(), waiting, actionable];

    let next = resolve_next_task(&session_tasks, &session_tasks, &current).expect("test");
    assert_eq!(next.slug, "c");
}

#[test]
fn resolve_next_task_prefers_child_over_next_sibling() {
    let parent = t("p", TaskState::Wip, None, Some("s"));
    let child = t("c", TaskState::Open, Some("p"), Some("s"));
    let sibling = t("sib", TaskState::Open, None, Some("s"));
    let session_tasks = vec![parent.clone(), child, sibling];

    let next = resolve_next_task(&session_tasks, &session_tasks, &parent).expect("test");
    assert_eq!(
        next.slug, "c",
        "next after a parent must be its child, not a sibling"
    );
}

#[test]
fn resolve_next_task_none_when_current_not_found() {
    let stray = t("stray", TaskState::Wip, None, Some("s"));
    let session_tasks = vec![t("a", TaskState::Open, None, Some("s"))];
    assert!(resolve_next_task(&session_tasks, &session_tasks, &stray).is_none());
}

#[test]
fn resolve_next_task_none_when_current_is_last() {
    let current = t("a", TaskState::Wip, None, Some("s"));
    let session_tasks = vec![current.clone()];
    assert!(resolve_next_task(&session_tasks, &session_tasks, &current).is_none());
}

#[test]
fn display_rows_indents_subtasks_under_parent_in_creation_order() {
    // Deliberately unsorted input; slug tiebreak must produce a stable order.
    let tasks = vec![
        t("deploy", TaskState::Open, None, Some("s")),
        t("child-b", TaskState::Open, Some("api"), Some("s")),
        t("api", TaskState::Wip, None, Some("s")),
        t("child-a", TaskState::Done, Some("api"), Some("s")),
    ];
    let rows = display_rows(tasks, &["s".to_string()]);
    assert_eq!(
        slugs(&rows),
        vec![
            (0, "api".to_string()),
            (1, "child-a".to_string()),
            (1, "child-b".to_string()),
            (0, "deploy".to_string()),
        ]
    );
}

#[test]
fn display_rows_orders_priority_sessions_first_then_others_then_none() {
    let tasks = vec![
        t("z", TaskState::Open, None, None),
        t("other", TaskState::Open, None, Some("zzz")),
        t("cur", TaskState::Open, None, Some("current")),
    ];
    let rows = display_rows(tasks, &["current".to_string()]);
    assert_eq!(
        rows.iter().map(|r| r.task.slug.clone()).collect::<Vec<_>>(),
        ["cur", "other", "z"]
    );
}

#[test]
fn display_rows_keeps_unlisted_session_tasks() {
    // A session absent from the priority list must still get a group.
    let tasks = vec![t("only", TaskState::Open, None, Some("ghost"))];
    let rows = display_rows(tasks, &[]);
    assert_eq!(slugs(&rows), vec![(0, "only".to_string())]);
}

#[test]
fn display_rows_orphan_parent_reference_renders_child_as_root() {
    // Parent not present in the group -> child is a depth-0 root, not lost.
    let tasks = vec![t("child", TaskState::Open, Some("missing"), Some("s"))];
    let rows = display_rows(tasks, &["s".to_string()]);
    assert_eq!(slugs(&rows), vec![(0, "child".to_string())]);
}

#[test]
fn display_rows_survives_parent_cycle_without_infinite_recursion() {
    // Malformed data: a <-> b cite each other as parent. Must terminate.
    let mut a = t("a", TaskState::Open, Some("b"), Some("s"));
    let mut b = t("b", TaskState::Open, Some("a"), Some("s"));
    a.created_at = "2026-01-01T00:00:01Z".to_string();
    b.created_at = "2026-01-01T00:00:02Z".to_string();
    let rows = display_rows(vec![a, b], &["s".to_string()]);
    // Neither is a root (each cites the other), but the completeness guard
    // still renders both exactly once instead of dropping them or looping.
    let mut got: Vec<String> = rows.iter().map(|r| r.task.slug.clone()).collect();
    got.sort();
    assert_eq!(got, ["a", "b"]);
}

// --- mandatory-session wiring (2026-07-21 rework) ---

fn created(outcome: StartOutcome) -> super::session::Session {
    match outcome {
        StartOutcome::Created(s) => s,
        other => panic!("expected Created, got {other:?}"),
    }
}

#[test]
fn add_task_with_explicit_session_tags_it() {
    let dir = TempDir::new().expect("test");
    let session = created(
        start_session(dir.path(), Some("s"), None, PROJECT, StartDecision::Auto).expect("test"),
    );
    let task = add_task(
        dir.path(),
        "Do thing",
        ParentSpec::Detached,
        SessionChoice::Named {
            id: &session.id,
            owner: &session::EngineIdentity::from_session_id(None),
            other_session: None,
        },
        PROJECT,
    )
    .expect("test");
    assert_eq!(task.session, Some(session.id));
}

#[test]
fn add_task_explicit_session_rejects_unknown_id() {
    let dir = TempDir::new().expect("test");
    assert!(
        add_task(
            dir.path(),
            "Do thing",
            ParentSpec::Detached,
            SessionChoice::Named {
                id: "no-such-session",
                owner: &session::EngineIdentity::from_session_id(None),
                other_session: None,
            },
            PROJECT
        )
        .is_err()
    );
}

#[test]
fn add_task_auto_resolves_when_exactly_one_open_session_for_project() {
    let dir = TempDir::new().expect("test");
    let session = created(
        start_session(dir.path(), Some("s"), None, PROJECT, StartDecision::Auto).expect("test"),
    );
    let task = add_task(
        dir.path(),
        "Do thing",
        ParentSpec::Detached,
        ANYONE,
        PROJECT,
    )
    .expect("test");
    assert_eq!(task.session, Some(session.id));
}

#[test]
fn add_task_errors_with_zero_open_sessions_for_project() {
    let dir = TempDir::new().expect("test");
    let err = add_task(
        dir.path(),
        "Do thing",
        ParentSpec::Detached,
        ANYONE,
        PROJECT,
    )
    .unwrap_err();
    assert!(err.to_string().contains("session start"));
}

#[test]
fn add_task_errors_with_two_open_sessions_for_project() {
    let dir = TempDir::new().expect("test");
    start_session(
        dir.path(),
        Some("first"),
        None,
        PROJECT,
        StartDecision::Auto,
    )
    .expect("test");
    start_session(
        dir.path(),
        Some("second"),
        None,
        PROJECT,
        StartDecision::New,
    )
    .expect("test");
    let err = add_task(
        dir.path(),
        "Do thing",
        ParentSpec::Detached,
        ANYONE,
        PROJECT,
    )
    .unwrap_err();
    assert!(err.to_string().contains("--session"));
}

#[test]
fn add_task_does_not_auto_resolve_a_different_projects_session() {
    let dir = TempDir::new().expect("test");
    start_session(
        dir.path(),
        Some("s"),
        None,
        "other-project-1111111111",
        StartDecision::Auto,
    )
    .expect("test");
    assert!(
        add_task(
            dir.path(),
            "Do thing",
            ParentSpec::Detached,
            ANYONE,
            PROJECT
        )
        .is_err()
    );
}

#[test]
fn start_task_touches_its_sessions_last_activity() {
    let dir = TempDir::new().expect("test");
    let session = created(
        start_session(dir.path(), Some("s"), None, PROJECT, StartDecision::Auto).expect("test"),
    );
    let original = session.last_activity.clone();
    let task = add_task(
        dir.path(),
        "Do thing",
        ParentSpec::Detached,
        SessionChoice::Named {
            id: &session.id,
            owner: &session::EngineIdentity::from_session_id(None),
            other_session: None,
        },
        PROJECT,
    )
    .expect("test");
    start_task(dir.path(), &task.slug, false).expect("test");
    let reloaded = open_sessions_strict(dir.path(), PROJECT)
        .expect("test")
        .into_iter()
        .find(|s| s.id == session.id)
        .expect("test");
    assert_ne!(reloaded.last_activity, original);
}

// Task/TaskNote/TaskState derive Serialize/Deserialize and persist as
// JSON. A serde roundtrip must be lossless — a drifted derive (renamed
// field, wrong rename attr) would silently corrupt a user's task store.
// Also covers slugify/resolve_identifier/nesting invariants (#231
// pre-pr-review property-test-gap-finder pass).
mod proptests {
    use super::*;
    use proptest::prelude::*;

    fn arb_task_state() -> impl Strategy<Value = TaskState> {
        prop_oneof![
            Just(TaskState::Open),
            Just(TaskState::Wip),
            Just(TaskState::Waiting),
            Just(TaskState::Done),
        ]
    }

    fn arb_task_note() -> impl Strategy<Value = TaskNote> {
        (".{0,40}", ".{0,80}").prop_map(|(at, text)| TaskNote { at, text })
    }

    fn arb_task() -> impl Strategy<Value = Task> {
        (
            ".{1,30}",
            ".{1,60}",
            arb_task_state(),
            proptest::option::of(".{1,30}"),
            proptest::collection::vec(".{1,30}", 0..4),
            proptest::collection::vec(arb_task_note(), 0..4),
            proptest::option::of(".{1,60}"),
            proptest::option::of(".{1,30}"),
            ".{1,30}",
            ".{1,30}",
            prop_oneof![Just(Relation::Queued), Just(Relation::Child)],
            any::<bool>(),
        )
            .prop_map(
                |(
                    slug,
                    title,
                    state,
                    parent,
                    blocked_on,
                    notes,
                    detail,
                    session,
                    created_at,
                    updated_at,
                    relation,
                    parallel,
                )| {
                    Task {
                        slug,
                        title,
                        state,
                        parent,
                        relation,
                        parallel,
                        blocked_on,
                        notes,
                        detail,
                        session,
                        created_at,
                        updated_at,
                    }
                },
            )
    }

    /// Like [`arb_task`], but with the slug restricted to characters that
    /// survive a filesystem round-trip on every platform (no NUL, no path
    /// separators, no lone surrogates/unpaired combining marks that trip
    /// APFS's Unicode normalization). `arb_task`'s fully-arbitrary slug is
    /// fine for the in-memory JSON round-trip, but real `Task`s only ever
    /// get a slug via [`slugify`] (lowercase alnum + hyphen) — this
    /// generator matches that realistic shape for the file-backed
    /// [`save_task`]/[`load_task`] round-trip (#1283).
    ///
    /// No leading `-`: [`save_task`]/[`load_task`] validate the slug via
    /// [`llmenv_paths::is_valid_short_name`], which rejects one (so it
    /// can't be mistaken for a CLI flag) — a slug this generator produces
    /// must be one `save_task`/`load_task` will actually accept.
    fn arb_task_with_fs_safe_slug() -> impl Strategy<Value = Task> {
        ("[A-Za-z0-9_][A-Za-z0-9_-]{0,39}", arb_task()).prop_map(|(slug, mut task)| {
            task.slug = slug;
            task
        })
    }

    /// A single-session task forest with unique slugs `t0..tn` where each
    /// task's parent (if any) is an earlier index — guaranteeing an acyclic
    /// forest so the depth/parent-order invariants are well-defined.
    fn arb_forest() -> impl Strategy<Value = Vec<Task>> {
        proptest::collection::vec((arb_task_state(), proptest::option::of(0usize..7)), 1..8)
            .prop_map(|specs| {
                specs
                    .into_iter()
                    .enumerate()
                    .map(|(i, (state, parent_pick))| {
                        let parent = parent_pick.filter(|&p| p < i).map(|p| format!("t{p}"));
                        Task {
                            slug: format!("t{i}"),
                            title: format!("task {i}"),
                            state,
                            parent,
                            blocked_on: Vec::new(),
                            notes: Vec::new(),
                            detail: None,
                            session: Some("s".to_string()),
                            created_at: format!("2026-01-01T00:00:{:02}Z", i.min(59)),
                            updated_at: "2026-01-01T00:00:00Z".to_string(),
                            relation: Relation::Queued,
                            parallel: false,
                        }
                    })
                    .collect()
            })
    }

    /// True when `slug` is `root` or one of its descendants, by walking
    /// `slug`'s parent chain. Bounded by the task count for safety.
    fn in_subtree(slug: &str, root: &str, tasks: &[Task]) -> bool {
        let mut cur = Some(slug.to_string());
        for _ in 0..=tasks.len() {
            let Some(s) = cur else { return false };
            if s == root {
                return true;
            }
            cur = tasks
                .iter()
                .find(|t| t.slug == s)
                .and_then(|t| t.parent.clone());
        }
        false
    }

    /// Like [`arb_forest`], but each task also carries 0..2 `blocked_on`
    /// refs to earlier-or-equal indices (including itself, exercising
    /// [`is_done_including_descendants`]'s cycle guard) — the shape
    /// `resolve_next_task`'s blocker-resolution invariants need (#1121).
    fn arb_forest_with_blockers() -> impl Strategy<Value = Vec<Task>> {
        proptest::collection::vec(
            (
                arb_task_state(),
                proptest::option::of(0usize..7),
                proptest::collection::vec(0usize..7, 0..2),
            ),
            1..8,
        )
        .prop_map(|specs| {
            let n = specs.len();
            specs
                .into_iter()
                .enumerate()
                .map(|(i, (state, parent_pick, blocker_picks))| {
                    let parent = parent_pick.filter(|&p| p < i).map(|p| format!("t{p}"));
                    let blocked_on = blocker_picks
                        .into_iter()
                        .filter(|&b| b < n)
                        .map(|b| format!("t{b}"))
                        .collect();
                    Task {
                        slug: format!("t{i}"),
                        title: format!("task {i}"),
                        state,
                        parent,
                        blocked_on,
                        notes: Vec::new(),
                        detail: None,
                        session: Some("s".to_string()),
                        created_at: format!("2026-01-01T00:00:{:02}Z", i.min(59)),
                        updated_at: "2026-01-01T00:00:00Z".to_string(),
                        relation: Relation::Queued,
                        parallel: false,
                    }
                })
                .collect()
        })
    }

    proptest! {
        #[test]
        fn task_note_json_roundtrips(note in arb_task_note()) {
            let json = serde_json::to_string(&note).unwrap();
            let back: TaskNote = serde_json::from_str(&json).unwrap();
            prop_assert_eq!(back, note);
        }

        #[test]
        fn task_state_json_roundtrips(state in arb_task_state()) {
            let json = serde_json::to_string(&state).unwrap();
            let back: TaskState = serde_json::from_str(&json).unwrap();
            prop_assert_eq!(back, state);
        }

        #[test]
        fn task_json_roundtrips(task in arb_task()) {
            let json = serde_json::to_string(&task).unwrap();
            let back: Task = serde_json::from_str(&json).unwrap();
            prop_assert_eq!(back, task);
        }

        #[test]
        fn save_task_load_task_file_roundtrips(task in arb_task_with_fs_safe_slug()) {
            let dir = tempfile::TempDir::new().unwrap();
            save_task(dir.path(), &task).unwrap();
            let loaded = load_task(dir.path(), &task.slug).unwrap();
            prop_assert_eq!(loaded, task);
        }

        #[test]
        fn slugify_output_is_lowercase_alnum_and_hyphen_only(title in ".{0,80}") {
            let slug = slugify(&title);
            prop_assert!(slug.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'));
        }

        #[test]
        fn slugify_never_starts_or_ends_with_hyphen(title in ".{0,80}") {
            let slug = slugify(&title);
            prop_assert!(!slug.starts_with('-'));
            prop_assert!(!slug.ends_with('-'));
        }

        #[test]
        fn slugify_is_idempotent(title in ".{0,80}") {
            let once = slugify(&title);
            let twice = slugify(&once);
            prop_assert_eq!(once, twice);
        }

        #[test]
        fn slugify_only_derived_from_first_six_whitespace_words(
            words in proptest::collection::vec("[a-zA-Z0-9]{1,8}", 0..12)
        ) {
            let title = words.join(" ");
            let full = slugify(&title);
            let truncated_title = words.iter().take(6).cloned().collect::<Vec<_>>().join(" ");
            let truncated = slugify(&truncated_title);
            // Appending more whitespace-delimited words beyond the 6th
            // must never change the slug — slugify only reads the first 6.
            prop_assert_eq!(full, truncated);
        }

        #[test]
        fn resolve_identifier_finds_every_added_tasks_own_slug(
            titles in proptest::collection::vec("[a-zA-Z]{3,12}", 1..6)
        ) {
            let dir = tempfile::TempDir::new().unwrap();
            let mut slugs = Vec::new();
            for title in &titles {
                let task = mk(dir.path(), title, None).unwrap();
                slugs.push(task.slug);
            }
            for slug in &slugs {
                prop_assert_eq!(resolve_identifier(dir.path(), slug).unwrap(), slug.clone());
            }
        }

        #[test]
        fn nested_chain_of_arbitrary_depth_links_correctly(
            titles in proptest::collection::vec("[a-zA-Z]{3,12}", 1..8)
        ) {
            let dir = tempfile::TempDir::new().unwrap();
            let mut prev_slug: Option<String> = None;
            let mut chain = Vec::new();
            for title in &titles {
                let task = mk(dir.path(), title, prev_slug.as_deref()).unwrap();
                prop_assert_eq!(&task.parent, &prev_slug);
                prev_slug = Some(task.slug.clone());
                chain.push(task);
            }
            // Walk the chain back from the deepest task to the root,
            // confirming every parent link resolves and matches.
            for (i, task) in chain.iter().enumerate().rev() {
                if i == 0 {
                    prop_assert_eq!(&task.parent, &None);
                } else {
                    prop_assert_eq!(task.parent.as_deref(), Some(chain[i - 1].slug.as_str()));
                    let loaded = load_task(dir.path(), task.parent.as_ref().unwrap()).unwrap();
                    prop_assert_eq!(loaded.slug, chain[i - 1].slug.clone());
                }
            }
        }

        #[test]
        fn display_rows_is_complete_and_indents_children(tasks in arb_forest()) {
            let input: std::collections::BTreeSet<String> =
                tasks.iter().map(|t| t.slug.clone()).collect();
            let rows = display_rows(tasks.clone(), &["s".to_string()]);
            // Completeness: every input task appears exactly once.
            prop_assert_eq!(rows.len(), tasks.len());
            let output: std::collections::BTreeSet<String> =
                rows.iter().map(|r| r.task.slug.clone()).collect();
            prop_assert_eq!(output, input);
            // Depth invariant + parent-before-child ordering.
            let depth: std::collections::HashMap<String, usize> =
                rows.iter().map(|r| (r.task.slug.clone(), r.depth)).collect();
            let pos: std::collections::HashMap<String, usize> = rows
                .iter()
                .enumerate()
                .map(|(i, r)| (r.task.slug.clone(), i))
                .collect();
            for r in &rows {
                match &r.task.parent {
                    Some(p) => {
                        prop_assert_eq!(r.depth, depth[p] + 1);
                        prop_assert!(pos[p] < pos[&r.task.slug]);
                    }
                    None => prop_assert_eq!(r.depth, 0),
                }
            }
        }

        #[test]
        fn filter_by_state_keeps_exactly_matching_states(
            tasks in proptest::collection::vec(arb_task(), 0..12),
            states in proptest::collection::hash_set(arb_task_state(), 0..4),
        ) {
            let want: Vec<TaskState> = states.iter().copied().collect();
            let kept = filter_by_state(tasks.clone(), &want);
            if want.is_empty() {
                prop_assert_eq!(kept.len(), tasks.len());
            } else {
                prop_assert!(kept.iter().all(|t| want.contains(&t.state)));
                let expected = tasks.iter().filter(|t| want.contains(&t.state)).count();
                prop_assert_eq!(kept.len(), expected);
            }
        }

        #[test]
        fn resolve_next_task_never_returns_done_or_waiting(
            tasks in arb_forest(),
            idx in 0usize..8,
        ) {
            let current = tasks[idx % tasks.len()].clone();
            if let Some(next) = resolve_next_task(&tasks, &tasks, &current) {
                prop_assert_ne!(next.state, TaskState::Done);
                prop_assert_ne!(next.state, TaskState::Waiting);
            }
        }

        // -- #1121: filter_tasks_for_project --

        #[test]
        fn filter_tasks_for_project_membership_subset_and_drops_sessionless(
            n_a in 0usize..4,
            n_b in 0usize..4,
            task_kinds in proptest::collection::vec(0usize..3, 0..10),
        ) {
            let dir = TempDir::new().unwrap();

            let mut a_ids = Vec::new();
            for _ in 0..n_a {
                let StartOutcome::Created(s) =
                    start_session(dir.path(), None, None, "proj-a", StartDecision::New).unwrap()
                else {
                    panic!("StartDecision::New always creates");
                };
                a_ids.push(s.id);
            }
            let mut b_ids = Vec::new();
            for _ in 0..n_b {
                let StartOutcome::Created(s) =
                    start_session(dir.path(), None, None, "proj-b", StartDecision::New).unwrap()
                else {
                    panic!("StartDecision::New always creates");
                };
                b_ids.push(s.id);
            }

            let tasks: Vec<Task> = task_kinds
                .iter()
                .enumerate()
                .map(|(i, &kind)| {
                    let session = match kind {
                        1 if !a_ids.is_empty() => Some(a_ids[i % a_ids.len()].clone()),
                        2 if !b_ids.is_empty() => Some(b_ids[i % b_ids.len()].clone()),
                        _ => None,
                    };
                    Task {
                        slug: format!("t{i}"),
                        title: format!("task {i}"),
                        state: TaskState::Open,
                        parent: None,
                        blocked_on: Vec::new(),
                        notes: Vec::new(),
                        detail: None,
                        session,
                        created_at: "2026-01-01T00:00:00Z".to_string(),
                        updated_at: "2026-01-01T00:00:00Z".to_string(),
                        relation: Relation::Queued,
                        parallel: false,
                    }
                })
                .collect();

            let input_slugs: HashSet<String> = tasks.iter().map(|t| t.slug.clone()).collect();
            let a_id_set: HashSet<String> = a_ids.into_iter().collect();

            let expected_count = tasks
                .iter()
                .filter(|t| t.session.as_deref().is_some_and(|s| a_id_set.contains(s)))
                .count();

            let sessions = session::list_sessions_strict(dir.path()).expect("test");
            let result = filter_tasks_for_project(&sessions, "proj-a", tasks);

            for t in &result {
                // Subset: every returned task was in the input.
                prop_assert!(input_slugs.contains(&t.slug));
                // Membership: every returned task's session is a proj-a session.
                prop_assert!(t.session.as_deref().is_some_and(|s| a_id_set.contains(s)));
            }
            // Complement: nothing that should have been kept was dropped —
            // without this, the two checks above pass vacuously whenever
            // `result` is empty, and "drops_sessionless" (the test's own
            // name) was never actually pinned (#1121 pre-pr-review).
            prop_assert_eq!(result.len(), expected_count);
        }

        // -- #1121: resolve_current_task --

        #[test]
        fn resolve_current_task_never_returns_done(tasks in arb_forest()) {
            if let Some(current) = resolve_current_task(&tasks) {
                prop_assert_ne!(current.state, TaskState::Done);
            }
        }

        #[test]
        fn resolve_current_task_prefers_wip_when_present(tasks in arb_forest()) {
            if tasks.iter().any(|t| t.state == TaskState::Wip) {
                let current = resolve_current_task(&tasks);
                prop_assert_eq!(current.map(|t| t.state), Some(TaskState::Wip));
            }
        }

        #[test]
        fn resolve_current_task_none_when_everything_done(mut tasks in arb_forest()) {
            for t in &mut tasks {
                t.state = TaskState::Done;
            }
            prop_assert_eq!(resolve_current_task(&tasks), None);
        }

        // -- #1121: resolve_next_task --

        #[test]
        fn resolve_next_task_blockers_all_resolve_to_done(
            tasks in arb_forest_with_blockers(),
            idx in 0usize..8,
        ) {
            let current = tasks[idx % tasks.len()].clone();
            if let Some(next) = resolve_next_task(&tasks, &tasks, &current) {
                let by_slug: HashMap<&str, &Task> =
                    tasks.iter().map(|t| (t.slug.as_str(), t)).collect();
                for blocker in &next.blocked_on {
                    prop_assert!(is_done_including_descendants(blocker, &next.slug, &by_slug));
                }
            }
        }

        // #2300: the waiter's own subtree never decides whether its blocker is done.
        #[test]
        fn waiter_subtree_states_never_change_the_blocker_result(
            tasks in arb_forest(),
            b in 0usize..8,
            w in 0usize..8,
        ) {
            let blocker = format!("t{}", b % tasks.len());
            let waiter = format!("t{}", w % tasks.len());
            prop_assume!(blocker != waiter);
            prop_assume!(!in_subtree(&blocker, &waiter, &tasks));
            let with_states = |state: TaskState| {
                let mut ts = tasks.clone();
                for t in &mut ts {
                    if in_subtree(&t.slug, &waiter, &tasks) {
                        t.state = state;
                    }
                }
                ts
            };
            let (open, done) = (with_states(TaskState::Open), with_states(TaskState::Done));
            let index = |ts: &[Task]| -> bool {
                let by_slug: HashMap<&str, &Task> =
                    ts.iter().map(|t| (t.slug.as_str(), t)).collect();
                is_done_including_descendants(&blocker, &waiter, &by_slug)
            };
            prop_assert_eq!(index(&open), index(&done));
        }

        #[test]
        fn first_undone_is_an_undone_task_outside_the_waiter_subtree(
            tasks in arb_forest(),
            b in 0usize..8,
            w in 0usize..8,
        ) {
            let blocker = format!("t{}", b % tasks.len());
            let waiter = format!("t{}", w % tasks.len());
            prop_assume!(blocker != waiter);
            prop_assume!(!in_subtree(&blocker, &waiter, &tasks));
            let by_slug: HashMap<&str, &Task> =
                tasks.iter().map(|t| (t.slug.as_str(), t)).collect();
            let first = first_undone_in_subtree(&blocker, &waiter, &by_slug)
                .expect("arb_forest has no loop");
            if let Some(undone) = first {
                prop_assert!(in_subtree(undone, &blocker, &tasks));
                prop_assert!(!in_subtree(undone, &waiter, &tasks));
                prop_assert_ne!(by_slug[undone].state, TaskState::Done);
            }
        }

        #[test]
        fn resolve_next_task_comes_strictly_after_current_in_execution_order(
            tasks in arb_forest(),
            idx in 0usize..8,
        ) {
            let current = tasks[idx % tasks.len()].clone();
            if let Some(next) = resolve_next_task(&tasks, &tasks, &current) {
                let order = execution_order(&tasks);
                let current_pos = order
                    .iter()
                    .position(|t| t.slug == current.slug)
                    .expect("current came from tasks, so execution_order(tasks) must contain it");
                let next_pos = order
                    .iter()
                    .position(|t| t.slug == next.slug)
                    .expect("resolve_next_task draws next from execution_order(tasks) itself");
                prop_assert!(next_pos > current_pos);
            }
        }

        #[test]
        fn resolve_next_task_none_when_current_not_in_session_tasks(tasks in arb_forest()) {
            let dangling = Task {
                slug: "not-in-forest".to_string(),
                title: "dangling".to_string(),
                state: TaskState::Open,
                parent: None,
                blocked_on: Vec::new(),
                notes: Vec::new(),
                detail: None,
                session: Some("s".to_string()),
                created_at: "2026-01-01T00:00:00Z".to_string(),
                updated_at: "2026-01-01T00:00:00Z".to_string(),
                relation: Relation::Queued,
                parallel: false,
            };
            prop_assert_eq!(resolve_next_task(&tasks, &tasks, &dangling), None);
        }

        #[test]
        fn resolve_next_task_none_when_current_is_last_in_execution_order(tasks in arb_forest()) {
            let order = execution_order(&tasks);
            let last = order.last().unwrap().clone();
            prop_assert_eq!(resolve_next_task(&tasks, &tasks, &last), None);
        }

        // -- #930: edit_task parent-change cycle rejection --

        #[test]
        fn edit_task_parent_change_cycle_rejection_matches_forest_structure(
            forest in arb_forest(),
            a_idx in 0usize..8,
            b_idx in 0usize..8,
        ) {
            let a = forest[a_idx % forest.len()].clone();
            let b = forest[b_idx % forest.len()].clone();
            prop_assume!(a.slug != b.slug);

            let dir = TempDir::new().unwrap();
            for t in &forest {
                save_task(dir.path(), t).unwrap();
            }

            // Independent reference computation (doesn't call reject_cycle):
            // is `a` among `b`'s ancestors, per the forest's own `parent`
            // links? `arb_forest` guarantees every parent index is earlier
            // than its child's, so this walk always terminates.
            let mut a_is_ancestor_of_b = false;
            let mut current = b.parent.clone();
            while let Some(p) = current {
                if p == a.slug {
                    a_is_ancestor_of_b = true;
                    break;
                }
                current = forest.iter().find(|t| t.slug == p).and_then(|t| t.parent.clone());
            }

            let edit = TaskEdit {
                parent: Some(&b.slug),
                ..Default::default()
            };
            let result = edit_task(dir.path(), &a.slug, &edit);

            prop_assert_eq!(
                result.is_err(),
                a_is_ancestor_of_b,
                "setting '{}' parent to '{}': expected cycle rejection = {}, got is_err = {}",
                a.slug,
                b.slug,
                a_is_ancestor_of_b,
                result.is_err(),
            );
        }
    }
}
