//! The ancestors that `llmenv task start` puts in progress with a sub-task (#2455).

use std::path::Path;

use super::{Relation, StrictTasks, Task, TaskState, is_not_found, load_task, relation, session};

/// The `open` ancestors that starting the sub-task `child` puts in progress (#2455). A queued
/// ancestor is held by the queue like any task, and a `done` ancestor refuses unless `force`.
/// A top-level task has no ancestors to start.
pub(super) fn ancestors_to_start(
    state_dir: &Path,
    child: &Task,
    force: bool,
    all_tasks: &StrictTasks,
) -> anyhow::Result<Vec<Task>> {
    let mut promoted = Vec::new();
    let mut seen = vec![child.slug.clone()];
    let mut current = child.clone();
    while current.relation == Relation::Child {
        let Some(parent_slug) = current.parent.clone() else {
            anyhow::bail!(
                "cannot start '{}': sub-task '{}' has no parent. Fix: repair the task file of '{}' \
                 so that its parent link matches its relation",
                child.slug,
                current.slug,
                current.slug
            );
        };
        if seen.contains(&parent_slug) {
            anyhow::bail!(
                "cannot start '{}': its ancestors loop at '{parent_slug}'. Fix: clear the parent \
                 of one task in that loop with `llmenv task edit <task> --no-parent`",
                child.slug
            );
        }
        // A missing parent fails the start with the fix, so a dangling link is never passed over
        // silently (#3591). Any other read error fails the start, so the done and queue checks
        // never run blind.
        let parent = match load_task(state_dir, &parent_slug) {
            Ok(parent) => parent,
            Err(e) if is_not_found(&e) => anyhow::bail!(
                "cannot start '{}': its parent '{parent_slug}' is missing from the task store. \
                 Fix: clear the parent with `llmenv task edit {} --no-parent`",
                child.slug,
                child.slug
            ),
            Err(e) => {
                return Err(e.context(format!(
                    "cannot start '{}': ancestor '{parent_slug}' is unreadable. Repair or \
                     remove its task file, then start again",
                    child.slug
                )));
            }
        };
        if parent.state == TaskState::Done && !force {
            anyhow::bail!(
                "the parent '{parent_slug}' of '{}' is done. Reopen it with `llmenv task start \
                 --reopen {parent_slug}`, or pass --force.",
                child.slug
            );
        }
        if parent.state == TaskState::Open {
            // An open ancestor with no session has no queue to check. It is promoted as before
            // sessions existed (#3586).
            if !force
                && let Some(session_id) = &parent.session
                && let Some(block) = relation::queue_block(
                    &parent,
                    &session::filter::tasks_in_sessions(
                        all_tasks.as_slice(),
                        &std::iter::once(session_id.clone()).collect(),
                    ),
                )
            {
                anyhow::bail!("the parent of '{}' cannot start: {block}", child.slug);
            }
            promoted.push(parent.clone());
        }
        seen.push(parent_slug);
        current = parent;
    }
    Ok(promoted)
}

/// The error of a failed save of the promoted ancestors in `start`. The saves are not one write,
/// so a failure can leave some ancestors `wip` while the task stays `open`. The message names the
/// whole set, so the user can see which ones may have moved.
pub(super) fn promotion_failed(slug: &str, names: &[String], err: anyhow::Error) -> anyhow::Error {
    err.context(format!(
        "cannot start '{slug}': saving the ancestors {} as wip failed, and '{slug}' stays open. \
         Fix: run `llmenv task start {slug}` again once the store is writable",
        names.join(", ")
    ))
}
