//! The `llmenv task add` command: picks the session that takes the new task, and records an
//! `--other-session` override on that task (#3486).

use std::path::Path;

use crate::task::session::EngineIdentity;
use crate::task::{NewTask, ParentSpec, Placement, SessionChoice, add_task_with};

use super::current_project_tag;

/// The flags of `task add`, after the detail flags are resolved.
pub(super) struct AddArgs<'a> {
    pub(super) title: &'a str,
    pub(super) parent: Option<&'a str>,
    pub(super) no_parent: bool,
    pub(super) child_of: Option<&'a str>,
    pub(super) parallel: bool,
    pub(super) after: Option<&'a str>,
    pub(super) session: Option<&'a str>,
    pub(super) other_session: Option<&'a str>,
    pub(super) detail: Option<&'a str>,
}

/// Adds the task and prints its slug. An `--other-session` override is recorded as a note on the
/// new task, because the session-level check leaves no record of its own.
///
/// # Errors
/// Errors if the caller identity cannot be read, if the session check refuses the session, or if
/// the task cannot be saved.
pub(super) fn run(state_dir: &Path, args: &AddArgs<'_>) -> anyhow::Result<()> {
    if args.no_parent {
        eprintln!("llmenv: --no-parent has no effect since v3.12.0; a new task no longer chains");
    }
    let parent_spec = match args.parent {
        Some(p) => ParentSpec::Explicit(p),
        None => ParentSpec::Detached,
    };
    let placement = match (args.child_of, args.parallel) {
        (Some(p), _) => Placement::Child(p),
        (None, true) => Placement::Parallel,
        (None, false) => Placement::Queue,
    };
    let project = current_project_tag()?;
    let owner = EngineIdentity::from_env()?;
    let choice = match args.session {
        Some(id) => SessionChoice::Named {
            id,
            owner: &owner,
            other_session: args.other_session,
        },
        None => SessionChoice::Resolve(&owner),
    };
    if args.detail.is_some_and(str::is_empty) {
        eprintln!("llmenv: the task detail is empty, so none was saved");
    }
    let new = NewTask {
        title: args.title,
        detail: args.detail,
        placement,
        after: args.after,
        note: None,
    };
    let task = add_task_with(state_dir, &new, parent_spec, choice, &project)?;
    println!("Added task '{}' ({})", task.slug, task.title);
    Ok(())
}
