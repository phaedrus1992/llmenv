//! The commands of `llmenv task <subcommand>`, one function each (#3595). Each one is a thin
//! formatting layer over `crate::task`, which owns the store logic.

use std::path::Path;

use super::task_guard::{guard_task, guard_tasks, record_override};
use super::{
    ColorMode, ShowTarget, TaskCommand, caller_session_for_task, run_task_session_command,
    run_task_show_current_or_next, task_add, task_clear, task_done, task_ls,
};
use crate::task::ownership::{Caller, OverrideNote};
use crate::task::{self, Task, with_store_lock};

/// Run one `llmenv task` subcommand. The commands pass through three dispatchers, so no function
/// matches more than a few of them.
pub(super) fn run(state_dir: &Path, command: TaskCommand, color: ColorMode) -> anyhow::Result<()> {
    match command {
        c @ TaskCommand::Add { .. } => add(state_dir, c),
        c @ TaskCommand::Start { .. } => start(state_dir, c),
        TaskCommand::Done {
            id,
            force,
            other_session,
        } => task_done::run(state_dir, &id, force, other_session.as_deref()),
        c @ TaskCommand::Reopen { .. } => reopen(state_dir, c),
        rest => run_reads_and_notes(state_dir, rest, color),
    }
}

fn run_reads_and_notes(
    state_dir: &Path,
    command: TaskCommand,
    color: ColorMode,
) -> anyhow::Result<()> {
    match command {
        c @ TaskCommand::Ls { .. } => task_ls::run(state_dir, c, color),
        TaskCommand::Show { id, current, next } => show(state_dir, id.as_deref(), current, next),
        c @ TaskCommand::Note { .. } => note(state_dir, c),
        c @ TaskCommand::Wait { .. } => wait(state_dir, c),
        rest => run_edits(state_dir, rest, color),
    }
}

fn run_edits(state_dir: &Path, command: TaskCommand, color: ColorMode) -> anyhow::Result<()> {
    match command {
        TaskCommand::Block {
            id,
            on,
            other_session,
        } => block(state_dir, &id, &on, other_session.as_deref()),
        c @ TaskCommand::Edit { .. } => edit(state_dir, c),
        TaskCommand::Clear {
            ids,
            session,
            other_session,
        } => with_store_lock(state_dir, || {
            task_clear::run(
                state_dir,
                &ids,
                session.as_deref(),
                other_session.as_deref(),
            )
        }),
        TaskCommand::Session { command } => run_task_session_command(state_dir, command, color),
        // The two earlier dispatchers take these commands. The list has no wildcard, so a new
        // command is a compile error here until a dispatcher handles it.
        TaskCommand::Add { .. }
        | TaskCommand::Start { .. }
        | TaskCommand::Done { .. }
        | TaskCommand::Reopen { .. }
        | TaskCommand::Ls { .. }
        | TaskCommand::Show { .. }
        | TaskCommand::Note { .. }
        | TaskCommand::Wait { .. } => Err(misrouted("command")),
    }
}

/// The error for a command that reached a handler of another command.
fn misrouted(name: &str) -> anyhow::Error {
    anyhow::anyhow!(
        "this is a bug in llmenv: `task {name}` was routed to the wrong handler. Fix: report it \
         with the command line"
    )
}

/// The text of an argument, or all of stdin when the argument is absent. The read comes before
/// the store lock, so a slow writer does not hold the store.
fn text_or_stdin(text: Option<String>) -> anyhow::Result<String> {
    match text {
        Some(text) => Ok(text),
        None => {
            use std::io::Read;
            let mut buf = String::new();
            std::io::stdin().read_to_string(&mut buf)?;
            Ok(buf.trim().to_string())
        }
    }
}

fn add(state_dir: &Path, command: TaskCommand) -> anyhow::Result<()> {
    let TaskCommand::Add {
        title,
        parent,
        no_parent,
        child_of,
        parallel,
        after,
        session,
        other_session,
        detail,
    } = command
    else {
        return Err(misrouted("add"));
    };
    let detail = detail.resolve()?;
    let args = task_add::AddArgs {
        title: &title,
        parent: parent.as_deref(),
        no_parent,
        child_of: child_of.as_deref(),
        parallel,
        after: after.as_deref(),
        session: session.as_deref(),
        other_session: other_session.as_deref(),
        detail: detail.as_deref(),
    };
    with_store_lock(state_dir, || task_add::run(state_dir, &args))
}

fn start(state_dir: &Path, command: TaskCommand) -> anyhow::Result<()> {
    let TaskCommand::Start {
        id,
        force,
        reopen,
        other_session,
    } = command
    else {
        return Err(misrouted("start"));
    };
    with_store_lock(state_dir, || {
        start_guarded(state_dir, &id, (force, reopen), other_session.as_deref())
    })
}

