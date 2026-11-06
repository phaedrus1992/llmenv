//! `llmenv task edit`: change a task's title, parent, blockers, and notes in one save.

use std::path::Path;

use super::*;

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
        for name in edit
            .parent
            .into_iter()
            .chain(edit.block_on.iter().map(String::as_str))
            .chain(edit.unblock.iter().map(String::as_str))
        {
            resolve::check_identifier(name)?;
        }
        let all_tasks = list_tasks_strict(state_dir)?;
        let resolve = |name: &str| resolve::resolve_in(state_dir, name, &all_tasks);
        let slug = target.slug_in(state_dir, &all_tasks)?;
        let mut task = load_task(state_dir, &slug)?;

        if let Some(title) = edit.title {
            task.title = title.to_string();
        }

        if let Some(parent_input) = edit.parent {
            let parent_slug = resolve(parent_input)?;
            if parent_slug == slug {
                anyhow::bail!("task '{slug}' cannot be its own parent");
            }
            let parent_task = all_tasks.as_slice().iter().find(|t| t.slug == parent_slug);
            ownership::ensure_parent_in_session(
                parent_task,
                &parent_slug,
                task.session.as_deref(),
            )?;
            reject_cycle(state_dir, &slug, &parent_slug)?;
            task.parent = Some(parent_slug);
        } else if edit.no_parent {
            task.parent = None;
        }

        for on in edit.block_on {
            let on_slug = resolve(on)?;
            if on_slug == slug {
                anyhow::bail!("task '{slug}' cannot be blocked on itself");
            }
            if !task.blocked_on.contains(&on_slug) {
                task.blocked_on.push(on_slug);
            }
        }

        for on in edit.unblock {
            let on_slug = resolve(on)?;
            task.blocked_on.retain(|b| b != &on_slug);
        }

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
