//! Adaptive recall flows for the lifecycle hooks (#2249).
//!
//! Design: docs/superpowers/specs/2026-09-27-adaptive-icm-recall-design.md

use std::path::Path;
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::hook_run::HookEvent;
use crate::hook_run::action::{Action, RecallQuery, split_recall_records};
use crate::hook_run::mcp_client::McpHttpClient;
use crate::hook_run::recall::{RECALL_BUDGET_BYTES, RecallBudget, run_with_budget_filtered};
use crate::hook_run::relevance::{self, TurnSignals};
use crate::hook_run::session_ledger::{Ledger, LedgerStore, MAIN_AGENT, record_hash, unix_now};
use crate::hook_run::transcript;

const FAILURE_BUDGET_BYTES: usize = 2_000;
const SUBAGENT_BUDGET_BYTES: usize = 4_000;
const MAIN_LIMIT: u8 = 10;
const FANOUT_LIMIT: u8 = 3;
const FAILURE_LIMIT: u8 = 5;
const TAIL_CHARS: usize = 300;
/// Wave 2 would push a slow backend past the latency that a prompt tolerates.
const WAVE2_CUTOFF: Duration = Duration::from_millis(1_500);

/// What every adaptive flow needs.
pub(super) struct AdaptiveCtx<'a> {
    pub(super) client: &'a McpHttpClient,
    pub(super) store: &'a LedgerStore,
    pub(super) session_id: &'a str,
    pub(super) payload: &'a Value,
}

fn query(text: &str, limit: u8) -> RecallQuery {
    RecallQuery {
        query: text.to_string(),
        topic: None,
        keyword: None,
        project: None,
        limit,
    }
}

/// Run one recall; a failure costs its own records only.
async fn recall(client: &McpHttpClient, q: Option<RecallQuery>) -> String {
    let Some(q) = q else {
        return String::new();
    };
    Action::RecallQuery(q)
        .run(client, "", "")
        .await
        .unwrap_or_else(|e| {
            tracing::warn!("adaptive recall call failed, its records are skipped: {e}");
            String::new()
        })
}

/// The recall texts for `text`, in budget order: main, keyword fanout, topic
/// fanout, cross-project.
async fn waves(client: &McpHttpClient, text: &str, keywords: &[String]) -> Vec<String> {
    let keyword = |i: usize| {
        keywords.get(i).map(|k| RecallQuery {
            keyword: Some(k.clone()),
            project: Some(String::new()),
            ..query(text, FANOUT_LIMIT)
        })
    };
    let cross = RecallQuery {
        project: Some(String::new()),
        ..query(text, FANOUT_LIMIT)
    };
    let start = Instant::now();
    let (main, kw0, kw1, cross) = tokio::join!(
        recall(client, Some(query(text, MAIN_LIMIT))),
        recall(client, keyword(0)),
        recall(client, keyword(1)),
        recall(client, Some(cross)),
    );
    let mut texts = vec![main, kw0, kw1];
    if start.elapsed() < WAVE2_CUTOFF {
        let topics: Vec<String> = texts
            .iter()
            .flat_map(|t| split_recall_records(t))
            .filter_map(|r| relevance::record_topic(&r).map(str::to_string))
            .collect();
        let siblings = relevance::sibling_topics(&topics);
        let topic = |i: usize| {
            siblings.get(i).map(|t| RecallQuery {
                topic: Some(t.clone()),
                ..query(text, FANOUT_LIMIT)
            })
        };
        let (t0, t1) = tokio::join!(recall(client, topic(0)), recall(client, topic(1)));
        texts.extend([t0, t1]);
    }
    texts.push(cross);
    texts
}

fn resets_ledger(source: Option<&str>) -> bool {
    matches!(source, Some("startup" | "clear" | "compact"))
}