/// The start of one task, under the store lock. `flags` holds `--force` and `--reopen`.
fn start_guarded(
    state_dir: &Path,
    id: &str,
    flags: (bool, bool),
    other_session: Option<&str>,
) -> anyhow::Result<()> {
    let (force, reopen) = flags;
    let mut guarded = guard_task(state_dir, id, other_session)?;
    let audit = guarded.audit.take();
    if reopen {
        task::reopen_task(state_dir, guarded.target())?;
    }
    // The reopen writes, so the start reads the store again.
    let task = match task::start_task(state_dir, guarded.target(), force) {
        Ok(task) => task,
        Err(e) if reopen => return Err(failed_after_reopen(state_dir, id, e, audit)),
        Err(e) => return Err(e),
    };
    // The audit note comes first: a closed stdout makes `println!` panic.
    record_override(state_dir, audit)?;
    println!("Started '{}' — now {:?}", task.slug, task.state);
    Ok(())
}

/// The error of a start that failed after its reopen was saved. The audit note of the reopen is
/// written before the error, and a failed note is added to the error.
fn failed_after_reopen(
    state_dir: &Path,
    id: &str,
    error: anyhow::Error,
    audit: Option<OverrideNote>,
) -> anyhow::Error {
    let error = error.context(format!(
        "task '{id}' is now reopened (open), but it did not start"
    ));
    match record_override(state_dir, audit) {
        Ok(()) => error,
        Err(note) => error.context(format!("its audit note was not saved: {note:#}")),
    }
}

fn reopen(state_dir: &Path, command: TaskCommand) -> anyhow::Result<()> {
    let TaskCommand::Reopen { ids, other_session } = command else {
        return Err(misrouted("reopen"));
    };
    with_store_lock(state_dir, || {
        reopen_guarded(state_dir, &ids, other_session.as_deref())
    })
}

fn reopen_guarded(
    state_dir: &Path,
    ids: &[String],
    other_session: Option<&str>,
) -> anyhow::Result<()> {
    let caller = caller_session_for_task(state_dir)?;
    reopen_as(state_dir, &caller, ids, other_session)
}

/// The reopen of `ids` for a caller that is already known.
fn reopen_as(
    state_dir: &Path,
    caller: &Caller,
    ids: &[String],
    other_session: Option<&str>,
) -> anyhow::Result<()> {
    let guarded = guard_tasks(state_dir, caller, ids, other_session)?;
    let (slugs, audits): (Vec<String>, Vec<_>) = guarded.items.into_iter().unzip();
    let result = task::reopen::reopen_resolved(state_dir, &guarded.tasks, &slugs);
    let failed = record_reopen_notes(state_dir, &slugs, audits, &result);
    let reopened = match result {
        Ok(tasks) => tasks,
        Err(e) => return Err(failed.into_iter().fold(e, |e, note| e.context(note))),
    };
    for task in reopened {
        println!("Reopened '{}' — now {:?}", task.slug, task.state);
    }
    if failed.is_empty() {
        Ok(())
    } else {
        // The reopens are saved, so the status says a note failed and not the change (#3765).
        Err(task::note_not_saved::mark(&anyhow::anyhow!(
            "{}",
            failed.join("\n")
        )))
    }
}

/// Record the audit note of each task that was reopened. A failed save leaves the earlier tasks
/// reopened, so their notes are still recorded (#3749). Every note is tried, so one failed note
/// does not hide the others. The notes come before the print: a closed stdout makes `println!`
/// panic. The result lists the failed notes.
fn record_reopen_notes(
    state_dir: &Path,
    slugs: &[String],
    audits: Vec<Option<OverrideNote>>,
    result: &anyhow::Result<Vec<Task>>,
) -> Vec<String> {
    let saved: &[Task] = match result {
        Ok(tasks) => tasks,
        Err(e) => task::partial::changed_before_failure(e),
    };
    slugs
        .iter()
        .zip(audits)
        .filter(|(slug, _)| saved.iter().any(|t| &t.slug == *slug))
        .filter_map(|(_, audit)| record_override(state_dir, audit).err())
        .map(|e| format!("{e:#}"))
        .collect()
}

fn show(state_dir: &Path, id: Option<&str>, current: bool, next: bool) -> anyhow::Result<()> {
    match (current, next) {
        (true, _) => run_task_show_current_or_next(state_dir, ShowTarget::Current),
        (_, true) => run_task_show_current_or_next(state_dir, ShowTarget::Next),
        (false, false) => {
            let id = id.ok_or_else(|| anyhow::anyhow!("provide a task id, or --current/--next"))?;
            let slug = task::resolve_identifier(state_dir, id)?;
            let task = task::load_task(state_dir, &slug)?;
            println!("{}", serde_json::to_string_pretty(&task)?);
            Ok(())
        }
    }
}

fn note(state_dir: &Path, command: TaskCommand) -> anyhow::Result<()> {
    let TaskCommand::Note {
        id,
        text,
        other_session,
    } = command
    else {
        return Err(misrouted("note"));
    };
    let text = text_or_stdin(text)?;
    with_store_lock(state_dir, || {
        let guarded = guard_task(state_dir, &id, other_session.as_deref())?;
        let task = task::note_task(state_dir, guarded.target(), &text)?;
        // The audit note comes first: a closed stdout makes `println!` panic.
        record_override(state_dir, guarded.audit)?;
        println!("Noted on '{}'", task.slug);
        Ok(())
    })
}

