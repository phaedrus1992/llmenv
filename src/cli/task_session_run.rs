//! Runs `llmenv task session <subcommand>`. One function handles each subcommand, so each one
//! stays under the complexity limit.

use super::{
    ColorMode, ResumeArgs, TaskListFormat, TaskSessionCommand, add_session_tasks,
    current_project_tag, guard_session_for_write, render_task_session_summary_human,
    resolve_session_for_read, should_use_color, start_outcome_lacks_context,
};
use crate::task::session::{self, EngineIdentity, StartDecision, StartOutcome};
use std::path::Path;

/// The fields of `session start`. A struct keeps the start handler within the parameter limit.
struct StartArgs {
    name: Option<String>,
    description: Option<String>,
    resume: Option<String>,
    replace: bool,
    other_session: Vec<String>,
    new: bool,
    tasks: Vec<String>,
    resume_context: ResumeArgs,
}

/// Handle `llmenv task session <subcommand>` (#905, reworked for mandatory
/// sessions). Thin formatting layer over `crate::task::session`.
pub(super) fn run_task_session_command(
    state_dir: &Path,
    command: TaskSessionCommand,
    color: ColorMode,
) -> anyhow::Result<()> {
    let project = current_project_tag()?;
    match command {
        TaskSessionCommand::Start {
            name,
            description,
            resume,
            replace,
            other_session,
            new,
            tasks,
            resume_context,
        } => start_cmd(
            state_dir,
            &project,
            StartArgs {
                name,
                description,
                resume,
                replace,
                other_session,
                new,
                tasks,
                resume_context,
            },
        ),
        TaskSessionCommand::Edit {
            id,
            resume_context,
            other_session,
        } => edit_cmd(
            state_dir,
            &project,
            id,
            resume_context,
            other_session.as_deref(),
        ),
        TaskSessionCommand::Note {
            text,
            id,
            other_session,
        } => note_cmd(state_dir, &project, text, id, other_session.as_deref()),
        TaskSessionCommand::Finish {
            id,
            abandon_open,
            other_session,
        } => finish_cmd(
            state_dir,
            &project,
            id,
            abandon_open,
            other_session.as_deref(),
        ),
        TaskSessionCommand::Show { id } => show_cmd(state_dir, &project, id),
        TaskSessionCommand::Summary { id, format } => {
            summary_cmd(state_dir, &project, id, format, color)
        }
        TaskSessionCommand::Ls => ls_cmd(state_dir, &project),
    }
}

/// The session decision that the `--resume`, `--replace`, and `--new` flags name.
fn start_decision(
    resume: Option<String>,
    replace: bool,
    new: bool,
    other_session: Vec<String>,
) -> StartDecision {
    match (resume, replace, new) {
        (Some(id), false, false) => StartDecision::Resume(id),
        (None, true, false) => StartDecision::Replace(other_session),
        (None, false, true) => StartDecision::New,
        (None, false, false) => StartDecision::Auto,
        _ => unreachable!("clap's conflicts_with_all enforces at most one"),
    }
}

fn start_cmd(state_dir: &Path, project: &str, args: StartArgs) -> anyhow::Result<()> {
    let StartArgs {
        name,
        description,
        resume,
        replace,
        other_session,
        new,
        tasks,
        resume_context,
    } = args;
    let explicit = resume_context.into_context()?;
    let mut detected = explicit.clone();
    let branch = crate::task::resume::git_branch(&std::env::current_dir()?)?;
    detected.fill_detected(branch.as_deref());
    let is_resume = resume.is_some();
    let decision = start_decision(resume, replace, new, other_session);
    let auto = matches!(decision, StartDecision::Auto);
    let owner = EngineIdentity::from_env()?;
    let request = session::StartRequest {
        name: name.as_deref(),
        description: description.as_deref(),
        project,
        owner: &owner,
        // A resumed session keeps what it has: only explicit flags change it.
        resume: if is_resume { &explicit } else { &detected },
    };
    let outcome = session::start_session_as(state_dir, &request, decision)?;
    let nudge = start_outcome_lacks_context(&outcome);
    let session_id = match &outcome {
        StartOutcome::Created(s) | StartOutcome::Resumed(s) => s.id.clone(),
        StartOutcome::Replaced { session, .. } => session.id.clone(),
    };
    print_start_outcome(&outcome, auto);
    if nudge {
        println!("{}", crate::task::resume::MISSING_CONTEXT_NUDGE);
    }
    add_tasks_to_session(state_dir, project, &session_id, &owner, &tasks)?;
    if tasks.is_empty() {
        // Known steps added now let the idle reminders catch a step that never starts (#2987).
        println!(
            "Add every known step now with `llmenv task add <title>`, and add `--session <id>` when more than one session is open."
        );
    }
    Ok(())
}

/// Print what `session start` did. `auto` is true when no flag chose the session.
fn print_start_outcome(outcome: &StartOutcome, auto: bool) {
    match outcome {
        StartOutcome::Created(s) => println!(
            "Started session '{}'{}",
            s.id,
            s.name
                .as_deref()
                .map(|n| format!(" ({n})"))
                .unwrap_or_default()
        ),
        StartOutcome::Resumed(s) if auto => println!(
            "Resumed session '{}': it is the one open session of this project that this \
             conversation or engine process started. Pass --new for a second session.",
            s.id
        ),
        StartOutcome::Resumed(s) => println!("Resumed session '{}'", s.id),
        StartOutcome::Replaced { session, abandoned } => {
            for a in abandoned {
                println!(
                    "Abandoned session '{}' — its incomplete tasks were untagged and noted as orphaned.",
                    a.id
                );
            }
            println!("Started session '{}'", session.id);
        }
    }
}

