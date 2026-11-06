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
            break;
        };
        if seen.contains(&parent_slug) {
            break;
        }
        // A deleted parent leaves a dangling link, which the rest of the store tolerates. Any
        // other read error fails the start, so the done and queue checks never run blind.
        let parent = match load_task(state_dir, &parent_slug) {
            Ok(parent) => parent,
            Err(e) if is_not_found(&e) => break,
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
