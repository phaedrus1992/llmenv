//! Minimal reader for Claude Code's transcript JSONL (#317, phase 3).
//!
//! The transcript path arrives on the hook payload as `transcript_path`. Only
//! the tail is read: these layers ask about the *current* turn, and a long
//! session's transcript is large enough that parsing all of it on every tool
//! call would be a per-call cost nobody agreed to.
//!
//! Format, verified against a real transcript rather than assumed:
//!
//! - a line is a JSON object with a `type` (`user`, `assistant`, and several
//!   non-message kinds like `attachment`, `mode`, `last-prompt`);
//! - message lines carry `message.role` and `message.content`, where content is
//!   either a string or an array of blocks (`text`, `thinking`, `tool_use`,
//!   `tool_result`).
//!
//! The trap: **a tool result is a `user` line.** Treating every `user` entry as
//! something the human said would make every tool result look like a fresh
//! prompt, which is exactly backwards for layers that ask "has the human been
//! answered yet". A genuine user message is one whose content is a string or
//! contains a `text` block.

use std::path::Path;

/// What the tail of the transcript says about the current turn.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct TurnState {
    /// The last genuine user message, if one was found in the tail. Private:
    /// callers ask `has_unanswered_question` rather than re-deriving "is this
    /// a question" from the raw text, so the rule lives in one place.
    last_user_text: Option<String>,
    /// Whether the assistant has produced visible text since then. Thinking
    /// and tool calls don't count: neither is something the user can read.
    pub(crate) assistant_spoke_since: bool,
}

impl TurnState {
    /// Whether the last user message reads as a question that hasn't been
    /// answered in text yet.
    pub(crate) fn has_unanswered_question(&self) -> bool {
        !self.assistant_spoke_since
            && self
                .last_user_text
                .as_deref()
                .is_some_and(|t| t.trim_end().ends_with('?'))
    }
}

/// How many trailing lines to parse. Generous enough to span a turn with a
/// long tool-call sequence, small enough that the read stays cheap.
const TAIL_LINES: usize = 200;

/// Read the tail of `path` and summarise the current turn.
///
/// Returns `None` when the transcript can't be read or parsed at all — these
/// layers fail open, since denying a tool call because a log file was
/// unreadable would be worse than the slippage they guard against.
pub(crate) fn read_turn_state(path: &Path) -> Option<TurnState> {
    let text = std::fs::read_to_string(path).ok()?;
    let lines: Vec<&str> = text.lines().collect();
    let tail = lines.len().saturating_sub(TAIL_LINES);
    let mut state = TurnState::default();
    for line in &lines[tail..] {
        let Ok(entry) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let Some(message) = entry.get("message") else {
            continue;
        };
        match message.get("role").and_then(serde_json::Value::as_str) {
            Some("user") => {
                if let Some(text) = user_text(message) {
                    state.last_user_text = Some(text);
                    state.assistant_spoke_since = false;
                }
            }
            Some("assistant") if assistant_spoke(message) => {
                state.assistant_spoke_since = true;
            }
            _ => {}
        }
    }
    Some(state)
}

/// The human-authored text of a user message, or `None` when the entry is a
/// tool result rather than something the user typed.
fn user_text(message: &serde_json::Value) -> Option<String> {
    match message.get("content")? {
        serde_json::Value::String(s) => Some(s.clone()),
        serde_json::Value::Array(blocks) => {
            let text: String = blocks
                .iter()
                .filter(|b| b.get("type").and_then(serde_json::Value::as_str) == Some("text"))
                .filter_map(|b| b.get("text").and_then(serde_json::Value::as_str))
                .collect::<Vec<_>>()
                .join("\n");
            (!text.trim().is_empty()).then_some(text)
        }
        _ => None,
    }
}

