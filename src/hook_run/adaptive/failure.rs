//! The failure recall: which failures skip it (#2910), and which memories it keeps (#2921).
//!
//! A typo (exit 127) or a parse error in the agent's own script matches memories from every
//! project. A recall for such a failure injects unrelated notes, so the recall is skipped.

use std::collections::BTreeSet;

use super::{
    AdaptiveCtx, FAILURE_BUDGET_BYTES, FAILURE_LIMIT, FANOUT_LIMIT, Outcome, RECALL_STATE_UNSAVED,
    RESOLVED_TOPIC, agent_key, note_skipped, note_unless_saved, query, report, report_ledger,
};
use crate::hook_run::action::{Action, RecallQuery, Recalled, Reply};
use crate::hook_run::recall::RecallBudget;
use crate::hook_run::relevance;
use crate::hook_run::session_state::unix_now;
use llmenv_mcp::mcp_client::McpHttpClient;

/// Words that appear in most failures and in most memories, so a shared one shows no relation.
const STOP_WORDS: [&str; 40] = [
    "after", "before", "cannot", "code", "command", "could", "error", "errors", "exit", "expected",
    "fail", "failed", "failure", "false", "file", "files", "found", "from", "have", "help", "into",
    "line", "none", "note", "null", "path", "result", "returned", "running", "status", "test",
    "tests", "that", "then", "there", "this", "true", "unable", "warning", "with",
];

/// Text that marks a generic failure. Matched case-insensitively.
const GENERIC_MARKERS: [&str; 5] = [
    "exit code 127",
    "command not found",
    "syntaxerror",
    "json.parse",
    "unexpected token",
];

/// Whether `error` is a generic failure, so a failure recall would inject unrelated memories.
pub(super) fn is_generic(error: &str) -> bool {
    // Only the first line names the failure. A long log of a real failure can quote a marker
    // further down, and that failure is not generic.
    let first = error
        .lines()
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    GENERIC_MARKERS.iter().any(|marker| first.contains(marker))
}

/// Recalls `query` for a failure, and keeps only the memories that share a token in `wanted`.
/// `None` means no recall ran. The call goes past [`Action::run`], because the match reads the
/// keywords, which the injected record does not keep.
///
/// ICM normalizes its score per query, so the best of an unrelated set also scores near 1.0
/// and cannot gate the injection. A raw score is requested upstream (rtk-ai/icm#521).
async fn recall(
    client: &McpHttpClient,
    query: Option<RecallQuery>,
    wanted: &BTreeSet<String>,
) -> Option<anyhow::Result<Reply>> {
    let action = Action::RecallQuery(query?);
    let reply = client
        .call_tool(action.tool_name(), action.arguments("", ""))
        .await;
    Some(reply.and_then(|body| related_records(&body, wanted).map(Reply::Records)))
}

/// The memories of a JSON recall reply that share a token in `wanted`, matched on the summary
/// and the keywords.
///
/// # Errors
/// The reply is not a JSON list of icm records.
fn related_records(body: &str, wanted: &BTreeSet<String>) -> anyhow::Result<Vec<Recalled>> {
    let shares = |text: &str| !distinctive_tokens(text).is_disjoint(wanted);
    Ok(crate::memory::recall_json::parse(body)?
        .iter()
        .filter(|m| shares(&format!("{} {}", m.summary, m.keywords.join(" "))))
        .map(Recalled::from_record)
        .collect())
}

/// The lowercase tokens of `text` that can tie a memory to a failure: a word of four or more
/// characters that is not a stop word, or an error code such as `E0063`.
pub(super) fn distinctive_tokens(text: &str) -> BTreeSet<String> {
    // Lowercase first: a letter such as `İ` lowercases to a letter plus a combining mark, and the
    // mark must split a token like any other non-word character.
    text.to_lowercase()
        .split(|c: char| !(c.is_alphanumeric() || c == '_'))
        .filter(|t| is_distinctive(t))
        .map(str::to_string)
        .collect()
}

