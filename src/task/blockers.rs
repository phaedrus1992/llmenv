//! The blocker walk: which `blocked_on` refs are done, counting each task's subtree.

use std::collections::{HashMap, HashSet};

use super::*;

/// Record an ordering dependency: `input` is blocked on `on`.
///
/// # Errors
/// Errors if `on` doesn't resolve to an existing task — this is a fresh
/// write, not a load of possibly-stale state, so it's validated eagerly
/// (unlike the load-time tolerance for dangling `blocked_on` entries left
/// behind by a since-deleted task file). Also errors if `input` and `on`
/// resolve to the same task — a task cannot block itself.
pub(crate) fn block_task<'a>(
    state_dir: &Path,
    target: impl Into<TaskTarget<'a>>,
    on: &str,
) -> anyhow::Result<Task> {
    let target = target.into();
    with_store_lock(state_dir, || {
        // One strict read resolves both names (#3477).
        target.check()?;
        resolve::check_identifier(on)?;
        let snapshot = target.tasks(state_dir)?;
        let all_tasks: &StrictTasks = &snapshot;
        let slug = target.slug_in(state_dir, all_tasks)?;
        let on_slug = resolve::resolve_in(state_dir, on, all_tasks)?;
        if slug == on_slug {
            anyhow::bail!("task '{slug}' cannot be blocked on itself");
        }
        block_cycle::reject_block_cycle(all_tasks.as_slice(), &slug, &on_slug)?;
        let mut task = load_task(state_dir, &slug)?;
        if !task.blocked_on.contains(&on_slug) {
            task.blocked_on.push(on_slug);
        }
        task.updated_at = now_rfc3339();
        save_task(state_dir, &task)?;
        Ok(task)
    })
}

#[cfg(test)]
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
    ensure_waiter_chain_ends(waiter, by_slug)?;
    // A dangling or undone root needs no child index, which keeps the common answer cheap.
    match by_slug.get(slug) {
        Some(task) if task.state == TaskState::Done => {}
        _ => return Ok(Some(slug)),
    }
    // One parent-to-children index serves the whole walk, and an explicit stack replaces the
    // recursion, so a deep chain of done tasks costs O(n) time and no call stack (#3757).
    let mut children: HashMap<&str, Vec<&str>> = HashMap::new();
    for t in by_slug.values().filter(|t| t.slug != waiter) {
        if let Some(parent) = t.parent.as_deref() {
            children.entry(parent).or_default().push(t.slug.as_str());
        }
    }
    for list in children.values_mut() {
        list.sort_unstable_by(|a, b| b.cmp(a));
    }
    let mut visited: HashSet<&str> = HashSet::new();
    let mut stack: Vec<&str> = vec![slug];
    while let Some(current) = stack.pop() {
        if !visited.insert(current) {
            anyhow::bail!(
                "the parent links of task '{current}' loop, so its blocker walk cannot end. \
                 {LOOP_FIX}"
            );
        }
        match by_slug.get(current) {
            Some(task) if task.state == TaskState::Done => {}
            _ => return Ok(Some(current)),
        }
        // The lists hold the children in reverse slug order, so the stack pops the first one first.
        if let Some(list) = children.get(current) {
            stack.extend(list.iter().copied());
        }
    }
    Ok(None)
}

/// Why a blocker keeps `task` from starting: the first blocker that is not done. A blocker that
/// does not exist has nothing to finish, so its fix is to remove it. `None` when every blocker is
/// done. The caller has already checked the blocker walks for loops.
pub(super) fn unmet_blocker(task: &Task, by_slug: &HashMap<&str, &Task>) -> Option<String> {
    let blocker = task
        .blocked_on
        .iter()
        .find(|b| !is_done_including_descendants(b, &task.slug, by_slug))?;
    Some(if by_slug.contains_key(blocker.as_str()) {
        format!("it waits on '{blocker}', which is not done. Fix: finish '{blocker}' first")
    } else {
        format!(
            "it waits on '{blocker}', which does not exist. Fix: remove it with `llmenv task edit \
             {} --unblock {blocker}`",
            task.slug
        )
    })
}

/// The first task of `open`, in the order given, that `llmenv task start` would refuse, and why. A
/// loop in a blocker walk comes before any other refusal, because a loop is damaged data that the
/// user must repair, and a wait ends by itself (#3618, #3763, #3764, #3770). `all` is every task
/// of the store, as [`super::startability::refusal`] needs it.
pub(super) fn first_obstacle(
    open: &[&Task],
    all: &[Task],
    by_slug: &HashMap<&str, &Task>,
) -> Option<(String, String)> {
    open.iter()
        .find_map(|t| blocker_walk_error(t, by_slug).map(|why| (t.slug.to_owned(), why)))
        .or_else(|| {
            open.iter().find_map(|t| {
                super::startability::refusal(t, all, by_slug).map(|why| (t.slug.to_owned(), why))
            })
        })
}

/// The fix that every loop error of the blocker walk ends with.
const LOOP_FIX: &str =
    "Fix: clear the parent of one task in the loop with `llmenv task edit <task> --no-parent`";

/// Errors when the parent links above `waiter` loop. The child walk skips the subtree of
/// `waiter`, so it never enters a loop that passes through `waiter`. This check reports that
/// loop (#3618).
fn ensure_waiter_chain_ends(waiter: &str, by_slug: &HashMap<&str, &Task>) -> anyhow::Result<()> {
    let mut seen: HashSet<&str> = HashSet::new();
    let mut current = Some(waiter);
    while let Some(slug) = current {
        anyhow::ensure!(
            seen.insert(slug),
            "the parent links of task '{slug}' loop, so the blocker walk of '{waiter}' cannot \
             end. {LOOP_FIX}"
        );
        current = by_slug.get(slug).and_then(|t| t.parent.as_deref());
    }
    Ok(())
}

/// [`ensure_waiter_chain_ends`] for the tests of other modules.
#[cfg(test)]
pub(super) fn ensure_waiter_chain_ends_for_test(
    waiter: &str,
    by_slug: &HashMap<&str, &Task>,
) -> anyhow::Result<()> {
    ensure_waiter_chain_ends(waiter, by_slug)
}

/// The text of the first blocker walk of `task` that fails, or `None` when every walk ends. A
/// task with such a walk is never offered as the next step, and the reminder names the loop
/// (#3618).
pub(super) fn blocker_walk_error(task: &Task, by_slug: &HashMap<&str, &Task>) -> Option<String> {
    task.blocked_on.iter().find_map(|blocker| {
        first_undone_in_subtree(blocker, &task.slug, by_slug)
            .err()
            .map(|err| format!("{err:#}"))
    })
}
