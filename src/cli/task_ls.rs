//! `llmenv task ls` (#3595).

use std::path::Path;

use super::{
    ColorMode, TaskCommand, TaskListFormat, current_project_tag, render_task_ls_human,
    should_use_color,
};
use crate::task::{self, Task, TaskState};

/// List the tasks of the store, filtered by the flags of the command.
pub(super) fn run(state_dir: &Path, command: TaskCommand, color: ColorMode) -> anyhow::Result<()> {
    let TaskCommand::Ls {
        format,
        session,
        all,
        state,
        hide_done,
        current_project,
    } = command
    else {
        return Err(anyhow::anyhow!(
            "this is a bug in llmenv: `task ls` was routed to the wrong handler. Fix: report it \
             with the command line"
        ));
    };
    // #1124: force a deliberate choice instead of silently listing every session's tasks.
    // --current-project alone doesn't count: it narrows by project, not by session.
    if session.is_none() && !all {
        anyhow::bail!(
            "llmenv task ls requires either --session <id> or --all \
             — pass --all to deliberately list every session's tasks"
        );
    }
    let (mut tasks, sessions) = ls_scoped(state_dir, session.as_deref(), current_project)?;
    let states: Vec<TaskState> = state.iter().copied().map(Into::into).collect();
    tasks = task::filter_by_state(tasks, &states);
    if hide_done {
        tasks.retain(|t| t.state != TaskState::Done);
    }
    ls_print(state_dir, tasks, sessions, format.as_ref(), color)
}

/// The tasks of the store that the project and session flags keep, with the session read of the
/// project filter. One strict read of the session files serves the filter and the renderer
/// (#3479).
fn ls_scoped(
    state_dir: &Path,
    session: Option<&str>,
    current_project: bool,
) -> anyhow::Result<(Vec<Task>, Option<Vec<task::session::Session>>)> {
    let mut tasks = task::try_list_tasks(state_dir)?;
    let mut sessions = None;
    if current_project {
        let project = current_project_tag()?;
        let read = task::session::list_sessions_strict(state_dir)?;
        tasks = task::filter_tasks_for_project(&read, &project, tasks);
        sessions = Some(read);
    }
    if let Some(session_id) = session {
        tasks.retain(|t| t.session.as_deref() == Some(session_id));
    }
    Ok((tasks, sessions))
}

fn ls_print(
    state_dir: &Path,
    mut tasks: Vec<Task>,
    sessions: Option<Vec<task::session::Session>>,
    format: Option<&TaskListFormat>,
    color: ColorMode,
) -> anyhow::Result<()> {
    match format {
        Some(TaskListFormat::Json) => {
            // Stable order even at second-precision timestamp ties, so scripts that parse the
            // JSON get deterministic output.
            tasks.sort_by(|a, b| {
                crate::util::time::cmp_instants(&a.created_at, &b.created_at, "task created_at")
                    .then_with(|| a.slug.cmp(&b.slug))
            });
            println!("{}", serde_json::to_string(&tasks)?);
        }
        None => {
            use std::io::IsTerminal;
            let use_color = should_use_color(Some(color), std::io::stdout().is_terminal());
            print!(
                "{}",
                render_task_ls_human(state_dir, sessions, tasks, use_color)?
            );
        }
    }
    Ok(())
}
