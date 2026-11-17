//! `llmenv task edit`: change a task's title, parent, blockers, and notes in one save.

use std::path::Path;

use super::*;

/// What to change in an [`edit_task`] call. Every field defaults to "leave
/// unchanged" / "nothing to add or remove" — a call with every field at its
/// default is a legal no-op (#930), still bumping `updated_at`.
#[derive(Debug, Clone, Copy, Default)]
pub struct TaskEdit<'a> {
    /// New title, if changing it.
    pub(crate) title: Option<&'a str>,
    /// Set the parent to this id. Mutually exclusive with `no_parent` — the
    /// CLI enforces this with `conflicts_with`; if both are set here,
    /// `parent` wins.
    pub(crate) parent: Option<&'a str>,
    /// Clear the parent.
    pub(crate) no_parent: bool,
    /// Ids to add to `blocked_on`. Idempotent — an id already present is a
    /// no-op, same as `block_task`.
    pub(crate) block_on: &'a [String],
    /// Ids to remove from `blocked_on`. Idempotent — an id not present is a
    /// no-op.
    pub(crate) unblock: &'a [String],
    /// Append a new note.
    pub(crate) add_note: Option<&'a str>,
    /// Remove a note, identified by its 0-based index or its exact RFC3339
    /// `at` timestamp.
    pub(crate) delete_note: Option<&'a str>,
    /// Replace the detail. An empty string clears it.
    pub(crate) detail: Option<&'a str>,
}

/// Mutate an existing task's title, parent, `blocked_on` set, and notes in a
/// single load-mutate-save cycle (#930).
///
/// # Errors
/// - `edit.parent` doesn't resolve to an existing task, names `input` itself,
///   or would make `input` its own ancestor (cycle) — see [`reject_cycle`].
/// - Any `edit.block_on` id doesn't resolve to an existing task, or names
///   `input` itself — same eager-validation reasoning as [`block_task`].
/// - Any `edit.unblock` id doesn't resolve to an existing task.
/// - `edit.delete_note` doesn't match an existing note's index or timestamp.
pub(crate) fn edit_task<'a>(
    state_dir: &Path,
    target: impl Into<TaskTarget<'a>>,
    edit: &TaskEdit<'_>,
) -> anyhow::Result<Task> {
    let target = target.into();
    let task = with_store_lock(state_dir, || {
        // One strict read resolves the task, the parent, and every blocker name (#3477). Each
        // name is checked first, so a bad name gets its own error even over a broken store.
        target.check()?;
        check_names(edit)?;
        let snapshot = target.tasks(state_dir)?;
        let ctx = EditContext {
            state_dir,
            all_tasks: &snapshot,
        };
        let slug = target.slug_in(state_dir, ctx.all_tasks)?;
        let mut task = load_task(state_dir, &slug)?;
        if let Some(title) = edit.title {
            task.title = title.to_string();
        }
        set_parent(&ctx, &mut task, edit)?;
        change_blockers(&ctx, &mut task, edit)?;
        change_notes(&mut task, edit)?;
        if let Some(detail) = edit.detail {
            task.detail = (!detail.is_empty()).then(|| detail.to_string());
        }
        task.updated_at = now_rfc3339();
        save_task(state_dir, &task)?;
        Ok(task)
    })?;
    touch_task_session(state_dir, &task);
    Ok(task)
}

/// The store and the one strict read of an edit, so each name resolves against the same tasks.
struct EditContext<'a> {
    state_dir: &'a Path,
    all_tasks: &'a StrictTasks,
}

impl EditContext<'_> {
    fn resolve(&self, name: &str) -> anyhow::Result<String> {
        resolve::resolve_in(self.state_dir, name, self.all_tasks)
    }
}

/// Refuse a name in the edit that cannot be a task identifier.
fn check_names(edit: &TaskEdit<'_>) -> anyhow::Result<()> {
    edit.parent
        .into_iter()
        .chain(edit.block_on.iter().map(String::as_str))
        .chain(edit.unblock.iter().map(String::as_str))
        .try_for_each(resolve::check_identifier)
}

/// Apply `--parent` or `--no-parent`. A parent wins over `--no-parent`.
fn set_parent(ctx: &EditContext<'_>, task: &mut Task, edit: &TaskEdit<'_>) -> anyhow::Result<()> {
    let Some(parent_input) = edit.parent else {
        if edit.no_parent {
            task.parent = None;
        }
        return Ok(());
    };
    let parent_slug = ctx.resolve(parent_input)?;
    if parent_slug == task.slug {
        anyhow::bail!("task '{}' cannot be its own parent", task.slug);
    }
    let parent_task = ctx
        .all_tasks
        .as_slice()
        .iter()
        .find(|t| t.slug == parent_slug);
    ownership::ensure_parent_in_session(parent_task, &parent_slug, task.session.as_deref())?;
    reject_cycle(ctx.state_dir, &task.slug, &parent_slug)?;
    task.parent = Some(parent_slug);
    Ok(())
}

/// Add the `--block-on` ids to `blocked_on`, then remove the `--unblock` ids.
fn change_blockers(
    ctx: &EditContext<'_>,
    task: &mut Task,
    edit: &TaskEdit<'_>,
) -> anyhow::Result<()> {
    for on in edit.block_on {
        let on_slug = ctx.resolve(on)?;
        if on_slug == task.slug {
            anyhow::bail!("task '{}' cannot be blocked on itself", task.slug);
        }
        block_cycle::reject_block_cycle(ctx.all_tasks.as_slice(), &task.slug, &on_slug)?;
        if !task.blocked_on.contains(&on_slug) {
            task.blocked_on.push(on_slug);
        }
    }
    for on in edit.unblock {
        let on_slug = ctx.resolve(on)?;
        task.blocked_on.retain(|b| b != &on_slug);
    }
    Ok(())
}

/// Add the new note, then remove the note that `--delete-note` names.
fn change_notes(task: &mut Task, edit: &TaskEdit<'_>) -> anyhow::Result<()> {
    if let Some(text) = edit.add_note {
        task.notes.push(TaskNote {
            at: now_rfc3339(),
            text: text.to_string(),
        });
    }
    if let Some(id) = edit.delete_note {
        let idx = resolve_note_index(&task.notes, id)?;
        task.notes.remove(idx);
    }
    Ok(())
}
