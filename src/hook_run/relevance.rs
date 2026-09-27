//! Pure functions that turn session signals into ICM recall queries (#2249).
//!
//! Design: docs/superpowers/specs/2026-09-27-adaptive-icm-recall-design.md

use std::collections::VecDeque;
use std::path::Path;

use crate::hook_run::session_ledger::{Activity, ToolError};

const QUERY_CAP: usize = 600;
const PROMPT_CAP: usize = 500;
const TAIL_CAP: usize = 300;
const ACTIVITY_WINDOW: usize = 10;
const FANOUT_KEYWORDS: usize = 2;
const MAX_SIBLINGS: usize = 2;
/// Directory names too common to say anything about the topic of the work.
const GENERIC_DIRS: &[&str] = &["src", "tests", "test", "lib", "docs", "crates", "."];

/// What a `TurnStart` knows about the session.
#[derive(Debug, Clone, Copy)]
pub(crate) struct TurnSignals<'a> {
    pub(crate) prompt: &'a str,
    pub(crate) activity: &'a VecDeque<Activity>,
    pub(crate) errors: &'a VecDeque<ToolError>,
    pub(crate) last_turn_at: i64,
    pub(crate) assistant_tail: Option<&'a str>,
}

fn cap_chars(text: &str, max: usize) -> String {
    text.chars()
        .take(max)
        .collect::<String>()
        .trim()
        .to_string()
}

fn join_capped(parts: &[String]) -> String {
    let joined = parts
        .iter()
        .filter(|p| !p.is_empty())
        .cloned()
        .collect::<Vec<_>>()
        .join(" ");
    cap_chars(&joined, QUERY_CAP)
}

/// Reduce a tool call to the part that names a topic: a file path or a command name.
pub(crate) fn activity_from_tool_call(
    tool_name: &str,
    tool_input: &serde_json::Value,
    at: i64,
) -> Activity {
    let path = ["file_path", "notebook_path", "path"]
        .iter()
        .find_map(|key| tool_input.get(key).and_then(serde_json::Value::as_str));
    let command = tool_input
        .get("command")
        .and_then(serde_json::Value::as_str)
        .and_then(|c| c.split_whitespace().find(|word| !word.contains('=')))
        .and_then(|word| Path::new(word).file_name())
        .and_then(|name| name.to_str());
    Activity {
        tool: tool_name.to_string(),
        target: path.or(command).map(str::to_string),
        at,
    }
}

/// The directory that names the topic of `path`: its parent, or the grandparent
/// when the parent is a generic name such as `src`.
fn topic_dir(path: &Path) -> Option<&str> {
    let parent = path.parent();
    match dir_name(parent) {
        Some(dir) if GENERIC_DIRS.contains(&dir) => dir_name(parent.and_then(Path::parent)),
        other => other,
    }
}

fn dir_name(path: Option<&Path>) -> Option<&str> {
    path.and_then(Path::file_name).and_then(|n| n.to_str())
}

/// Topic terms from the newest activity: file stems, their directory, command names.
fn activity_terms(activity: &VecDeque<Activity>) -> Vec<String> {
    let mut terms: Vec<String> = Vec::new();
    let mut add = |term: &str| {
        if !term.is_empty() && !GENERIC_DIRS.contains(&term) && !terms.iter().any(|t| t == term) {
            terms.push(term.to_string());
        }
    };
    for target in activity
        .iter()
        .rev()
        .take(ACTIVITY_WINDOW)
        .filter_map(|a| a.target.as_deref())
    {
        let path = Path::new(target);
        match path.file_stem().and_then(|s| s.to_str()) {
            Some(stem) if target.contains('/') => {
                add(stem);
                topic_dir(path).into_iter().for_each(&mut add);
            }
            _ => add(target),
        }
    }
    terms
}

/// The activity terms used as exact-match `keyword` filters.
pub(crate) fn fanout_keywords(activity: &VecDeque<Activity>) -> Vec<String> {
    activity_terms(activity)
        .into_iter()
        .take(FANOUT_KEYWORDS)
        .collect()
}

/// The per-turn relevance query.
pub(crate) fn turn_query(signals: &TurnSignals<'_>) -> String {
    let fresh_error = signals
        .errors
        .back()
        .filter(|e| e.at > signals.last_turn_at)
        .map(|e| strip_exit_code(&e.head))
        .unwrap_or_default();
    join_capped(&[
        cap_chars(signals.prompt, PROMPT_CAP),
        activity_terms(signals.activity).join(" "),
        fresh_error,
        cap_chars(signals.assistant_tail.unwrap_or_default(), TAIL_CAP),
    ])
}

/// The `[topic]` of one recall record.
pub(crate) fn record_topic(record: &str) -> Option<&str> {
    let (topic, _) = record.strip_prefix('[')?.split_once(']')?;
    (!topic.is_empty()).then_some(topic)
}

/// Sibling topics of the canonical `context-X` / `decisions-X` names, plus
/// `errors-resolved`.
pub(crate) fn sibling_topics(hit_topics: &[String]) -> Vec<String> {
    let mut siblings: Vec<String> = hit_topics
        .iter()
        .filter_map(|topic| {
            if let Some(project) = topic.strip_prefix("context-") {
                Some(format!("decisions-{project}"))
            } else {
                topic
                    .strip_prefix("decisions-")
                    .map(|project| format!("context-{project}"))
            }
        })
        .collect();
    if !siblings.is_empty() {
        siblings.push("errors-resolved".to_string());
    }
    let mut unique: Vec<String> = Vec::new();
    for topic in siblings {
        if !hit_topics.contains(&topic) && !unique.contains(&topic) {
            unique.push(topic);
        }
    }
    unique.into_iter().take(MAX_SIBLINGS).collect()
}

