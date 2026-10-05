//! Task tracking nudges and the commit deny-once (#2456), engine-neutral.
//!
//! The task tracker only reminded the agent at SessionStart and Stop. These handlers fire while
//! the work happens: after a workflow skill starts, after several tool calls with no task, after
//! a question to the user, and before the first `git commit` or `gh pr create` with no task in
//! progress. Per-session counters live in `state_dir/task_nudge/{session_id}.json`.
//!
//! Fail-soft: an I/O error logs and passes the call through, and a deny needs its marker saved.
//! Load-modify-save has no lock, like `repeat_detect`. Parallel tool calls of one session can
//! lose an update, which costs one extra or one missed nudge and nothing more.
//! The tracker state is per project, not per session: a task in progress in any open session of
//! the project counts as tracked work.
//! Design: docs/design/issue-2438-task-tracking-nudges.md

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::config::TaskTracker;
use crate::task::Tracking;

/// Skills that start a multi-step workflow, when `workflow_skills` is not set.
const DEFAULT_WORKFLOW_SKILLS: [&str; 5] = [
    "dev-sprint",
    "ship-issue",
    "pre-pr-review",
    "executing-plans",
    "writing-plans",
];
/// Mutating tool calls with no task before the first nudge. A short task is two or three calls,
/// so eight means the work has more than one part.
const DEFAULT_NUDGE_AFTER: u32 = 8;
/// Calls between later nudges, so a long untracked run is reminded without nagging.
const DEFAULT_NUDGE_EVERY: u32 = 20;
/// Tools that change the project. `Bash` counts because it commits, builds, and edits.
const MUTATING_TOOLS: [&str; 4] = ["Bash", "Edit", "Write", "MultiEdit"];

#[derive(Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
struct NudgeState {
    /// Mutating calls seen while the work was untracked.
    #[serde(default)]
    calls: u32,
    /// The value of `calls` at the last nudge.
    #[serde(default)]
    last_nudge: u32,
    /// The skill reminder fired in this session.
    #[serde(default)]
    skill_reminded: bool,
    /// A commit or PR was denied once, and the retry is allowed.
    #[serde(default)]
    commit_denied: bool,
}

fn state_path(state_dir: &Path, session_id: &str) -> Option<PathBuf> {
    // A session id becomes a file name, so only a plain name is accepted.
    crate::paths::is_valid_short_name(session_id).then(|| {
        state_dir
            .join("task_nudge")
            .join(format!("{session_id}.json"))
    })
}

/// The state of a session. A missing file is a new session. `None` means the file exists but
/// cannot be read, and the caller skips the nudge. A corrupt file is reset.
fn load(path: &Path) -> Option<NudgeState> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Some(NudgeState::default()),
        Err(e) => {
            tracing::error!("task nudge state {} cannot be read: {e}", path.display());
            return None;
        }
    };
    Some(serde_json::from_str(&text).unwrap_or_else(|e| {
        tracing::error!(
            "task nudge state {} is unreadable, reset: {e}",
            path.display()
        );
        NudgeState::default()
    }))
}

/// Whether the state was saved. A caller that depends on the state, such as the deny marker,
/// must not act when it was not.
fn save(path: &Path, state: &NudgeState) -> bool {
    let written = path
        .parent()
        .map_or(Ok(()), crate::paths::create_dir_owner_only)
        .and_then(|()| {
            let json = serde_json::to_string(state)?;
            crate::paths::write_owner_only_atomic(path, json.as_bytes())
        });
    if let Err(e) = &written {
        tracing::error!("task nudge state {} cannot be saved: {e:#}", path.display());
    }
    written.is_ok()
}

/// The commands that register work, for the state of the project.
fn how_to_track(tracking: &Tracking) -> String {
    match tracking {
        Tracking::NoSession => "Start a session with its first tasks: `llmenv task session start \
             <name> --task \"<step 1>\" --task \"<step 2>\"`. Then run `llmenv task start <slug>` \
             for the step you do first."
            .to_string(),
        _ => "Add a task for each step: `llmenv task add \"<step>\"`. Then run `llmenv task start \
             <slug>` for the step you do first. Use `--child-of <slug>` for the parts of a step."
            .to_string(),
    }
}