/// Add each title in `tasks` to the new session. Every added task is printed.
fn add_tasks_to_session(
    state_dir: &Path,
    project: &str,
    session_id: &str,
    owner: &EngineIdentity,
    tasks: &[String],
) -> anyhow::Result<()> {
    add_session_tasks(session_id, tasks, |title| {
        let new = crate::task::NewTask {
            title,
            ..crate::task::NewTask::default()
        };
        let task = crate::task::add_task_with(
            state_dir,
            &new,
            crate::task::ParentSpec::Detached,
            crate::task::SessionChoice::Named {
                id: session_id,
                owner,
                other_session: None,
            },
            project,
        )?;
        println!("Added task '{}' ({})", task.slug, task.title);
        Ok(task)
    })
}

fn edit_cmd(
    state_dir: &Path,
    project: &str,
    id: Option<String>,
    resume_context: ResumeArgs,
    other_session: Option<&str>,
) -> anyhow::Result<()> {
    let update = resume_context.into_context()?;
    let session = crate::task::with_store_lock(state_dir, || {
        let (id, audit) = guard_session_for_write(state_dir, project, id, other_session)?;
        let session = session::update_resume(state_dir, &id, |r| r.apply(&update))?;
        audit.record(state_dir)?;
        Ok(session)
    })?;
    println!("Updated resume context for session '{}'", session.id);
    Ok(())
}

fn note_cmd(
    state_dir: &Path,
    project: &str,
    text: Option<String>,
    id: Option<String>,
    other_session: Option<&str>,
) -> anyhow::Result<()> {
    let text = match text {
        Some(text) => text,
        None => std::io::read_to_string(std::io::stdin())?,
    };
    let text = text.trim_end();
    anyhow::ensure!(
        !text.is_empty(),
        "the note is empty: pass text or pipe it on stdin"
    );
    let session = crate::task::with_store_lock(state_dir, || {
        let (id, audit) = guard_session_for_write(state_dir, project, id, other_session)?;
        let session = session::update_resume(state_dir, &id, |r| r.append_note(text))?;
        audit.record(state_dir)?;
        Ok(session)
    })?;
    println!("Noted session '{}'", session.id);
    Ok(())
}

fn finish_cmd(
    state_dir: &Path,
    project: &str,
    id: Option<String>,
    abandon_open: bool,
    other_session: Option<&str>,
) -> anyhow::Result<()> {
    let outcome = crate::task::with_store_lock(state_dir, || {
        let (id, audit) = guard_session_for_write(state_dir, project, id, other_session)?;
        let outcome = match session::finish_session(state_dir, &id, abandon_open) {
            Ok(outcome) => outcome,
            Err(e) => return Err(audit.record_after_failure(state_dir, e)),
        };
        audit.record(state_dir)?;
        Ok(outcome)
    })?;
    let (done, total) = (outcome.done, outcome.total);
    let id = &outcome.session.id;
    if outcome.abandoned.is_empty() {
        println!("Finished session '{id}' ({done}/{total} done)");
    } else {
        let n = outcome.abandoned.len();
        println!("Finished session '{id}' ({done}/{total} done, {n} abandoned)");
        for task in &outcome.abandoned {
            println!(
                "  abandoned {} {} {}",
                task.state.as_str(),
                task.slug,
                task.title
            );
        }
    }
    Ok(())
}

fn show_cmd(state_dir: &Path, project: &str, id: Option<String>) -> anyhow::Result<()> {
    let id = resolve_session_for_read(state_dir, project, id)?;
    // Validate the session exists rather than printing "0/0 done" for
    // a typo'd id (session_progress returns (0, 0) for any unknown id).
    // A corrupt session file is reported with its path, not as a missing id (#2628).
    let session = session::load_existing_session(state_dir, &id)?;
    let (done, total) = session::try_session_progress(state_dir, &session.id)?;
    let state = if session.is_open() {
        "open"
    } else if session.finished_at.is_some() {
        "finished"
    } else {
        "abandoned"
    };
    println!(
        "Session '{}'{}: {done}/{total} done ({state})",
        session.id,
        session
            .name
            .as_deref()
            .map(|n| format!(" ({n})"))
            .unwrap_or_default(),
    );
    let resume = session.resume.render();
    if !resume.is_empty() {
        println!("{resume}");
    }
    Ok(())
}

fn summary_cmd(
    state_dir: &Path,
    project: &str,
    id: Option<String>,
    format: Option<TaskListFormat>,
    color: ColorMode,
) -> anyhow::Result<()> {
    let id = resolve_session_for_read(state_dir, project, id)?;
    let summary = session::session_summary(state_dir, &id)?;
    match format {
        Some(TaskListFormat::Json) => {
            println!("{}", serde_json::to_string_pretty(&summary)?);
        }
        None => {
            use std::io::IsTerminal;
            let use_color = should_use_color(Some(color), std::io::stdout().is_terminal());
            print!("{}", render_task_session_summary_human(&summary, use_color));
        }
    }
    Ok(())
}

fn ls_cmd(state_dir: &Path, project: &str) -> anyhow::Result<()> {
    let mut sessions: Vec<_> = session::list_sessions_strict(state_dir)?
        .into_iter()
        .filter(session::Session::is_open)
        .collect();
    session::sort_listing(&mut sessions, project);
    if sessions.is_empty() {
        println!("No open sessions.");
    }
    for s in &sessions {
        println!(
            "{}\t{}\t{}\t{}\tidle {}",
            s.id,
            s.name.as_deref().unwrap_or("-"),
            s.project,
            s.description.as_deref().unwrap_or("-"),
            session::idle_display(&s.last_activity, &s.id),
        );
    }
    Ok(())
}