/// `SessionStart`: reset per `source`, then send the wake-up pack and the scope set.
pub(super) async fn session_start(
    ctx: &AdaptiveCtx<'_>,
    wake: Action,
    scope: Vec<Action>,
) -> anyhow::Result<String> {
    if resets_ledger(ctx.payload["source"].as_str()) {
        ctx.store.update(ctx.session_id, Ledger::reset);
    }
    let sent = ctx
        .store
        .load(ctx.session_id)
        .unwrap_or_default()
        .sent_for(MAIN_AGENT);
    let mut actions = vec![wake];
    actions.extend(scope);
    let budget = RecallBudget::new(RECALL_BUDGET_BYTES, sent);
    let (text, budget) = run_with_budget_filtered(actions, budget, |a| async move {
        a.run(ctx.client, "", "").await
    })
    .await?;
    let kept = budget.kept_hashes();
    ctx.store.update(ctx.session_id, |l| {
        l.mark_sent(MAIN_AGENT, kept);
        l.set_scope_sent(MAIN_AGENT);
    });
    Ok(text)
}

/// Add the scope set to `budget`, for a session where no `SessionStart` ran in
/// this epoch.
async fn fallback_scope(ctx: &AdaptiveCtx<'_>, scope: Vec<Action>, budget: &mut RecallBudget) {
    for action in scope {
        if budget.is_full() {
            break;
        }
        let text = action.run(ctx.client, "", "").await.unwrap_or_else(|e| {
            tracing::warn!("scope recall call failed, its records are skipped: {e}");
            String::new()
        });
        budget.add_text(&text);
    }
}

/// `TurnStart`: relevance recall, with the scope set as a fallback when no
/// `SessionStart` ran in this epoch.
pub(super) async fn turn_start(
    ctx: &AdaptiveCtx<'_>,
    scope: Vec<Action>,
) -> anyhow::Result<String> {
    let ledger = ctx.store.load(ctx.session_id).unwrap_or_default();
    let mut budget = RecallBudget::new(RECALL_BUDGET_BYTES, ledger.sent_for(MAIN_AGENT));
    let fallback = !ledger.scope_sent(MAIN_AGENT);
    if fallback {
        fallback_scope(ctx, scope, &mut budget).await;
    }
    let tail = ctx.payload["transcript_path"]
        .as_str()
        .and_then(|p| transcript::last_assistant_text(Path::new(p), TAIL_CHARS));
    let text = relevance::turn_query(&TurnSignals {
        prompt: ctx.payload["prompt"].as_str().unwrap_or_default(),
        activity: ledger.activity(),
        errors: ledger.errors(),
        last_turn_at: ledger.last_turn_at,
        assistant_tail: tail.as_deref(),
    });
    let query_hash = record_hash(&text);
    let repeated = ledger.last_query_hash.as_deref() == Some(query_hash.as_str())
        && !ledger.activity_since(ledger.last_turn_at);
    if !repeated && !text.is_empty() {
        let keywords = relevance::fanout_keywords(ledger.activity());
        for wave_text in waves(ctx.client, &text, &keywords).await {
            budget.add_text(&wave_text);
        }
    }
    let kept = budget.kept_hashes();
    let now = unix_now();
    ctx.store.update(ctx.session_id, |l| {
        l.mark_sent(MAIN_AGENT, kept);
        l.last_query_hash = Some(query_hash);
        l.last_turn_at = now;
        if fallback {
            l.set_scope_sent(MAIN_AGENT);
        }
    });
    Ok(budget.render(Vec::new()))
}

/// The ledger key for the context that fired the hook, and whether hashes may be
/// recorded under it. An unsafe `agent_id` filters as `main` and records nothing.
fn agent_key(payload: &Value) -> (String, bool) {
    match payload["agent_id"].as_str() {
        None => (MAIN_AGENT.to_string(), true),
        Some(id) if crate::paths::is_valid_short_name(id) => (id.to_string(), true),
        Some(_) => (MAIN_AGENT.to_string(), false),
    }
}