/// The skill name without its plugin prefix: `nbl-dev:ship-issue` is `ship-issue`.
fn bare_skill(name: &str) -> &str {
    name.rsplit(':').next().unwrap_or(name)
}

fn is_workflow_skill(tracker: &TaskTracker, skill: &str) -> bool {
    let skill = bare_skill(skill.trim());
    match &tracker.workflow_skills {
        Some(list) => list.iter().any(|s| bare_skill(s.trim()) == skill),
        None => DEFAULT_WORKFLOW_SKILLS.contains(&skill),
    }
}

fn untracked(tracking: &Tracking) -> bool {
    matches!(tracking, Tracking::NoSession | Tracking::NoTasks)
}

/// The advisory text after a tool call, or an empty string.
pub(crate) fn handle_post_tool_use(
    tracker: &TaskTracker,
    payload: &serde_json::Value,
    session_id: Option<&str>,
    state_dir: &Path,
) -> String {
    if !tracker.nudges {
        return String::new();
    }
    let Some(tool) = payload["tool_name"].as_str() else {
        return String::new();
    };
    let tracking = crate::task::tracking(state_dir);
    if tool == "AskUserQuestion" {
        return waiting_reminder(&tracking);
    }
    let Some(path) = session_id.and_then(|id| state_path(state_dir, id)) else {
        return String::new();
    };
    let Some(mut state) = load(&path) else {
        return String::new();
    };
    let before = (
        state.calls,
        state.last_nudge,
        state.skill_reminded,
        state.commit_denied,
    );
    let text = if tool == "Skill" {
        skill_reminder(tracker, payload, &tracking, &mut state)
    } else if MUTATING_TOOLS.contains(&tool) {
        work_nudge(tracker, &tracking, &mut state)
    } else {
        String::new()
    };
    if matches!(tracking, Tracking::Tracked { .. }) {
        state.calls = 0;
        state.last_nudge = 0;
    }
    if (
        state.calls,
        state.last_nudge,
        state.skill_reminded,
        state.commit_denied,
    ) != before
    {
        save(&path, &state);
    }
    text
}

fn skill_reminder(
    tracker: &TaskTracker,
    payload: &serde_json::Value,
    tracking: &Tracking,
    state: &mut NudgeState,
) -> String {
    let skill = payload["tool_input"]["skill"].as_str().unwrap_or_default();
    if state.skill_reminded || !untracked(tracking) || !is_workflow_skill(tracker, skill) {
        return String::new();
    }
    state.skill_reminded = true;
    format!(
        "llmenv task tracker: the skill '{skill}' runs in several steps, and no task tracks them. \
         {}",
        how_to_track(tracking)
    )
}

fn work_nudge(tracker: &TaskTracker, tracking: &Tracking, state: &mut NudgeState) -> String {
    if !untracked(tracking) {
        return String::new();
    }
    state.calls = state.calls.saturating_add(1);
    let after = tracker.nudge_after.unwrap_or(DEFAULT_NUDGE_AFTER);
    let every = tracker.nudge_every.unwrap_or(DEFAULT_NUDGE_EVERY);
    let due = if state.last_nudge == 0 {
        state.calls >= after
    } else {
        state.calls.saturating_sub(state.last_nudge) >= every
    };
    if !due {
        return String::new();
    }
    state.last_nudge = state.calls;
    format!(
        "llmenv task tracker: {} tool calls changed the project, and no task is open. If the work \
         has more than one part, track it. {}",
        state.calls,
        how_to_track(tracking)
    )
}

/// The reminder to park the task that waits for the user's answer.
fn waiting_reminder(tracking: &Tracking) -> String {
    match tracking {
        Tracking::Tracked {
            wip: Some(slug), ..
        } => format!(
            "llmenv task tracker: you asked the user a question while '{slug}' is in progress. If \
             you wait for the answer, run `llmenv task wait {slug} \"<what you wait for>\"`. After \
             the user answers, run `llmenv task start {slug}`."
        ),
        _ => String::new(),
    }
}

