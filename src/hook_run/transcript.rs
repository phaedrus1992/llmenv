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

/// How many trailing bytes to read. `transcript_path` comes from hook stdin, so
/// the read must stay bounded whatever the file is.
const TAIL_BYTES: u64 = 1024 * 1024;

/// The last [`TAIL_BYTES`] of a regular file, starting at a line boundary.
/// `None` for anything that is not a readable regular file.
fn read_tail(path: &Path) -> Option<String> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file = std::fs::File::open(path).ok()?;
    let meta = file.metadata().ok()?;
    if !meta.is_file() {
        return None;
    }
    let start = meta.len().saturating_sub(TAIL_BYTES);
    file.seek(SeekFrom::Start(start)).ok()?;
    let mut bytes = Vec::new();
    file.take(TAIL_BYTES).read_to_end(&mut bytes).ok()?;
    let text = String::from_utf8_lossy(&bytes).into_owned();
    if start == 0 {
        return Some(text);
    }
    // The first line after the seek is cut in the middle, so it is dropped.
    Some(
        text.split_once('\n')
            .map(|(_, rest)| rest.to_string())
            .unwrap_or_default(),
    )
}

/// Read the tail of `path` and summarise the current turn.
///
/// Returns `None` when the transcript can't be read or parsed at all — these
/// layers fail open, since denying a tool call because a log file was
/// unreadable would be worse than the slippage they guard against.
pub(crate) fn read_turn_state(path: &Path) -> Option<TurnState> {
    let text = read_tail(path)?;
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

/// The newest assistant text in the tail of `path`, capped at `max_chars`.
///
/// Returns `None` when the transcript can't be read or holds no assistant text.
pub(crate) fn last_assistant_text(path: &Path, max_chars: usize) -> Option<String> {
    let text = read_tail(path)?;
    let lines: Vec<&str> = text.lines().collect();
    let tail = lines.len().saturating_sub(TAIL_LINES);
    lines.get(tail..)?.iter().rev().find_map(|line| {
        let entry: serde_json::Value = serde_json::from_str(line).ok()?;
        let message = entry.get("message")?;
        if message.get("role").and_then(serde_json::Value::as_str) != Some("assistant") {
            return None;
        }
        // `user_text` reads the `text` blocks of any message; its name is historical.
        user_text(message).map(|t| t.chars().take(max_chars).collect())
    })
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

    #[test]
    fn transcript_reads_refuse_a_non_file_and_cap_a_large_one() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(last_assistant_text(dir.path(), 10), None);
        assert_eq!(read_turn_state(dir.path()), None);
        let filler = serde_json::json!({"message": {"role": "user",
            "content": [{"type": "tool_result", "content": "x".repeat(10_000)}]}});
        let mut lines = vec![filler; 300];
        lines.push(serde_json::json!({"message": {"role": "assistant",
            "content": [{"type": "text", "text": "last words"}]}}));
        let file = transcript(&lines);
        assert!(std::fs::metadata(file.path()).unwrap().len() > TAIL_BYTES);
        assert_eq!(
            last_assistant_text(file.path(), 20).as_deref(),
            Some("last words")
        );
    }

    #[test]
    fn the_tail_read_spans_more_than_a_few_kilobytes() {
        let filler = serde_json::json!({"message": {"role": "user",
            "content": [{"type": "tool_result", "content": "x".repeat(20_000)}]}});
        let file = transcript(&[
            serde_json::json!({"message": {"role": "assistant",
                "content": [{"type": "text", "text": "before the filler"}]}}),
            filler,
        ]);
        assert_eq!(
            last_assistant_text(file.path(), 40).as_deref(),
            Some("before the filler")
        );
    }

    #[test]
    fn last_assistant_text_returns_the_newest_visible_text_capped() {
        let assistant = |content: serde_json::Value| serde_json::json!({"message": {"role": "assistant", "content": content}});
        let file = transcript(&[
            assistant(serde_json::json!([{"type": "text", "text": "old"}])),
            assistant(serde_json::json!([{"type": "thinking", "thinking": "x"}])),
            assistant(serde_json::json!([{"type": "text", "text": "newest reply"}])),
            serde_json::json!({"message": {"role": "user",
                "content": [{"type": "tool_result", "content": "r"}]}}),
        ]);
        assert_eq!(
            last_assistant_text(file.path(), 6).as_deref(),
            Some("newest")
        );
        assert_eq!(
            last_assistant_text(std::path::Path::new("/nonexistent"), 6),
            None
        );
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
    // Content arrives either as an array of blocks or as a bare string. Every
    // test above uses the array form, so the string arm went unexercised —
    // deleting it entirely left the suite green.
    #[test]
    fn string_content_is_read_for_both_roles() {
        let file = transcript(&[serde_json::json!({
            "type": "user",
            "message": { "role": "user", "content": "is this right?" },
        })]);
        let state = read_turn_state(file.path()).unwrap();
        assert_eq!(state.last_user_text.as_deref(), Some("is this right?"));
        assert!(state.has_unanswered_question());

        let answered = transcript(&[
            serde_json::json!({
                "type": "user",
                "message": { "role": "user", "content": "is this right?" },
            }),
            serde_json::json!({
                "type": "assistant",
                "message": { "role": "assistant", "content": "yes, it is" },
            }),
        ]);
        assert!(
            !read_turn_state(answered.path())
                .unwrap()
                .has_unanswered_question(),
            "a string-form assistant reply is still the assistant speaking"
        );
    }

    // An empty or whitespace-only reply is not an answer, in either shape.
    #[test]
    fn blank_assistant_text_does_not_count_as_speaking() {
        for content in [
            serde_json::json!("   "),
            serde_json::json!([{ "type": "text", "text": "  " }]),
        ] {
            let file = transcript(&[
                user("did it work?"),
                serde_json::json!({
                    "type": "assistant",
                    "message": { "role": "assistant", "content": content },
                }),
            ]);
            assert!(
                read_turn_state(file.path())
                    .unwrap()
                    .has_unanswered_question(),
                "blank text is not an answer"
            );
        }
    }

    // A text block with no `text` field, or a non-text block claiming to be
    // one, must not register as speech — the `&&` between the two checks is
    // what enforces that.
    #[test]
    fn a_text_block_without_text_is_not_speech() {
        let file = transcript(&[
            user("well?"),
            serde_json::json!({
                "type": "assistant",
                "message": { "role": "assistant", "content": [{ "type": "text" }] },
            }),
        ]);
        assert!(
            read_turn_state(file.path())
                .unwrap()
                .has_unanswered_question()
        );
    }
}
