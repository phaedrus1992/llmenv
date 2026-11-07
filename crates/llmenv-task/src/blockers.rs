//! The blocker walk: which `blocked_on` refs are done, counting each task's subtree.

use std::collections::{HashMap, HashSet};

use super::{Task, TaskState};

/// True when `task` is neither `done` nor `waiting`, and every one of its
/// `blocked_on` refs is (recursively) done per [`is_done_including_descendants`].
/// A dangling or not-yet-done blocker keeps the task non-actionable — fail
/// closed rather than treat an unresolvable reference as satisfied.
pub(super) fn is_actionable(task: &Task, by_slug: &HashMap<&str, &Task>) -> bool {
    !matches!(task.state, TaskState::Done | TaskState::Waiting)
        && task
            .blocked_on
            .iter()
            .all(|b| is_done_including_descendants(b, &task.slug, by_slug))
}

/// True when the task named `slug` is `done` and every task transitively
/// parented under it (its children, their children, …) is also `done` —
/// the resolution `blocked_on` uses (#1164), so blocking on a parent task
/// alone covers its whole child set without a block edge per sibling.
///
/// A dangling `slug` (no task in `by_slug`) resolves to `false` — fail
/// closed, same rationale as the old non-recursive check this replaces. A
/// malformed parent cycle terminates via the `visited` guard (mirroring
/// [`append_forest`]'s guard) instead of recursing forever; a cycle is
/// invalid data, not a case this needs to resolve "correctly" for, only
/// safely.
///
/// The walk skips the subtree of `waiter`, the task that is blocked on
/// `slug`. `task add` chains a new task under the previous one by default,
/// so the waiter is often a descendant of its own blocker. Its subtree
/// cannot be done before the waiter starts, and would block it forever (#2300).
pub(super) fn is_done_including_descendants(
    slug: &str,
    waiter: &str,
    by_slug: &HashMap<&str, &Task>,
) -> bool {
    // A loop is not done. This fails closed, so the task stays out of the next-task choice, and
    // `start_task` reports the loop with its fix (#3601).
    match first_undone_in_subtree(slug, waiter, by_slug) {
        Ok(undone) => undone.is_none(),
        Err(err) => {
            tracing::warn!(
                error = %format!("{err:#}"),
                blocker = slug,
                "task next: a blocker walk failed, so the task is not offered"
            );
            false
        }
    }
}

/// The first task in `slug`'s subtree (skipping `waiter`'s) that holds
/// [`is_done_including_descendants`] false: `slug` itself when it is dangling
/// or not done, else the first undone descendant in slug order. `None` when
/// the whole subtree is done.
///
/// # Errors
/// Errors if the child links loop back to a task already visited. A tree has
/// one parent per task, so a revisit is a loop in the store (#3601).
pub(super) fn first_undone_in_subtree<'a>(
    slug: &'a str,
    waiter: &str,
    by_slug: &HashMap<&'a str, &'a Task>,
) -> anyhow::Result<Option<&'a str>> {
    fn go<'a>(
        slug: &'a str,
        waiter: &str,
        by_slug: &HashMap<&'a str, &'a Task>,
        visited: &mut HashSet<&'a str>,
    ) -> anyhow::Result<Option<&'a str>> {
        if !visited.insert(slug) {
            anyhow::bail!(
                "the parent links of task '{slug}' loop, so its blocker walk cannot end. Fix: \
                 clear the parent of one task in the loop with `llmenv task edit <task> --no-parent`"
            );
        }
        let Some(&task) = by_slug.get(slug) else {
            return Ok(Some(slug));
        };
        if task.state != TaskState::Done {
            return Ok(Some(slug));
        }
        let mut children: Vec<&'a str> = by_slug
            .values()
            .filter(|t| t.parent.as_deref() == Some(slug) && t.slug != waiter)
            .map(|t| t.slug.as_str())
            .collect();
        children.sort_unstable();
        for child in children {
            if let Some(undone) = go(child, waiter, by_slug, visited)? {
                return Ok(Some(undone));
            }
        }
        Ok(None)
    }
    go(slug, waiter, by_slug, &mut HashSet::new())
}