/// The addition to the Stop reminder when the turn ends with a question to the user.
pub(crate) fn handle_stop(
    tracker: &TaskTracker,
    payload: &serde_json::Value,
    state_dir: &Path,
) -> String {
    if !tracker.nudges {
        return String::new();
    }
    let asks = payload["last_assistant_message"]
        .as_str()
        .is_some_and(|m| m.trim_end().ends_with('?'));
    if !asks {
        return String::new();
    }
    match crate::task::tracking(state_dir) {
        tracking @ Tracking::Tracked { wip: Some(_), .. } => waiting_reminder(&tracking),
        Tracking::Tracked { waiting, .. } if !waiting.is_empty() => format!(
            "llmenv task tracker: waiting on the user: {}. After the user answers, run `llmenv \
             task start <slug>`.",
            waiting.join(", ")
        ),
        _ => String::new(),
    }
}

/// Split `command` into segments at the shell operators `;`, `|`, `&`, and a newline, and each
/// segment into words. Quotes group a word and are dropped, so an operator inside quotes does not
/// split.
fn shell_segments(command: &str) -> Vec<Vec<String>> {
    let mut segments: Vec<Vec<String>> = Vec::new();
    let mut words: Vec<String> = Vec::new();
    let mut word = String::new();
    let mut in_word = false;
    let mut quote: Option<char> = None;
    let mut chars = command.chars();
    let end_word = |words: &mut Vec<String>, word: &mut String, in_word: &mut bool| {
        if *in_word {
            words.push(std::mem::take(word));
            *in_word = false;
        }
    };
    while let Some(c) = chars.next() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some('"'), '\\') => {
                if let Some(next) = chars.next() {
                    word.push(next);
                }
            }
            (Some(_), c) => word.push(c),
            (None, '\'' | '"') => {
                quote = Some(c);
                in_word = true;
            }
            (None, '\\') => {
                if let Some(next) = chars.next() {
                    word.push(next);
                    in_word = true;
                }
            }
            (None, ';' | '|' | '&' | '\n') => {
                end_word(&mut words, &mut word, &mut in_word);
                if !words.is_empty() {
                    segments.push(std::mem::take(&mut words));
                }
            }
            (None, c) if c.is_whitespace() => end_word(&mut words, &mut word, &mut in_word),
            (None, c) => {
                word.push(c);
                in_word = true;
            }
        }
    }
    end_word(&mut words, &mut word, &mut in_word);
    if !words.is_empty() {
        segments.push(words);
    }
    segments
}

/// The program name of a word: `/usr/bin/git` is `git`.
fn program(word: &str) -> &str {
    word.rsplit('/').next().unwrap_or(word)
}

/// Words that run the next command with another environment or privilege.
const WRAPPERS: [&str; 7] = ["env", "sudo", "command", "time", "nohup", "exec", "nice"];

/// Whether one segment starts a commit or a pull request.
fn starts_commit_or_pr(words: &[String]) -> bool {
    // Group and subshell openers, an env assignment, and a wrapper come before the program.
    let mut rest = words
        .iter()
        .map(|w| w.trim_start_matches(['(', '{']))
        .filter(|w| !w.is_empty())
        .skip_while(|w| (w.contains('=') && !w.starts_with('-')) || WRAPPERS.contains(&program(w)));
    let Some(first) = rest.next() else {
        return false;
    };
    let tail: Vec<&str> = rest.collect();
    match program(first) {
        "gh" => {
            let mut iter = tail.iter();
            let mut subcommands: Vec<&str> = Vec::new();
            while let Some(word) = iter.next() {
                match *word {
                    "-R" | "--repo" => {
                        iter.next();
                    }
                    w if w.starts_with('-') => {}
                    w => subcommands.push(w),
                }
                if subcommands.len() == 2 {
                    break;
                }
            }
            subcommands == ["pr", "create"]
        }
        "git" => {
            let mut iter = tail.iter();
            while let Some(word) = iter.next() {
                match *word {
                    "-C" | "-c" | "--git-dir" | "--work-tree" => {
                        iter.next();
                    }
                    w if w.starts_with('-') => {}
                    w => return w == "commit",
                }
            }
            false
        }
        "sh" | "bash" | "zsh" => match tail.as_slice() {
            ["-c", script, ..] => runs_commit_or_pr(script),
            _ => false,
        },
        _ => false,
    }
}