fn is_distinctive(token: &str) -> bool {
    // A number alone is a line, a count, or a year, so it names no failure.
    if !token.chars().any(char::is_alphabetic) {
        return false;
    }
    // An error code such as E0063 or ENOENT2 is ASCII, so `len` counts its characters.
    let is_code = token.is_ascii() && token.len() >= 3 && token.bytes().any(|b| b.is_ascii_digit());
    is_code || (token.chars().count() >= 4 && !STOP_WORDS.contains(&token))
}

/// `PostToolUseFailure`: record the error, then inject related memories once.
pub(in crate::hook_run) async fn tool_failure(
    ctx: &AdaptiveCtx<'_>,
) -> anyhow::Result<crate::hook_run::HookText> {
    let tool = ctx.payload["tool_name"].as_str().unwrap_or_default();
    let error = ctx.payload["error"].as_str().unwrap_or_default();
    let now = unix_now();
    // One lock covers the error record and the read. Without the read, a busy or
    // unreadable ledger would drop the record with no log (#2723).
    let Some(ledger) = ctx.store.update(ctx.session_id, |l| {
        l.push_error(tool, error, now);
        l.clone()
    }) else {
        crate::agent_notice::record_logged(format!(
            "llmenv: tool_failure: session {} tool {tool}: error not recorded and recall skipped, the recall ledger cannot be updated (unsafe session id, busy lock, or unreadable file). Fix: check the permissions of the session state folder.",
            ctx.session_id
        ));
        note_skipped();
        return Ok(crate::hook_run::HookText::default());
    };
    // A generic failure matches memories from every project, so no recall runs (#2910).
    if is_generic(error) {
        return Ok(crate::hook_run::HookText::default());
    }
    // The capped query text without the tool name: "bash" would tie every shell note to the
    // failure, and a long log would tie almost anything. With no token, no memory can pass.
    let wanted = distinctive_tokens(&relevance::error_query("", error));
    if wanted.is_empty() {
        tracing::debug!("tool_failure: the error names no distinctive token, so no recall runs");
        return Ok(crate::hook_run::HookText::default());
    }
    let (key, record) = agent_key(ctx.payload);
    let sent = ledger.sent_for(&key);
    let text = relevance::error_query(tool, error);
    // The session project comes first, as in `waves`. The shared `errors-resolved` topic is
    // the only cross-project recall here, and only while `global_topics` lists it (#2950). No
    // all-projects recall runs on a failure (#2910).
    let resolved = ctx
        .global_topics
        .iter()
        .any(|t| t.trim() == RESOLVED_TOPIC)
        .then(|| RecallQuery {
            topic: Some(RESOLVED_TOPIC.to_string()),
            ..query(&text, FANOUT_LIMIT)
        });
    let scoped = ledger.project.as_deref().map(|p| RecallQuery {
        project: Some(p.to_string()),
        ..query(&text, FAILURE_LIMIT)
    });
    // The all-projects recall is not run here: it injected other projects' notes (#2910).
    let (own, fixes) = tokio::join!(
        recall(ctx.client, scoped, &wanted),
        recall(ctx.client, resolved, &wanted),
    );
    let mut outcome = Outcome::default();
    outcome.push(own);
    outcome.push(fixes);
    let mut budget = RecallBudget::new(FAILURE_BUDGET_BYTES, sent);
    budget.add_records(std::mem::take(&mut outcome.records));
    report(ctx, "post_tool_use_failure", &outcome, &budget);
    if record {
        let kept = budget.kept_keys();
        let (updated, timing) = ctx
            .store
            .update_timed(ctx.session_id, |l| l.mark_sent(&key, kept));
        note_unless_saved(updated, RECALL_STATE_UNSAVED);
        report_ledger("post_tool_use_failure", &timing);
    }
    Ok(budget.hook_text(Vec::new()))
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::{GENERIC_MARKERS, STOP_WORDS, distinctive_tokens, is_generic, related_records};

    fn rel(body: &str, error: &str) -> String {
        related_records(body, &distinctive_tokens(error))
            .map(|records| crate::hook_run::action::Reply::Records(records).into_text())
            .unwrap_or_default()
    }

    fn reply(items: &[(&str, &str, &[&str])]) -> String {
        let list: Vec<serde_json::Value> = items
            .iter()
            .enumerate()
            .map(|(i, (t, s, k))| {
                serde_json::json!({
                    "id": format!("id{i}"), "topic": t, "summary": s, "keywords": k,
                    "importance": "low",
                })
            })
            .collect();
        serde_json::Value::Array(list).to_string()
    }

    #[test]
    fn tokens_keep_words_and_codes_and_drop_noise() {
        let tokens =
            distinctive_tokens("Exit code 101\nerror[E0063]: missing field `request_ctx` in 2026");
        let got: Vec<&str> = tokens.iter().map(String::as_str).collect();
        assert_eq!(got, ["e0063", "field", "missing", "request_ctx"]);
    }

    #[test]
    fn a_memory_needs_a_shared_token_in_its_summary_or_keywords() {
        let body = reply(&[
            ("a", "Semgrep rules need a pinned ruleset", &["semgrep"]),
            ("b", "pin mcp below 2", &["request_ctx"]),
            ("c", "the E0063 fix", &[]),
        ]);
        let out = rel(&body, "error[E0063]: missing request_ctx");
        assert_eq!(out, "[b] pin mcp below 2\n[c] the E0063 fix");
    }

    #[test]
    fn only_stop_words_in_common_is_no_relation() {
        let body = reply(&[("a", "the command failed with an error", &["error"])]);
        assert_eq!(rel(&body, "Error: command failed"), "");
    }

    #[test]
    fn an_empty_list_and_a_trailing_note_are_read() {
        assert_eq!(rel("[]", "boom"), "");
        let body = format!(
            "{}\n\n[ICM: 9 tool calls since last store.]",
            reply(&[("a", "boom fix", &[])])
        );
        assert_eq!(rel(&body, "boom"), "[a] boom fix");
    }

    /// A text reply comes from an icm older than 0.11.5, so it is an error that names the fix.
    #[test]
    fn a_text_reply_is_an_error() {
        let wanted = distinctive_tokens("ImportError request_ctx");
        for body in [
            "[errors-resolved] pin mcp for request_ctx",
            "No memories found.",
            "",
        ] {
            let Err(err) = related_records(body, &wanted) else {
                unreachable!("a text reply must be an error: {body:?}");
            };
            let fix = if body.is_empty() {
                "llmenv doctor"
            } else {
                "Fix: upgrade icm to 0.11.5 or later"
            };
            assert!(err.to_string().contains(fix), "{err}");
        }
    }

    #[test]
    fn a_letter_that_lowercases_to_a_mark_splits_the_token() {
        let tokens = distinctive_tokens("İstanbul");
        assert!(tokens.contains("stanbul"), "got {tokens:?}");
        assert!(
            tokens
                .iter()
                .all(|t| t.chars().all(|c| c.is_alphanumeric() || c == '_'))
        );
    }

    proptest! {
        /// Every token is a lowercase word of the input, has a letter, and has three or more
        /// characters.
        #[test]
        fn every_token_is_a_long_lowercase_word(text in "\\PC{0,120}") {
            let lower = text.to_lowercase();
            for t in distinctive_tokens(&text) {
                prop_assert_eq!(t.to_lowercase(), t.clone());
                prop_assert!(lower.contains(t.as_str()));
                prop_assert!(t.chars().any(char::is_alphabetic));
                prop_assert!(t.chars().all(|c| c.is_alphanumeric() || c == '_'));
                prop_assert!(t.chars().count() >= 3);
            }
        }

        /// Any reply text gives a result and never panics.
        #[test]
        fn any_reply_body_is_handled(body in "\\PC{0,400}", error in "\\PC{0,60}") {
            let _ = rel(&body, &error);
        }

        /// The note icm appends after the JSON list never changes the result.
        #[test]
        fn a_trailing_note_never_changes_the_result(
            items in prop::collection::vec(("[a-z]{1,8}", "[a-z ]{0,40}"), 0..6),
            error in "[a-z ]{0,40}",
            calls in 0u32..1000,
        ) {
            let items: Vec<(&str, &str, &[&str])> =
                items.iter().map(|(t, s)| (t.as_str(), s.as_str(), &[][..])).collect();
            let body = reply(&items);
            let noted = format!("{body}\n\n[ICM: {calls} tool calls since last store.]");
            prop_assert_eq!(rel(&noted, &error), rel(&body, &error));
        }

        /// A memory built from words the error does not hold is always dropped.
        #[test]
        fn an_unrelated_memory_is_dropped(
            error_words in prop::collection::vec(prop::sample::select(vec!["kestrel", "quartz", "nimbus"]), 1..4),
            memory_words in prop::collection::vec(prop::sample::select(vec!["walrus", "tundra", "cobalt"]), 1..4),
        ) {
            let summary = memory_words.join(" ");
            let body = reply(&[("t", &summary, &memory_words)]);
            prop_assert_eq!(rel(&body, &error_words.join(" ")), "");
        }

        /// The tokens of a token list are that list, and no token is a stop word or a number.
        #[test]
        fn tokens_are_stable_and_exclude_noise(text in "\\PC{0,120}") {
            let tokens = distinctive_tokens(&text);
            let joined: Vec<&str> = tokens.iter().map(String::as_str).collect();
            prop_assert_eq!(distinctive_tokens(&joined.join(" ")), tokens.clone());
            for t in &tokens {
                prop_assert!(!STOP_WORDS.contains(&t.as_str()), "{}", t);
                prop_assert!(!t.chars().all(|c| c.is_ascii_digit()), "{}", t);
            }
        }

        /// A marker in any letter case at the start of the first line is generic, and lines after
        /// the first never change the verdict.
        #[test]
        fn is_generic_reads_the_first_line_only(
            marker in prop::sample::select(GENERIC_MARKERS.to_vec()),
            upper in prop::collection::vec(any::<bool>(), 32),
            first in "[a-z ]{0,30}",
            rest in prop::collection::vec("[a-zA-Z0-9 .:]{0,30}", 0..4),
        ) {
            let cased: String = marker
                .chars()
                .zip(upper.iter().cycle())
                .map(|(c, up)| if *up { c.to_ascii_uppercase() } else { c })
                .collect();
            let line = format!("{cased} {first}");
            prop_assert!(is_generic(&line));
            let tail = rest.join("\n");
            let text = format!("{first}\n{tail}");
            prop_assert_eq!(is_generic(&text), is_generic(&first));
        }

        /// A memory that repeats the error always shares a token with it, when the error has one.
        #[test]
        fn a_memory_that_quotes_the_error_is_kept(error in "[a-zA-Z0-9_ :]{0,60}") {
            let body = reply(&[("t", &error, &[])]);
            let out = rel(&body, &error);
            prop_assert_eq!(out.is_empty(), distinctive_tokens(&error).is_empty());
        }
    }

    #[test]
    fn a_typo_and_a_script_parse_error_are_generic() {
        assert!(is_generic("Exit code 127\nzsh: command not found: gti"));
        assert!(is_generic(
            "SyntaxError: Unexpected token } in JSON at position 4"
        ));
    }

    #[test]
    fn a_marker_deep_in_a_long_log_is_not_generic() {
        let log = "Exit code 1\nrunning 40 tests\nthe parser test printed:\nSyntaxError: bad input";
        assert!(!is_generic(log));
    }

    #[test]
    fn a_failing_test_is_not_generic() {
        assert!(!is_generic(
            "Exit code 1\nthread 'parse' panicked at src/lib.rs:4"
        ));
        assert!(!is_generic(""));
    }
}
