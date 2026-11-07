use super::ancestors::ancestors_to_start;
use super::{reject_cycle, task_path};
use tempfile::TempDir;

#[test]
fn a_corrupt_ancestor_fails_the_parent_change() {
    let dir = TempDir::new().expect("tempdir");
    let path = task_path(dir.path(), "ancestor");
    std::fs::create_dir_all(path.parent().expect("task dir")).expect("mkdir");
    std::fs::write(&path, "{not task json").expect("write corrupt ancestor");

    let err = reject_cycle(dir.path(), "child", "ancestor")
        .expect_err("a corrupt ancestor must not pass silently");

    let msg = format!("{err:#}");
    assert!(msg.contains("ancestor"), "must name the ancestor: {msg}");
}

#[test]
fn a_missing_ancestor_fails_the_parent_change_with_its_fix() {
    let dir = TempDir::new().expect("tempdir");

    let err = reject_cycle(dir.path(), "child", "absent")
        .expect_err("a missing ancestor must not end the walk silently");

    let msg = format!("{err:#}");
    assert!(
        msg.contains("'absent'"),
        "must name the missing task: {msg}"
    );
    assert!(msg.contains("Fix:"), "must state the fix: {msg}");
}

/// Writes a stored task `slug` whose parent is `parent`, with the fields the store needs.
fn write_task_with_parent(dir: &std::path::Path, slug: &str, parent: &str) {
    let path = task_path(dir, slug);
    std::fs::create_dir_all(path.parent().expect("task dir")).expect("mkdir");
    let body = serde_json::json!({
        "slug": slug,
        "title": slug,
        "parent": parent,
        "created_at": "2026-01-01T00:00:00Z",
        "updated_at": "2026-01-01T00:00:00Z",
    });
    std::fs::write(&path, body.to_string()).expect("write task");
}

#[test]
fn a_loop_among_the_ancestors_fails_the_parent_change_with_its_fix() {
    let dir = TempDir::new().expect("tempdir");
    write_task_with_parent(dir.path(), "loop-a", "loop-b");
    write_task_with_parent(dir.path(), "loop-b", "loop-a");

    let err = reject_cycle(dir.path(), "child", "loop-a")
        .expect_err("a loop above the new parent must not pass silently");

    let msg = format!("{err:#}");
    assert!(msg.contains("loop"), "must say the ancestors loop: {msg}");
    assert!(msg.contains("Fix:"), "must state the fix: {msg}");
}

/// The answer a walk over the same links gives: `true` when `new_parent` reaches a root with no
/// parent, without passing task 0 or a loop or a missing task. Task indexes at or past the
/// length are missing.
fn reference_accepts(parents: &[Option<usize>], new_parent: usize) -> bool {
    let mut current = new_parent;
    let mut seen = std::collections::HashSet::new();
    loop {
        if current == 0 || !seen.insert(current) {
            return false;
        }
        let Some(parent) = parents.get(current) else {
            return false;
        };
        match parent {
            None => return true,
            Some(next) => current = *next,
        }
    }
}

proptest::proptest! {
    #![proptest_config(proptest::prelude::ProptestConfig::with_cases(64))]
    // On every small store, with loops and missing tasks, `reject_cycle` accepts exactly when a
    // reference walk over the same links finds a root (#3600).
    #[test]
    fn reject_cycle_agrees_with_a_reference_walk(
        parents in proptest::collection::vec(proptest::option::of(0usize..7), 1..6),
        new_parent in 0usize..7,
    ) {
        let dir = TempDir::new().unwrap();
        for (index, parent) in parents.iter().enumerate() {
            write_chain_task(dir.path(), index, *parent, false, true);
        }
        let new_parent_name = format!("p{new_parent}");
        let accepted = reject_cycle(dir.path(), "p0", &new_parent_name).is_ok();
        proptest::prop_assert_eq!(accepted, reference_accepts(&parents, new_parent));
    }
}

/// Writes a stored task `p{index}` with the given state, relation, and parent.
fn write_chain_task(
    dir: &std::path::Path,
    index: usize,
    parent: Option<usize>,
    child: bool,
    open: bool,
) {
    let path = task_path(dir, &format!("p{index}"));
    std::fs::create_dir_all(path.parent().expect("task dir")).expect("mkdir");
    let body = serde_json::json!({
        "slug": format!("p{index}"),
        "title": format!("p{index}"),
        "parent": parent.map(|j| format!("p{j}")),
        "relation": if child { "child" } else { "queued" },
        "state": if open { "open" } else { "done" },
        "created_at": "2026-01-01T00:00:00Z",
        "updated_at": "2026-01-01T00:00:00Z",
    });
    std::fs::write(&path, body.to_string()).expect("write task");
}

proptest::proptest! {
    #![proptest_config(proptest::prelude::ProptestConfig::with_cases(48))]
    // A chain of sub-tasks ends at a top-level task. With `force`, the start promotes exactly the
    // open ancestors, child to root, and skips the done ones (#3600).
    #[test]
    fn starting_a_chain_promotes_exactly_its_open_ancestors(
        open in proptest::collection::vec(proptest::bool::ANY, 1..5),
    ) {
        let dir = TempDir::new().unwrap();
        let length = open.len();
        // p0 is the sub-task being started. p1 .. p{length} are its ancestors.
        write_chain_task(dir.path(), 0, Some(1), true, true);
        for (offset, is_open) in open.iter().enumerate() {
            let index = offset + 1;
            let is_top = index == length;
            let parent = if is_top { None } else { Some(index + 1) };
            write_chain_task(dir.path(), index, parent, !is_top, *is_open);
        }
        let all = crate::list_tasks_strict(dir.path()).unwrap();
        let child = crate::load_task(dir.path(), "p0").unwrap();
        let promoted: Vec<String> = ancestors_to_start(dir.path(), &child, true, &all)
            .unwrap()
            .into_iter()
            .map(|t| t.slug)
            .collect();
        let expected: Vec<String> = open
            .iter()
            .enumerate()
            .filter(|(_, is_open)| **is_open)
            .map(|(offset, _)| format!("p{}", offset + 1))
            .collect();
        proptest::prop_assert_eq!(promoted, expected);
    }
}