/// Whether the shell command runs `git commit` or `gh pr create` in any of its parts.
fn runs_commit_or_pr(command: &str) -> bool {
    shell_segments(command)
        .iter()
        .any(|words| starts_commit_or_pr(words))
}

/// The `__DENY__` text for the first commit or pull request with no task in progress, or an
/// empty string. The retry passes, and the marker clears when a task is in progress.
pub(crate) fn handle_pre_tool_use(
    tracker: &TaskTracker,
    payload: &serde_json::Value,
    session_id: Option<&str>,
    state_dir: &Path,
) -> String {
    if !tracker.enforce_commit || payload["tool_name"].as_str() != Some("Bash") {
        return String::new();
    }
    let command = payload["tool_input"]["command"]
        .as_str()
        .unwrap_or_default();
    if !runs_commit_or_pr(command) {
        return String::new();
    }
    let Some(id) = session_id else {
        return String::new();
    };
    let Some(path) = state_path(state_dir, id) else {
        tracing::error!("task commit gate off: the session id is not a plain name");
        return String::new();
    };
    let tracking = crate::task::tracking(state_dir);
    let Some(mut state) = load(&path) else {
        return String::new();
    };
    match tracking {
        Tracking::Unknown => {
            tracing::error!("task commit gate off: the task store cannot be read");
            String::new()
        }
        Tracking::Tracked { wip: Some(_), .. } => {
            if state.commit_denied {
                state.commit_denied = false;
                if !save(&path, &state) {
                    tracing::error!(
                        "task commit gate: cannot clear the deny marker, so the next commit \
                         passes without a task check"
                    );
                }
            }
            String::new()
        }
        _ if state.commit_denied => String::new(),
        tracking => {
            state.commit_denied = true;
            // Without the marker the retry would be denied again, so a failed save allows.
            if !save(&path, &state) {
                return String::new();
            }
            format!(
                "__DENY__:llmenv blocked this commit or pull request once: no task is in \
                 progress. {} Then run the same command again, and it goes through. To turn this \
                 off, set features.task_tracker.enforce_commit to false.",
                how_to_track(&tracking)
            )
        }
    }
}

#[cfg(test)]
#[expect(clippy::unwrap_used, clippy::panic, reason = "test code")]
mod tests {
    use super::*;
    use crate::task::session::{StartDecision, StartOutcome, start_session};
    use crate::task::{NewTask, ParentSpec, SessionChoice, add_task_with, start_task};
    use proptest::prelude::*;
    use tempfile::TempDir;

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

    #[test]
    fn commit_and_pull_request_commands_are_found_in_any_part_of_a_command() {
        for (command, expected) in [
            ("git commit -m x", true),
            ("git add -A && git commit -m x", true),
            ("cd repo; git commit", true),
            ("git -C repo commit -m x", true),
            ("git -c user.name=a commit", true),
            ("GIT_AUTHOR_NAME=a git commit", true),
            ("git status\ngit commit", true),
            ("gh pr create --title x", true),
            ("true || gh pr create", true),
            ("git status", false),
            ("git log --oneline", false),
            ("echo \"git commit\"", false),
            ("gh pr view 1", false),
            ("gh issue create", false),
            ("git commit-tree abc", false),
            ("", false),
            ("FOO=\"a b\" git commit -m x", true),
            ("env git commit", true),
            ("command git commit", true),
            ("/usr/bin/git commit -m x", true),
            ("(git commit -m x)", true),
            ("{ git commit; }", true),
            ("bash -c \"git add . && git commit -m x\"", true),
            ("gh -R owner/repo pr create", true),
            ("gh pr create --title \"a && b\"", true),
            ("echo \"x; git commit\"", false),
            ("echo 'a && gh pr create'", false),
            ("git commit -m \"fix: a; b\"", true),
            ("git status 2>&1", false),
            ("gh --no-pager pr create", true),
            ("git --no-pager commit -m x", true),
            ("gh --no-pager pr view 1", false),
            ("bash script.sh", false),
        ] {
            assert_eq!(runs_commit_or_pr(command), expected, "{command:?}");
        }
    }

