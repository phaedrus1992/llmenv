//! The creation of one task in a session whose id is known (#3595).

use std::path::Path;

use super::{
    NewTask, ParentSpec, Placement, Relation, StrictTasks, Task, TaskNote, TaskState,
    list_tasks_strict, load_task, now_rfc3339, ownership, resolve, save_task, slugify, tasks_dir,
    unique_slug, with_store_lock,
};

/// [`super::add_task_for_session`] for a task that starts with a detail (#2339).
///
/// # Errors
/// Errors if `parent` is [`ParentSpec::Explicit`] and doesn't resolve to an existing task — same
/// eager-validation reasoning as `block_task`'s `on`.
pub(super) fn add_task_for_session_with(
    state_dir: &Path,
    new: &NewTask<'_>,
    parent: ParentSpec<'_>,
    session_id: &str,
) -> anyhow::Result<Task> {
    with_store_lock(state_dir, || {
        // One strict read resolves every name that the new task refers to (#3477). Each name is
        // checked first, so a bad name gets its own error even when the store is unreadable.
        let names = [
            match parent {
                ParentSpec::Explicit(p) => Some(p),
                ParentSpec::Detached => None,
            },
            match new.placement {
                Placement::Child(p) => Some(p),
                Placement::Queue | Placement::Parallel => None,
            },
            new.after,
        ];
        names
            .iter()
            .flatten()
            .try_for_each(|name| resolve::check_identifier(name))?;
        let ctx = NameContext {
            state_dir,
            all_tasks: names
                .iter()
                .any(Option::is_some)
                .then(|| list_tasks_strict(state_dir))
                .transpose()?,
        };
        let (parent_slug, relation, parallel) = place(&ctx, parent, new.placement, session_id)?;
        let blocked_on = match new.after {
            Some(after) => vec![ctx.resolve(after)?],
            None => Vec::new(),
        };
        let now = now_rfc3339();
        let slug = unique_slug(&tasks_dir(state_dir), &base_slug(new.title, &now));
        let task = Task {
            slug,
            title: new.title.to_string(),
            state: TaskState::Open,
            parent: parent_slug,
            relation,
            parallel,
            blocked_on,
            notes: new
                .note
                .map(|text| TaskNote {
                    at: now.clone(),
                    text: text.to_string(),
                })
                .into_iter()
                .collect(),
            detail: new.detail.filter(|d| !d.is_empty()).map(str::to_string),
            session: Some(session_id.to_string()),
            created_at: now.clone(),
            updated_at: now,
        };
        save_task(state_dir, &task)?;
        Ok(task)
    })
}

/// The store and the strict read of one creation. The read is `None` when the new task names no
/// other task.
struct NameContext<'a> {
    state_dir: &'a Path,
    all_tasks: Option<StrictTasks>,
}

impl NameContext<'_> {
    fn resolve(&self, name: &str) -> anyhow::Result<String> {
        let all = self
            .all_tasks
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("the task store was not read to find '{name}'"))?;
        resolve::resolve_in(self.state_dir, name, all)
    }
}

/// The parent, the relation, and the `parallel` flag of the new task.
fn place(
    ctx: &NameContext<'_>,
    parent: ParentSpec<'_>,
    placement: Placement<'_>,
    session_id: &str,
) -> anyhow::Result<(Option<String>, Relation, bool)> {
    let explicit_parent = match parent {
        ParentSpec::Explicit(p) => {
            let slug = ctx.resolve(p)?;
            let parent_task = ctx
                .all_tasks
                .as_ref()
                .and_then(|all| all.as_slice().iter().find(|t| t.slug == slug));
            ownership::ensure_parent_in_session(parent_task, &slug, Some(session_id))?;
            Some(slug)
        }
        ParentSpec::Detached => None,
    };
    Ok(match placement {
        Placement::Queue => (explicit_parent, Relation::Queued, false),
        Placement::Parallel => (explicit_parent, Relation::Queued, true),
        Placement::Child(p) => {
            let slug = ctx.resolve(p)?;
            ownership::check_child_parent(&load_task(ctx.state_dir, &slug)?, session_id)?;
            (Some(slug), Relation::Child, false)
        }
    })
}

/// The slug that a title gives before the number that makes it unique. A title with no
/// ASCII-alphanumeric character (a CJK-only title, or pure punctuation) would give an empty
/// slug, which is a hidden `.json` file that is awkward to reference, so the slug comes from the
/// time then.
fn base_slug(title: &str, now: &str) -> String {
    let slug = slugify(title);
    if slug.is_empty() {
        format!("task-{}", now.replace([':', '-'], ""))
    } else {
        slug
    }
}
