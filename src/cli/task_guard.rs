//! The ownership guard of the `llmenv task` commands: a task or a session of another
//! conversation is refused unless `--other-session` names its owner.

use super::caller_session_for_task;
use crate::task::ownership::{Caller, OverrideNote, SessionOverride};
use crate::task::{StrictTasks, TaskTarget};

/// A task that passed the ownership check, with the strict read of the store that the check made.
/// The command acts on [`Guarded::target`], so it does not read the store again (#3485).
pub(super) struct Guarded {
    tasks: StrictTasks,
    slug: String,
    pub(super) audit: Option<OverrideNote>,
}

impl Guarded {
    /// The checked task, with the read of the check.
    pub(super) fn target(&self) -> TaskTarget<'_> {
        TaskTarget::Checked(&self.slug, &self.tasks)
    }
}

/// Several tasks that passed the ownership check, with the one strict read that checked them.
pub(super) struct GuardedMany {
    pub(super) tasks: StrictTasks,
    pub(super) items: Vec<(String, Option<OverrideNote>)>,
}

/// Refuse a command that acts on task `id` when the task belongs to another session. The check
/// and the change run in one `with_store_lock`, so no other process changes the task between
/// the two (#3478).
pub(super) fn guard_task(
    state_dir: &std::path::Path,
    id: &str,
    other_session: Option<&str>,
) -> anyhow::Result<Guarded> {
    let caller = caller_session_for_task(state_dir)?;
    let tasks = crate::task::ownership::read_for_check(state_dir, &[id])?;
    let (slug, audit) =
        crate::task::ownership::ensure_task_is_ours(state_dir, &tasks, id, &caller, other_session)?;
    Ok(Guarded { tasks, slug, audit })
}

/// [`guard_task`] for several ids, with one read of the store. The command then acts on the
/// returned slugs, which are the tasks that were checked (#3477). `caller` is the caller that
/// the command already read.
pub(super) fn guard_tasks(
    state_dir: &std::path::Path,
    caller: &Caller,
    ids: &[String],
    other_session: Option<&str>,
) -> anyhow::Result<GuardedMany> {
    let inputs: Vec<&str> = ids.iter().map(String::as_str).collect();
    let tasks = crate::task::ownership::read_for_check(state_dir, &inputs)?;
    let items = crate::task::ownership::ensure_tasks_are_ours(
        state_dir,
        &tasks,
        &inputs,
        caller,
        other_session,
    )?;
    Ok(GuardedMany { tasks, items })
}

/// Refuse `clear --session <id>` when that session is not the caller's. Returns the override
/// note when `--other-session` was used.
pub(super) fn guard_session(
    state_dir: &std::path::Path,
    id: &str,
    other_session: Option<&str>,
) -> anyhow::Result<SessionOverride> {
    let caller = caller_session_for_task(state_dir)?;
    crate::task::ownership::ensure_session_is_ours(id, &caller, other_session)
}

/// Appends the pending `--other-session` note, once the command has succeeded.
pub(super) fn record_override(
    state_dir: &std::path::Path,
    audit: Option<OverrideNote>,
) -> anyhow::Result<()> {
    audit.map_or(Ok(()), |note| note.record(state_dir))
}