    #[test]
    fn workflow_skills_match_without_the_plugin_prefix() {
        let tracker = TaskTracker::default();
        assert!(is_workflow_skill(&tracker, "dev-sprint"));
        assert!(is_workflow_skill(&tracker, "nbl-dev:ship-issue"));
        assert!(!is_workflow_skill(&tracker, "brainstorming"));
        let custom = TaskTracker {
            workflow_skills: Some(vec!["my:deploy".to_string()]),
            ..TaskTracker::default()
        };
        assert!(is_workflow_skill(&custom, "deploy"));
        assert!(!is_workflow_skill(&custom, "dev-sprint"));
    }

    #[test]
    fn a_session_id_that_is_not_a_plain_name_has_no_state_file() {
        let dir = Path::new("/state");
        assert!(state_path(dir, "abc-123").is_some());
        for bad in ["", "../x", "a/b", ".."] {
            assert_eq!(state_path(dir, bad), None, "{bad:?}");
        }
    }

    #[test]
    fn the_work_nudge_fires_after_n_calls_and_then_every_m() {
        let dir = TempDir::new().unwrap();
        let tracker = TaskTracker {
            nudge_after: Some(3),
            nudge_every: Some(2),
            ..TaskTracker::default()
        };
        let call = |name: &str| {
            handle_post_tool_use(
                &tracker,
                &serde_json::json!({ "tool_name": name }),
                Some("s1"),
                dir.path(),
            )
        };
        // No session yet: the project is untracked.
        assert_eq!(call("Edit"), "");
        assert_eq!(call("Write"), "");
        let third = call("Bash");
        assert!(
            third.contains("3 tool calls") && third.contains("llmenv task session start"),
            "{third}"
        );
        assert_eq!(call("Edit"), "");
        assert!(call("Edit").contains("5 tool calls"));
        // A read-only tool is not counted.
        assert_eq!(call("Read"), "");
    }

    #[test]
    fn a_session_with_tasks_stops_the_nudges_and_resets_the_count() {
        let dir = TempDir::new().unwrap();
        let tracker = TaskTracker {
            nudge_after: Some(2),
            ..TaskTracker::default()
        };
        let post = |name: &str| {
            handle_post_tool_use(
                &tracker,
                &serde_json::json!({ "tool_name": name }),
                Some("s1"),
                dir.path(),
            )
        };
        assert_eq!(post("Edit"), "");
        let session = open_session(dir.path());
        let text = post("Edit");
        assert!(
            text.contains("llmenv task add"),
            "an empty session gets the add command: {text}"
        );
        add(dir.path(), &session, "Step one");
        for _ in 0..5 {
            assert_eq!(post("Edit"), "");
        }
        let state = load(&state_path(dir.path(), "s1").unwrap()).unwrap();
        assert_eq!(state.calls, 0, "tracked work resets the count");
    }

    #[test]
    fn nudges_off_means_no_text_and_no_state() {
        let dir = TempDir::new().unwrap();
        let tracker = TaskTracker {
            nudges: false,
            nudge_after: Some(1),
            ..TaskTracker::default()
        };
        let text = handle_post_tool_use(
            &tracker,
            &serde_json::json!({ "tool_name": "Edit" }),
            Some("s1"),
            dir.path(),
        );
        assert_eq!(text, "");
        assert!(!dir.path().join("task_nudge").exists());
    }

    #[test]
    fn the_skill_reminder_fires_once_for_a_workflow_skill_with_no_task() {
        let dir = TempDir::new().unwrap();
        let tracker = TaskTracker::default();
        let skill = |name: &str| {
            handle_post_tool_use(
                &tracker,
                &serde_json::json!({ "tool_name": "Skill", "tool_input": { "skill": name } }),
                Some("s1"),
                dir.path(),
            )
        };
        assert_eq!(skill("brainstorming"), "");
        let first = skill("nbl-dev:ship-issue");
        assert!(
            first.contains("'nbl-dev:ship-issue'") && first.contains("--task"),
            "{first}"
        );
        assert_eq!(skill("dev-sprint"), "", "once for each session");
    }