/// The `Exit code N` line names no topic, so it is dropped.
fn strip_exit_code(error: &str) -> String {
    error
        .lines()
        .filter(|line| !line.starts_with("Exit code "))
        .collect::<Vec<_>>()
        .join(" ")
}

/// The recall query for one failed tool call.
pub(crate) fn error_query(tool: &str, error: &str) -> String {
    join_capped(&[tool.to_string(), strip_exit_code(error)])
}

/// The recall query for a new subagent.
pub(crate) fn subagent_query(
    task: Option<&str>,
    agent_type: &str,
    activity: &VecDeque<Activity>,
) -> String {
    match task {
        Some(task) => cap_chars(task, QUERY_CAP),
        None => join_capped(&[agent_type.to_string(), activity_terms(activity).join(" ")]),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use proptest::prelude::*;
    use serde_json::json;

    use super::*;
    use crate::hook_run::session_ledger::{Activity, ToolError};

    fn act(tool: &str, target: Option<&str>, at: i64) -> Activity {
        Activity {
            tool: tool.into(),
            target: target.map(Into::into),
            at,
        }
    }

    fn signals<'a>(
        prompt: &'a str,
        activity: &'a VecDeque<Activity>,
        errors: &'a VecDeque<ToolError>,
        last_turn_at: i64,
        assistant_tail: Option<&'a str>,
    ) -> TurnSignals<'a> {
        TurnSignals {
            prompt,
            activity,
            errors,
            last_turn_at,
            assistant_tail,
        }
    }

    #[test]
    fn activity_from_tool_call_reads_paths_and_command_names() {
        let read = activity_from_tool_call(
            "Read",
            &json!({"file_path": "/r/src/hook_run/recall.rs"}),
            1,
        );
        assert_eq!(read.target.as_deref(), Some("/r/src/hook_run/recall.rs"));
        let bash = activity_from_tool_call(
            "Bash",
            &json!({"command": "RUST_LOG=x cargo nextest run"}),
            1,
        );
        assert_eq!(bash.target.as_deref(), Some("cargo"));
        let other = activity_from_tool_call("WebSearch", &json!({"query": "q"}), 1);
        assert_eq!(other.target, None);
    }

    #[test]
    fn activity_terms_are_newest_first_distinct_and_skip_generic_dirs() {
        let ring: VecDeque<_> = [
            act("Read", Some("/r/src/hook_run/recall.rs"), 1),
            act("Bash", Some("cargo"), 2),
            act("Edit", Some("/r/crates/llmenv-config/src/schema.rs"), 3),
        ]
        .into();
        assert_eq!(
            activity_terms(&ring),
            ["schema", "llmenv-config", "cargo", "recall", "hook_run"]
        );
        assert_eq!(fanout_keywords(&ring), ["schema", "llmenv-config"]);
    }

    #[test]
    fn turn_query_combines_prompt_terms_new_error_and_tail() {
        let activity: VecDeque<_> = [act("Bash", Some("git"), 5)].into();
        let errors: VecDeque<_> = [ToolError {
            tool: "Bash".into(),
            head: "Exit code 1\nmerge conflict".into(),
            at: 9,
        }]
        .into();
        let q = turn_query(&signals(
            "continue",
            &activity,
            &errors,
            8,
            Some("resolving the CI yaml"),
        ));
        assert_eq!(q, "continue git merge conflict resolving the CI yaml");
        let old = turn_query(&signals("continue", &activity, &errors, 10, None));
        assert_eq!(
            old, "continue git",
            "an error older than the last turn is left out"
        );
    }

    #[test]
    fn sibling_topics_map_canonical_names_and_skip_hits() {
        let hits = vec!["context-llmenv".to_string(), "preferences".to_string()];
        assert_eq!(
            sibling_topics(&hits),
            ["decisions-llmenv", "errors-resolved"]
        );
        let both = vec!["context-x".to_string(), "decisions-x".to_string()];
        assert_eq!(sibling_topics(&both), ["errors-resolved"]);
        assert!(sibling_topics(&["preferences".to_string()]).is_empty());
    }

    #[test]
    fn record_topic_reads_the_bracket_prefix() {
        assert_eq!(
            record_topic("[context-llmenv] text"),
            Some("context-llmenv")
        );
        assert_eq!(record_topic("no topic"), None);
        assert_eq!(record_topic("[] empty"), None);
    }

    #[test]
    fn error_query_drops_the_exit_code_line() {
        assert_eq!(
            error_query(
                "Bash",
                "Exit code 101\nerror[E0063]: missing field `adaptive_recall`"
            ),
            "Bash error[E0063]: missing field `adaptive_recall`"
        );
    }

    #[test]
    fn subagent_query_prefers_the_task_and_falls_back_to_type_and_terms() {
        let ring: VecDeque<_> = [act("Bash", Some("gh"), 1)].into();
        assert_eq!(
            subagent_query(Some("map the recall path"), "Explore", &ring),
            "map the recall path"
        );
        assert_eq!(subagent_query(None, "Explore", &ring), "Explore gh");
    }

    proptest! {
        #[test]
        fn caps_never_split_a_char(prompt in "\\PC{0,900}", tail in "\\PC{0,900}") {
            let empty = VecDeque::new();
            let errors = VecDeque::new();
            let q = turn_query(&signals(&prompt, &empty, &errors, 0, Some(&tail)));
            prop_assert!(q.chars().count() <= QUERY_CAP);
            prop_assert_eq!(q, turn_query(&signals(&prompt, &empty, &errors, 0, Some(&tail))));
        }
    }
}