/// Whether an assistant message contains text the user can read. `thinking`
/// and `tool_use` blocks are invisible to them, so neither counts as having
/// said anything.
fn assistant_spoke(message: &serde_json::Value) -> bool {
    match message.get("content") {
        Some(serde_json::Value::String(s)) => !s.trim().is_empty(),
        Some(serde_json::Value::Array(blocks)) => blocks.iter().any(|b| {
            b.get("type").and_then(serde_json::Value::as_str) == Some("text")
                && b.get("text")
                    .and_then(serde_json::Value::as_str)
                    .is_some_and(|t| !t.trim().is_empty())
        }),
        _ => false,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    fn transcript(lines: &[serde_json::Value]) -> tempfile::NamedTempFile {
        let file = tempfile::NamedTempFile::new().unwrap();
        let body: String = lines
            .iter()
            .map(|l| format!("{l}\n"))
            .collect::<Vec<_>>()
            .concat();
        std::fs::write(file.path(), body).unwrap();
        file
    }

    fn user(text: &str) -> serde_json::Value {
        serde_json::json!({
            "type": "user",
            "message": { "role": "user", "content": [{ "type": "text", "text": text }] },
        })
    }

    fn tool_result() -> serde_json::Value {
        serde_json::json!({
            "type": "user",
            "message": {
                "role": "user",
                "content": [{ "type": "tool_result", "content": "ok" }],
            },
        })
    }

    fn assistant_text(text: &str) -> serde_json::Value {
        serde_json::json!({
            "type": "assistant",
            "message": { "role": "assistant", "content": [{ "type": "text", "text": text }] },
        })
    }

    fn assistant_tool_use() -> serde_json::Value {
        serde_json::json!({
            "type": "assistant",
            "message": {
                "role": "assistant",
                "content": [{ "type": "tool_use", "name": "Bash", "input": {} }],
            },
        })
    }

    #[test]
    fn a_question_with_no_answer_yet_is_unanswered() {
        let file = transcript(&[user("why is this failing?"), assistant_tool_use()]);
        let state = read_turn_state(file.path()).unwrap();
        assert!(state.has_unanswered_question());
    }

    #[test]
    fn a_question_the_assistant_answered_in_text_is_answered() {
        let file = transcript(&[
            user("why is this failing?"),
            assistant_text("because the path is wrong"),
            assistant_tool_use(),
        ]);
        assert!(
            !read_turn_state(file.path())
                .unwrap()
                .has_unanswered_question()
        );
    }

    // The format trap: tool results are `user` lines. Counting them as user
    // messages would reset the turn on every tool call, so a question asked
    // three tools ago would look answered.
    #[test]
    fn a_tool_result_is_not_treated_as_something_the_user_said() {
        let file = transcript(&[
            user("what broke?"),
            assistant_tool_use(),
            tool_result(),
            assistant_tool_use(),
            tool_result(),
        ]);
        let state = read_turn_state(file.path()).unwrap();
        assert_eq!(state.last_user_text.as_deref(), Some("what broke?"));
        assert!(
            state.has_unanswered_question(),
            "tool results must not count as the question being answered"
        );
    }

    // Thinking is invisible to the user, so it can't be the answer.
    #[test]
    fn thinking_does_not_count_as_speaking() {
        let file = transcript(&[
            user("is this safe?"),
            serde_json::json!({
                "type": "assistant",
                "message": {
                    "role": "assistant",
                    "content": [{ "type": "thinking", "thinking": "let me consider" }],
                },
            }),
        ]);
        assert!(
            read_turn_state(file.path())
                .unwrap()
                .has_unanswered_question()
        );
    }

    #[test]
    fn a_statement_is_not_a_question() {
        let file = transcript(&[user("fix the parser"), assistant_tool_use()]);
        assert!(
            !read_turn_state(file.path())
                .unwrap()
                .has_unanswered_question()
        );
    }

    #[test]
    fn a_new_user_message_resets_the_turn() {
        let file = transcript(&[
            user("why?"),
            assistant_text("because"),
            user("and now what?"),
        ]);
        let state = read_turn_state(file.path()).unwrap();
        assert_eq!(state.last_user_text.as_deref(), Some("and now what?"));
        assert!(state.has_unanswered_question());
    }

    #[test]
    fn non_message_lines_and_junk_are_skipped() {
        let file = transcript(&[
            serde_json::json!({ "type": "attachment" }),
            serde_json::json!({ "type": "mode", "mode": "default" }),
            user("ok?"),
        ]);
        std::fs::write(
            file.path(),
            format!(
                "not json at all\n{}\n",
                std::fs::read_to_string(file.path()).unwrap().trim_end()
            ),
        )
        .unwrap();
        assert!(
            read_turn_state(file.path())
                .unwrap()
                .has_unanswered_question()
        );
    }

    #[test]
    fn an_unreadable_transcript_is_none_rather_than_a_panic() {
        assert!(read_turn_state(Path::new("/nonexistent/llmenv/transcript.jsonl")).is_none());
    }
}
