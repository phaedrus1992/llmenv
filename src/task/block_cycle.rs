//! The cycle check of the `blocked_on` links: a task cannot be blocked, directly or through other
//! tasks, on itself.

use std::collections::{HashMap, HashSet};

use super::Task;

/// Errs if making `slug` blocked on `on_slug` closes a loop of `blocked_on` links. The walk starts
/// at `on_slug` and follows each blocker in `all_tasks`. It stops at a task that is not in the
/// store, because a dangling blocker is tolerated at load and cannot lead back to `slug`.
///
/// # Errors
/// Errors when the walk reaches `slug`. The message names both tasks and the fix.
pub(super) fn reject_block_cycle(
    all_tasks: &[Task],
    slug: &str,
    on_slug: &str,
) -> anyhow::Result<()> {
    let by_slug: HashMap<&str, &Task> = all_tasks.iter().map(|t| (t.slug.as_str(), t)).collect();
    let mut visited = HashSet::new();
    let mut stack = vec![on_slug];
    while let Some(current) = stack.pop() {
        if current == slug {
            anyhow::bail!(
                "task '{slug}' cannot be blocked on '{on_slug}': '{on_slug}' is already blocked, \
                 directly or through other tasks, on '{slug}'. Fix: remove one of the blocks in \
                 that chain with `llmenv task edit <task> --unblock <blocker>`"
            );
        }
        if !visited.insert(current) {
            continue;
        }
        if let Some(task) = by_slug.get(current) {
            stack.extend(task.blocked_on.iter().map(String::as_str));
        }
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    fn task(slug: &str, blocked_on: &[&str]) -> Task {
        serde_json::from_value(serde_json::json!({
            "slug": slug,
            "title": slug,
            "blocked_on": blocked_on,
            "created_at": "2026-01-01T00:00:00Z",
            "updated_at": "2026-01-01T00:00:00Z",
        }))
        .unwrap()
    }

    #[test]
    fn a_direct_loop_is_refused() {
        let all = [task("b", &["a"])];
        let err = reject_block_cycle(&all, "a", "b").unwrap_err();
        assert!(format!("{err:#}").contains("Fix:"), "{err:#}");
    }

    #[test]
    fn a_loop_through_several_tasks_is_refused() {
        let all = [task("b", &["c"]), task("c", &["d"]), task("d", &["a"])];
        assert!(reject_block_cycle(&all, "a", "b").is_err());
    }

    #[test]
    fn a_chain_with_no_return_is_allowed() {
        let all = [task("b", &["c"]), task("c", &[])];
        reject_block_cycle(&all, "a", "b").unwrap();
    }

    #[test]
    fn a_dangling_blocker_ends_the_walk() {
        let all = [task("b", &["gone"])];
        reject_block_cycle(&all, "a", "b").unwrap();
    }

    #[test]
    fn a_loop_that_does_not_reach_the_task_does_not_hang() {
        let all = [task("b", &["c"]), task("c", &["b"])];
        reject_block_cycle(&all, "a", "b").unwrap();
    }
}
