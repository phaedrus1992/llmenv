//! The parent link of a task: the check that a new parent does not form a loop.

use std::collections::HashSet;
use std::path::Path;

use super::{is_not_found, load_task};

/// Errs if setting `slug`'s parent to `new_parent` would make `slug` its own
/// ancestor — walks up from `new_parent` and checks `slug` never reappears. A
/// loop elsewhere in the chain, or a missing ancestor, fails the change with the
/// task name and the fix (#3591): the walk must not end silently. An unreadable
/// or corrupt ancestor fails the change for the same reason.
pub(super) fn reject_cycle(state_dir: &Path, slug: &str, new_parent: &str) -> anyhow::Result<()> {
    let mut current = new_parent.to_string();
    let mut visited = HashSet::new();
    loop {
        if current == slug {
            anyhow::bail!("setting parent to '{new_parent}' would make '{slug}' its own ancestor");
        }
        if !visited.insert(current.clone()) {
            anyhow::bail!(
                "cannot set the parent of '{slug}' to '{new_parent}': the ancestors loop at \
                 '{current}'. Fix: clear the parent of one task in that loop with \
                 `llmenv task edit <task> --no-parent`"
            );
        }
        let task = match load_task(state_dir, &current) {
            Ok(task) => task,
            Err(e) if is_not_found(&e) => anyhow::bail!(
                "cannot set the parent of '{slug}' to '{new_parent}': ancestor '{current}' is \
                 missing from the task store. Fix: clear or re-point the parent of the task that \
                 names '{current}'"
            ),
            Err(e) => {
                return Err(e.context(format!(
                    "cannot check parent '{new_parent}' of '{slug}': ancestor '{current}' is unreadable"
                )));
            }
        };
        let Some(parent) = task.parent else {
            return Ok(());
        };
        current = parent;
    }
}