    #[test]
    fn the_skill_reminder_stays_quiet_when_tasks_exist() {
        let dir = TempDir::new().unwrap();
        let session = open_session(dir.path());
        add(dir.path(), &session, "Step one");
        let text = handle_post_tool_use(
            &TaskTracker::default(),
            &serde_json::json!({ "tool_name": "Skill", "tool_input": { "skill": "dev-sprint" } }),
            Some("s1"),
            dir.path(),
        );
        assert_eq!(text, "");
    }

    #[test]
    fn the_first_commit_with_no_task_in_progress_is_denied_and_the_retry_passes() {
        let dir = TempDir::new().unwrap();
        let tracker = TaskTracker::default();
        let pre =
            |command: &str| handle_pre_tool_use(&tracker, &bash(command), Some("s1"), dir.path());
        let denied = pre("git commit -m x");
        assert!(denied.starts_with("__DENY__:"), "{denied}");
        assert!(denied.contains("llmenv task session start"), "{denied}");
        assert!(denied.contains("enforce_commit"), "{denied}");
        assert_eq!(pre("git commit -m x"), "", "the retry is allowed");
        assert_eq!(pre("git status"), "", "other commands never deny");
    }

    #[test]
    fn a_task_in_progress_allows_the_commit_and_rearms_the_deny() {
        let dir = TempDir::new().unwrap();
        let tracker = TaskTracker::default();
        let session = open_session(dir.path());
        let slug = add(dir.path(), &session, "Step one");
        assert!(
            handle_pre_tool_use(&tracker, &bash("git commit"), Some("s1"), dir.path())
                .contains("llmenv task start")
        );
        start_task(dir.path(), &slug, false).unwrap();
        let logs = crate::test_log_capture::capture_logs(|| {
            assert_eq!(
                handle_pre_tool_use(&tracker, &bash("git commit"), Some("s1"), dir.path()),
                ""
            );
        });
        assert!(
            !load(&state_path(dir.path(), "s1").unwrap())
                .unwrap()
                .commit_denied
        );
        assert!(
            !logs.contains("deny marker"),
            "a saved marker logs nothing: {logs}"
        );
    }

    #[test]
    fn the_deny_switch_and_other_tools_pass_through() {
        let dir = TempDir::new().unwrap();
        let off = TaskTracker {
            enforce_commit: false,
            ..TaskTracker::default()
        };
        assert_eq!(
            handle_pre_tool_use(&off, &bash("git commit"), Some("s1"), dir.path()),
            ""
        );
        let on = TaskTracker::default();
        let edit =
            serde_json::json!({ "tool_name": "Edit", "tool_input": { "command": "git commit" } });
        assert_eq!(handle_pre_tool_use(&on, &edit, Some("s1"), dir.path()), "");
        assert_eq!(
            handle_pre_tool_use(&on, &bash("git commit"), None, dir.path()),
            ""
        );
    }

    #[test]
    fn a_question_to_the_user_parks_the_task_in_progress() {
        let dir = TempDir::new().unwrap();
        let session = open_session(dir.path());
        let slug = add(dir.path(), &session, "Step one");
        start_task(dir.path(), &slug, false).unwrap();
        let tracker = TaskTracker::default();
        let ask = serde_json::json!({ "tool_name": "AskUserQuestion" });
        let text = handle_post_tool_use(&tracker, &ask, Some("s1"), dir.path());
        assert!(text.contains(&format!("llmenv task wait {slug}")), "{text}");
        assert!(
            text.contains(&format!("llmenv task start {slug}")),
            "{text}"
        );
        let stop = serde_json::json!({ "last_assistant_message": "Which one do you want?" });
        assert!(handle_stop(&tracker, &stop, dir.path()).contains("llmenv task wait"));
        let no_question = serde_json::json!({ "last_assistant_message": "Done." });
        assert_eq!(handle_stop(&tracker, &no_question, dir.path()), "");
        let off = TaskTracker {
            nudges: false,
            ..TaskTracker::default()
        };
        assert_eq!(handle_stop(&off, &stop, dir.path()), "");
    }

