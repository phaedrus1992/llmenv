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
    walk_ancestors(child, force, all_tasks.as_slice(), |slug| {
        load_task(state_dir, slug)
    })
}

/// A refusal that comes from a damaged link, not from a wait: no parent, a loop, a missing
/// parent, or a parent of another session. The user must repair the store, so `task show --next`
/// reports it (#3777). Its text is the text of the refusal.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
struct Damaged(String);

/// Return a [`Damaged`] error with the formatted text.
macro_rules! damaged {
    ($($arg:tt)*) => {
        return Err(anyhow::Error::new(Damaged(format!($($arg)*))))
    };
}

/// The text of the damaged link above `child`, or `None` when no link is damaged. A wait, such as
/// a queue hold or a done parent, is not damage and does not hide damage further up, so the walk
/// runs as a forced start: `force` skips the waits and keeps the damage checks.
pub(super) fn ancestor_damage(child: &Task, all: &[Task]) -> Option<String> {
    let error = walk_ancestors(child, true, all, lookup_in(all)).err()?;
    error
        .downcast_ref::<Damaged>()
        .map(|damaged| damaged.to_string())
}

/// The parent lookup of a walk over `all`: a slug that is absent fails with a not-found error.
fn lookup_in(all: &[Task]) -> impl Fn(&str) -> anyhow::Result<Task> {
    move |slug| {
        all.iter()
            .find(|t| t.slug == slug)
            .map(Task::to_owned)
            .ok_or_else(|| anyhow::Error::new(std::io::Error::from(std::io::ErrorKind::NotFound)))
    }
}

/// The text of the refusal that starting `child` meets at an ancestor, or `None` when no ancestor
/// refuses. The parents come from `all`, so a reminder asks the same question as the start does
/// and never names a start that the start refuses (#3770). `all` must come from a strict read,
/// because a parent that is absent from it is reported as missing.
pub(super) fn ancestor_refusal(child: &Task, all: &[Task]) -> Option<String> {
    walk_ancestors(child, false, all, lookup_in(all))
        .err()
        .map(|e| format!("{e:#}"))
}

/// The walk of [`ancestors_to_start`], with the parent lookup as a parameter. `lookup` fails with a
/// not-found error for a parent that is missing, and with any other error for one it cannot read.
fn walk_ancestors(
    child: &Task,
    force: bool,
    all: &[Task],
    lookup: impl Fn(&str) -> anyhow::Result<Task>,
) -> anyhow::Result<Vec<Task>> {
    let mut promoted = Vec::new();
    let mut seen = vec![child.slug.clone()];
    let mut current = child.clone();
    while current.relation == Relation::Child {
        let Some(parent_slug) = current.parent.clone() else {
            damaged!(
                "cannot start '{}': sub-task '{}' has no parent. Fix: repair the task file of '{}' \
                 so that its parent link matches its relation",
                child.slug,
                current.slug,
                current.slug
            );
        };
        if seen.contains(&parent_slug) {
            damaged!(
                "cannot start '{}': its ancestors loop at '{parent_slug}'. Fix: clear the parent \
                 of one task in that loop with `llmenv task edit <task> --no-parent`",
                child.slug
            );
        }
        // A missing parent fails the start with the fix, so a dangling link is never passed over
        // silently (#3591). Any other read error fails the start, so the done and queue checks
        // never run blind.
        let parent = match lookup(&parent_slug) {
            Ok(parent) => parent,
            Err(e) if is_not_found(&e) => damaged!(
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
            // A link made before the #3480 and #3593 refusals can cross sessions. Promoting that
            // ancestor would change the task of another session (#3619). Every hop is compared
            // with the task that the caller starts, so a foreign ancestor above a `wip`
            // intermediate parent is refused too. An ancestor with no session is promoted as
            // before sessions existed (#3586).
            if let Some(owner) = parent.session.as_deref()
                && Some(owner) != child.session.as_deref()
            {
                damaged!(
                    "cannot start '{}': its ancestor '{parent_slug}' belongs to session \
                     '{owner}', so starting would change a task of another session. Fix: clear \
                     the parent with `llmenv task edit {} --no-parent`",
                    child.slug,
                    current.slug
                );
            }
            // An open ancestor with no session has no queue to check. It is promoted as before
            // sessions existed (#3586).
            if !force
                && let Some(session_id) = &parent.session
                && let Some(block) = relation::queue_block(
                    &parent,
                    &session::filter::tasks_in_sessions(
                        all,
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
/// so a failure can leave the first `saved` ancestors of `names` `wip` while the task stays
/// `open`. The message names the ones that were saved and the one that failed.
pub(super) fn promotion_failed(
    slug: &str,
    names: &[String],
    saved: usize,
    err: anyhow::Error,
) -> anyhow::Error {
    let done = names.get(..saved).unwrap_or(names).join(", ");
    let failed = names.get(saved).map_or("", String::as_str);
    err.context(format!(
        "cannot start '{slug}': saving the ancestor '{failed}' as wip failed, and '{slug}' stays \
         open. Ancestors already saved as wip: [{done}]. Fix: run `llmenv task start {slug}` again \
         once the store is writable"
    ))
}
