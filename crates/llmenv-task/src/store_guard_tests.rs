//! Tests of the store guards: the blocker cycle check through the public commands, and the message
//! of a failed ancestor save in `start_task`.

use tempfile::TempDir;

use super::*;

fn mk(dir: &Path, title: &str, parent: Option<&str>) -> Task {
    let parent_spec = match parent {
        Some(p) => ParentSpec::Explicit(p),
        None => ParentSpec::Detached,
    };
    add_task_for_session(dir, title, parent_spec, "test-session").expect("add task")
}

#[test]
fn block_task_refuses_the_loop_that_would_close() {
    let dir = TempDir::new().expect("tempdir");
    let a = mk(dir.path(), "Task A", None);
    let b = mk(dir.path(), "Task B", None);
    block_task(dir.path(), &a.slug, &b.slug).expect("a on b");
    let err = block_task(dir.path(), &b.slug, &a.slug).expect_err("b on a closes the loop");
    let text = format!("{err:#}");
    assert!(text.contains("cannot be blocked on"), "{text}");
    assert!(text.contains("Fix:"), "{text}");
    let on_disk = load_task(dir.path(), &b.slug).expect("load b");
    assert!(
        on_disk.blocked_on.is_empty(),
        "b must not keep the refused block"
    );
}

#[test]
fn edit_task_refuses_a_block_that_closes_a_loop_through_two_tasks() {
    let dir = TempDir::new().expect("tempdir");
    let a = mk(dir.path(), "Task A", None);
    let b = mk(dir.path(), "Task B", None);
    let c = mk(dir.path(), "Task C", None);
    block_task(dir.path(), &a.slug, &b.slug).expect("a on b");
    block_task(dir.path(), &b.slug, &c.slug).expect("b on c");
    let block_on = [a.slug.clone()];
    let edit = TaskEdit {
        block_on: &block_on,
        ..TaskEdit::default()
    };
    let err = edit_task(dir.path(), &c.slug, &edit).expect_err("c on a closes the loop");
    assert!(format!("{err:#}").contains("Fix:"), "{err:#}");
}

#[test]
fn a_failed_ancestor_save_names_every_promoted_ancestor() {
    let names = ["grand".to_string(), "parent".to_string()];
    let err = std::io::Error::other("disk full");
    let text = format!(
        "{:#}",
        ancestors::promotion_failed("child", &names, err.into())
    );
    assert!(text.contains("grand, parent"), "{text}");
    assert!(text.contains("stays open"), "{text}");
    assert!(
        text.contains("Fix: run `llmenv task start child`"),
        "{text}"
    );
    assert!(text.contains("disk full"), "{text}");
}