/// `PostToolUseFailure`: record the error, then inject related memories once.
pub(super) async fn tool_failure(ctx: &AdaptiveCtx<'_>) -> anyhow::Result<String> {
    let tool = ctx.payload["tool_name"].as_str().unwrap_or_default();
    let error = ctx.payload["error"].as_str().unwrap_or_default();
    let now = unix_now();
    ctx.store
        .update(ctx.session_id, |l| l.push_error(tool, error, now));
    let (key, record) = agent_key(ctx.payload);
    let sent = ctx
        .store
        .load(ctx.session_id)
        .unwrap_or_default()
        .sent_for(&key);
    let text = relevance::error_query(tool, error);
    let resolved = RecallQuery {
        topic: Some("errors-resolved".to_string()),
        project: Some(String::new()),
        ..query(&text, FANOUT_LIMIT)
    };
    let (main, fixes) = tokio::join!(
        recall(ctx.client, Some(query(&text, FAILURE_LIMIT))),
        recall(ctx.client, Some(resolved)),
    );
    let mut budget = RecallBudget::new(FAILURE_BUDGET_BYTES, sent);
    budget.add_text(&fixes);
    budget.add_text(&main);
    if record {
        let kept = budget.kept_hashes();
        ctx.store
            .update(ctx.session_id, |l| l.mark_sent(&key, kept));
    }
    Ok(budget.render(Vec::new()))
}

/// `SubagentStart`: task-relevant memories for a fresh subagent context.
pub(super) async fn subagent_start(ctx: &AdaptiveCtx<'_>) -> anyhow::Result<String> {
    let (key, record) = agent_key(ctx.payload);
    if !record || key == MAIN_AGENT {
        tracing::warn!("SubagentStart without a usable agent_id, injection skipped");
        return Ok(String::new());
    }
    let agent_type = ctx.payload["agent_type"].as_str().unwrap_or_default();
    let now = unix_now();
    let task = ctx
        .store
        .update(ctx.session_id, |l| l.take_subagent(agent_type, now))
        .flatten();
    let ledger = ctx.store.load(ctx.session_id).unwrap_or_default();
    let text = relevance::subagent_query(
        task.as_ref().map(|t| t.task.as_str()),
        agent_type,
        ledger.activity(),
    );
    let mut budget = RecallBudget::new(SUBAGENT_BUDGET_BYTES, ledger.sent_for(&key));
    if !text.is_empty() {
        let keywords = relevance::fanout_keywords(ledger.activity());
        for wave_text in waves(ctx.client, &text, &keywords).await {
            budget.add_text(&wave_text);
        }
    }
    let kept = budget.kept_hashes();
    ctx.store
        .update(ctx.session_id, |l| l.mark_sent(&key, kept));
    Ok(budget.render(Vec::new()))
}

/// Local ledger writes with no MCP call: batch activity and queued subagent tasks.
pub(super) fn record_local(
    event: HookEvent,
    store: &LedgerStore,
    session_id: &str,
    payload: &Value,
) {
    let now = unix_now();
    match event {
        HookEvent::PostToolBatch => {
            let empty = Vec::new();
            let calls = payload["tool_calls"].as_array().unwrap_or(&empty);
            store.update(session_id, |l| {
                for call in calls {
                    let tool = call["tool_name"].as_str().unwrap_or_default();
                    l.push_activity(relevance::activity_from_tool_call(
                        tool,
                        &call["tool_input"],
                        now,
                    ));
                }
            });
        }
        HookEvent::PreToolUse if payload["tool_name"].as_str() == Some("Agent") => {
            let input = &payload["tool_input"];
            let id = payload["tool_use_id"].as_str().unwrap_or_default();
            let kind = input["subagent_type"].as_str().unwrap_or("general-purpose");
            let prompt = input["prompt"].as_str().unwrap_or_default();
            store.update(session_id, |l| l.queue_subagent(id, kind, prompt, now));
        }
        _ => {}
    }
}

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "test code")]
mod tests {
    use std::time::Duration;

    use serde_json::json;
    use wiremock::matchers::{body_string_contains, method};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    fn text(body: &str) -> serde_json::Value {
        json!({"jsonrpc": "2.0", "id": 1,
               "result": {"content": [{"type": "text", "text": body}]}})
    }

    async fn server_with(pairs: &[(&str, &str)]) -> MockServer {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(body_string_contains("initialize"))
            .respond_with(ResponseTemplate::new(200).set_body_json(text("")))
            .mount(&server)
            .await;
        for (needle, body) in pairs {
            Mock::given(method("POST"))
                .and(body_string_contains(*needle))
                .respond_with(ResponseTemplate::new(200).set_body_json(text(body)))
                .mount(&server)
                .await;
        }
        server
    }