    #[test]
    fn a_waiting_task_is_reported_as_waiting_on_the_user_at_a_question() {
        let dir = TempDir::new().unwrap();
        let session = open_session(dir.path());
        let slug = add(dir.path(), &session, "Step one");
        start_task(dir.path(), &slug, false).unwrap();
        crate::task::wait_task(dir.path(), &slug, "needs an answer").unwrap();
        let stop = serde_json::json!({ "last_assistant_message": "Ready?" });
        let text = handle_stop(&TaskTracker::default(), &stop, dir.path());
        assert!(text.contains("waiting on the user: step-one"), "{text}");
    }

    #[test]
    fn a_state_file_that_cannot_be_read_skips_the_nudge_and_a_corrupt_one_resets() {
        let dir = TempDir::new().unwrap();
        let tracker = TaskTracker {
            nudge_after: Some(1),
            ..TaskTracker::default()
        };
        let path = state_path(dir.path(), "s1").unwrap();
        // A directory in place of the file: reading fails with an error that is not NotFound.
        std::fs::create_dir_all(&path).unwrap();
        let edit = serde_json::json!({ "tool_name": "Edit" });
        assert_eq!(
            handle_post_tool_use(&tracker, &edit, Some("s1"), dir.path()),
            ""
        );
        assert_eq!(
            handle_pre_tool_use(&tracker, &bash("git commit"), Some("s1"), dir.path()),
            ""
        );
        std::fs::remove_dir(&path).unwrap();
        std::fs::write(&path, "not json").unwrap();
        let text = handle_post_tool_use(&tracker, &edit, Some("s1"), dir.path());
        assert!(text.contains("1 tool calls"), "{text}");
    }

    #[test]
    fn an_unreadable_task_store_never_denies_or_nudges() {
        let dir = TempDir::new().unwrap();
        // A file where the task store directory belongs: the store cannot be read.
        std::fs::write(dir.path().join("tasks"), "x").unwrap();
        assert_eq!(crate::task::tracking(dir.path()), Tracking::Unknown);
        let tracker = TaskTracker {
            nudge_after: Some(1),
            ..TaskTracker::default()
        };
        assert_eq!(
            handle_pre_tool_use(&tracker, &bash("git commit"), Some("s1"), dir.path()),
            ""
        );
        let edit = serde_json::json!({ "tool_name": "Edit" });
        assert_eq!(
            handle_post_tool_use(&tracker, &edit, Some("s1"), dir.path()),
            ""
        );
    }

    #[test]
    fn a_question_with_only_unstarted_tasks_says_nothing() {
        let dir = TempDir::new().unwrap();
        let session = open_session(dir.path());
        add(dir.path(), &session, "Step one");
        let stop = serde_json::json!({ "last_assistant_message": "Ready?" });
        assert_eq!(handle_stop(&TaskTracker::default(), &stop, dir.path()), "");
    }

    proptest! {
        #[test]
        fn nudge_state_survives_a_json_roundtrip(
            calls in any::<u32>(),
            last_nudge in any::<u32>(),
            skill_reminded in any::<bool>(),
            commit_denied in any::<bool>(),
        ) {
            let state = NudgeState { calls, last_nudge, skill_reminded, commit_denied };
            let json = serde_json::to_string(&state).unwrap();
            prop_assert_eq!(serde_json::from_str::<NudgeState>(&json).unwrap(), state);
        }

        #[test]
        fn a_command_without_commit_or_pr_create_is_never_matched(command in "[a-z0-9 -;|&\n]{0,60}") {
            prop_assume!(!command.contains("commit") && !command.contains("pr create"));
            prop_assert!(!runs_commit_or_pr(&command));
        }

        #[test]
        fn matching_never_panics(command in "\\PC{0,80}") {
            let _ = runs_commit_or_pr(&command);
        }
    }
}
