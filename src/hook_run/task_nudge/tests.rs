use super::*;
use crate::hook_run::shell_split::{
    MAX_SUBSTITUTION_DEPTH, NestingTooDeep, split_substitutions, take_until_close,
};
use crate::task::session::{StartDecision, StartOutcome, start_session};
use crate::task::{NewTask, ParentSpec, SessionChoice, add_task_with, done_task, start_task};
use proptest::prelude::*;
use tempfile::TempDir;
mod commit_deny;
mod fail_open;
mod foreign_session;
mod nudges;
mod parsing;
mod same_command;

fn project() -> String {
    crate::task::project::current_tag().unwrap()
}

fn open_session(dir: &Path) -> String {
    let StartOutcome::Created(s) =
        start_session(dir, Some("work"), None, &project(), StartDecision::Auto).unwrap()
    else {
        panic!("expected Created");
    };
    s.id
}

fn add(dir: &Path, session: &str, title: &str) -> String {
    let new = NewTask {
        title,
        ..NewTask::default()
    };
    add_task_with(
        dir,
        &new,
        ParentSpec::Detached,
        SessionChoice::Named(session),
        &project(),
    )
    .unwrap()
    .slug
}

fn bash(command: &str) -> serde_json::Value {
    serde_json::json!({ "tool_name": "Bash", "tool_input": { "command": command } })
}