    struct Fixture {
        _dir: tempfile::TempDir,
        store: LedgerStore,
        client: McpHttpClient,
    }

    fn fixture(server: &MockServer) -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let store = LedgerStore::new(dir.path());
        let client = McpHttpClient::test_new(server.uri(), Duration::from_secs(2)).unwrap();
        Fixture {
            _dir: dir,
            store,
            client,
        }
    }

    fn ctx<'a>(f: &'a Fixture, payload: &'a serde_json::Value) -> AdaptiveCtx<'a> {
        AdaptiveCtx {
            client: &f.client,
            store: &f.store,
            session_id: "s1",
            payload,
        }
    }

    fn tag_action(tag: &str) -> Action {
        let mut queries = crate::hook_run::tag_recall_queries(&[tag.to_string()]).unwrap();
        Action::RecallTag(queries.remove(0))
    }

    #[tokio::test]
    async fn session_start_sends_scope_once_and_turn_start_does_not_repeat_it() {
        let server = server_with(&[
            ("icm_wake_up", "wake pack"),
            ("llmenv-tag:proj", "[context-p] scope fact"),
            (
                "\"limit\":10",
                "[context-p] scope fact\n[context-p] relevant fact",
            ),
        ])
        .await;
        let f = fixture(&server);
        let start = json!({"source": "startup"});
        let out = session_start(
            &ctx(&f, &start),
            Action::WakeUp(None),
            vec![tag_action("proj")],
        )
        .await
        .unwrap();
        assert!(
            out.contains("wake pack") && out.contains("scope fact"),
            "{out}"
        );
        let turn = json!({"prompt": "work on recall"});
        let out = turn_start(&ctx(&f, &turn), vec![tag_action("proj")])
            .await
            .unwrap();
        assert!(out.contains("relevant fact"), "{out}");
        assert!(!out.contains("scope fact"), "already sent: {out}");
    }

    #[tokio::test]
    async fn compact_resets_and_resume_keeps_the_ledger() {
        let server = server_with(&[
            ("icm_wake_up", ""),
            ("llmenv-tag:proj", "[context-p] scope fact"),
        ])
        .await;
        let f = fixture(&server);
        let mut outputs = Vec::new();
        for source in ["startup", "resume", "compact"] {
            let payload = json!({ "source": source });
            let out = session_start(
                &ctx(&f, &payload),
                Action::WakeUp(None),
                vec![tag_action("proj")],
            )
            .await
            .unwrap();
            outputs.push(out.contains("scope fact"));
        }
        assert_eq!(
            outputs,
            [true, false, true],
            "startup sends, resume keeps, compact resets"
        );
    }

    #[tokio::test]
    async fn turn_start_falls_back_to_the_scope_set_without_session_start() {
        let server = server_with(&[("llmenv-tag:proj", "[context-p] scope fact")]).await;
        let f = fixture(&server);
        let turn = json!({"prompt": "hi"});
        let out = turn_start(&ctx(&f, &turn), vec![tag_action("proj")])
            .await
            .unwrap();
        assert!(out.contains("scope fact"), "{out}");
        assert!(f.store.load("s1").unwrap().scope_sent(MAIN_AGENT));
    }

    #[tokio::test]
    async fn a_repeated_query_with_no_new_activity_makes_no_relevance_calls() {
        let server = server_with(&[("\"limit\":10", "[context-p] fact")]).await;
        let f = fixture(&server);
        f.store.update("s1", |l| l.set_scope_sent(MAIN_AGENT));
        let turn = json!({"prompt": "same"});
        turn_start(&ctx(&f, &turn), vec![]).await.unwrap();
        let calls_before = server.received_requests().await.unwrap().len();
        turn_start(&ctx(&f, &turn), vec![]).await.unwrap();
        assert_eq!(
            server.received_requests().await.unwrap().len(),
            calls_before
        );
    }

    #[tokio::test]
    async fn topic_fanout_recalls_sibling_topics() {
        let server = server_with(&[
            ("\"limit\":10", "[context-llmenv] main fact"),
            ("decisions-llmenv", "[decisions-llmenv] sibling fact"),
        ])
        .await;
        let f = fixture(&server);
        f.store.update("s1", |l| l.set_scope_sent(MAIN_AGENT));
        let turn = json!({"prompt": "why"});
        let out = turn_start(&ctx(&f, &turn), vec![]).await.unwrap();
        assert!(
            out.contains("main fact") && out.contains("sibling fact"),
            "{out}"
        );
    }

    #[tokio::test]
    async fn one_failed_call_keeps_the_others() {
        let server = server_with(&[("\"limit\":10", "[context-p] kept fact")]).await;
        Mock::given(method("POST"))
            .and(body_string_contains("\"project\":\"\""))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;
        let f = fixture(&server);
        f.store.update("s1", |l| l.set_scope_sent(MAIN_AGENT));
        let turn = json!({"prompt": "x"});
        let out = turn_start(&ctx(&f, &turn), vec![]).await.unwrap();
        assert!(out.contains("kept fact"), "{out}");
    }

    #[tokio::test]
    async fn tool_failure_injects_errors_once() {
        let server = server_with(&[("errors-resolved", "[errors-resolved] fix: pin mcp<2")]).await;
        let f = fixture(&server);
        let payload = json!({"tool_name": "Bash", "error": "Exit code 1\nImportError request_ctx"});
        let first = tool_failure(&ctx(&f, &payload)).await.unwrap();
        assert!(first.contains("pin mcp<2"), "{first}");
        assert_eq!(f.store.load("s1").unwrap().errors().len(), 1);
        let second = tool_failure(&ctx(&f, &payload)).await.unwrap();
        assert!(!second.contains("pin mcp<2"), "already sent: {second}");
    }

    #[tokio::test]
    async fn invalid_agent_id_records_nothing() {
        let server = server_with(&[("errors-resolved", "[errors-resolved] fix")]).await;
        let f = fixture(&server);
        let payload = json!({"tool_name": "Bash", "error": "boom", "agent_id": "../evil"});
        let out = tool_failure(&ctx(&f, &payload)).await.unwrap();
        assert!(out.contains("fix"));
        assert!(f.store.load("s1").unwrap().sent_for(MAIN_AGENT).is_empty());
    }

    #[tokio::test]
    async fn subagent_gets_its_task_memories_without_touching_the_parent() {
        let server = server_with(&[(
            "map the recall path",
            "[context-p] recall lives in hook_run",
        )])
        .await;
        let f = fixture(&server);
        f.store.update("s1", |l| {
            l.mark_sent(
                MAIN_AGENT,
                [record_hash("[context-p] recall lives in hook_run")],
            );
        });
        record_local(
            HookEvent::PreToolUse,
            &f.store,
            "s1",
            &json!({"tool_name": "Agent", "tool_use_id": "u1",
                    "tool_input": {"subagent_type": "Explore", "prompt": "map the recall path"}}),
        );
        let payload = json!({"agent_id": "agent-1", "agent_type": "Explore"});
        let out = subagent_start(&ctx(&f, &payload)).await.unwrap();
        assert!(out.contains("recall lives in hook_run"), "{out}");
        let ledger = f.store.load("s1").unwrap();
        assert_eq!(ledger.sent_for(MAIN_AGENT).len(), 1, "parent unchanged");
        assert_eq!(ledger.sent_for("agent-1").len(), 1);
    }

    #[test]
    fn record_local_appends_batch_activity() {
        let dir = tempfile::tempdir().unwrap();
        let store = LedgerStore::new(dir.path());
        record_local(
            HookEvent::PostToolBatch,
            &store,
            "s1",
            &json!({"tool_calls": [
                {"tool_name": "Read", "tool_input": {"file_path": "/r/src/a.rs"}},
                {"tool_name": "Bash", "tool_input": {"command": "cargo test"}}
            ]}),
        );
        let targets: Vec<_> = store
            .load("s1")
            .unwrap()
            .activity()
            .iter()
            .map(|a| a.target.clone().unwrap())
            .collect();
        assert_eq!(targets, ["/r/src/a.rs", "cargo"]);
    }
}