fn wait(state_dir: &Path, command: TaskCommand) -> anyhow::Result<()> {
    let TaskCommand::Wait {
        id,
        reason,
        other_session,
    } = command
    else {
        return Err(misrouted("wait"));
    };
    let reason = text_or_stdin(reason)?;
    with_store_lock(state_dir, || {
        let guarded = guard_task(state_dir, &id, other_session.as_deref())?;
        let task = task::wait_task(state_dir, guarded.target(), &reason)?;
        // The audit note comes first: a closed stdout makes `println!` panic.
        record_override(state_dir, guarded.audit)?;
        println!("Marked '{}' waiting", task.slug);
        Ok(())
    })
}

fn block(state_dir: &Path, id: &str, on: &str, other_session: Option<&str>) -> anyhow::Result<()> {
    with_store_lock(state_dir, || {
        let guarded = guard_task(state_dir, id, other_session)?;
        let task = task::block_task(state_dir, guarded.target(), on)?;
        // The audit note comes first: a closed stdout makes `println!` panic.
        record_override(state_dir, guarded.audit)?;
        println!(
            "'{}' is now blocked on: {}",
            task.slug,
            task.blocked_on.join(", ")
        );
        Ok(())
    })
}

fn edit(state_dir: &Path, command: TaskCommand) -> anyhow::Result<()> {
    let TaskCommand::Edit {
        id,
        title,
        parent,
        no_parent,
        block_on,
        unblock,
        add_note,
        delete_note,
        detail,
        other_session,
    } = command
    else {
        return Err(misrouted("edit"));
    };
    let detail = detail.resolve()?;
    // An empty `--add-note` reads the note from stdin, before the lock.
    let add_note = match add_note {
        Some(note) if note.is_empty() => Some(text_or_stdin(None)?),
        other => other,
    };
    let edit = task::TaskEdit {
        title: title.as_deref(),
        parent: parent.as_deref(),
        no_parent,
        block_on: &block_on,
        unblock: &unblock,
        add_note: add_note.as_deref(),
        delete_note: delete_note.as_deref(),
        detail: detail.as_deref(),
    };
    with_store_lock(state_dir, || {
        let guarded = guard_task(state_dir, &id, other_session.as_deref())?;
        let task = task::edit_task(state_dir, guarded.target(), &edit)?;
        // The audit note comes first: a closed stdout makes `println!` panic.
        record_override(state_dir, guarded.audit)?;
        println!("Updated '{}'", task.slug);
        Ok(())
    })
}

#[cfg(test)]
#[expect(clippy::unwrap_used, clippy::panic, reason = "test scaffolding")]
mod tests {
    use super::*;
    use crate::task::session::{EngineIdentity, StartDecision, StartOutcome, start_session};
    use crate::task::{ParentSpec, SessionChoice, TaskState, add_task, complete_task, load_task};

    const PROJECT: &str = "task-run-test";

    /// A session with one task, and the caller that owns the session.
    fn session_with_task(dir: &Path, title: &str) -> (String, Caller) {
        let outcome = start_session(dir, Some("a"), None, PROJECT, StartDecision::Auto).unwrap();
        let StartOutcome::Created(session) = outcome else {
            panic!("expected a new session");
        };
        let owner = EngineIdentity::from_session_id(None);
        let choice = SessionChoice::Named {
            id: &session.id,
            owner: &owner,
            other_session: None,
        };
        let task = add_task(dir, title, ParentSpec::Detached, choice, PROJECT).unwrap();
        (task.slug, Caller::Session(session.id))
    }

    #[test]
    fn reopen_moves_a_done_task_back_to_open() {
        let dir = tempfile::tempdir().unwrap();
        let (slug, caller) = session_with_task(dir.path(), "Reopen me");
        crate::task::start_task(dir.path(), slug.as_str(), false).unwrap();
        complete_task(dir.path(), slug.as_str(), false).unwrap();
        assert_eq!(load_task(dir.path(), &slug).unwrap().state, TaskState::Done);

        reopen_as(dir.path(), &caller, std::slice::from_ref(&slug), None).unwrap();

        assert_eq!(load_task(dir.path(), &slug).unwrap().state, TaskState::Open);
    }

    #[test]
    fn reopen_refuses_a_task_of_another_session_with_no_override() {
        let dir = tempfile::tempdir().unwrap();
        let (slug, _) = session_with_task(dir.path(), "Not yours");
        let stranger = Caller::Session("someone-else".to_string());
        let err = reopen_as(dir.path(), &stranger, std::slice::from_ref(&slug), None).unwrap_err();
        assert!(format!("{err:#}").contains("belongs to session"), "{err:#}");
    }

    #[test]
    fn the_notes_of_a_reopen_are_listed_for_the_saved_tasks_only() {
        let dir = tempfile::tempdir().unwrap();
        let slugs = vec!["a".to_string(), "b".to_string()];
        let saved: anyhow::Result<Vec<Task>> = Ok(Vec::new());
        // No task was saved, so no note is tried and none fails.
        assert!(record_reopen_notes(dir.path(), &slugs, vec![None, None], &saved).is_empty());
    }
}
