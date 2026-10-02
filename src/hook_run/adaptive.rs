//! Adaptive recall flows for the lifecycle hooks (#2249).
//!
//! Design: docs/superpowers/specs/2026-09-27-adaptive-icm-recall-design.md

use std::path::Path;
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::hook_run::HookEvent;
use crate::hook_run::action::{Action, RecallQuery, split_recall_records};
use crate::hook_run::recall::{RECALL_BUDGET_BYTES, RecallBudget};
use crate::hook_run::relevance::{self, TurnSignals};
use crate::hook_run::session_ledger::{LedgerStore, MAIN_AGENT, record_hash};
use crate::hook_run::session_state::unix_now;
use crate::hook_run::transcript;
use llmenv_mcp::mcp_client::McpHttpClient;

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

/// A recall across all projects. ICM's default project filter is the ICM
/// server's own cwd, which says nothing about this session when ICM runs remotely.
fn query(text: &str, limit: u8) -> RecallQuery {
    RecallQuery {
        query: text.to_string(),
        topic: None,
        keyword: None,
        project: Some(String::new()),
        limit,
    }
}

/// What a set of recall calls returned.
#[derive(Debug, Default)]
struct Outcome {
    texts: Vec<String>,
    calls: usize,
    failed: usize,
}

impl Outcome {
    /// Record one call. `None` means no call ran, so nothing is counted.
    fn push(&mut self, result: Option<anyhow::Result<String>>) {
        let Some(result) = result else {
            return;
        };
        self.calls += 1;
        match result {
            Ok(text) => self.texts.push(text),
            Err(e) => {
                self.failed += 1;
                tracing::warn!("adaptive recall call failed, its records are skipped: {e}");
            }
        }
    }

    fn merge(&mut self, other: Outcome) {
        self.texts.extend(other.texts);
        self.calls += other.calls;
        self.failed += other.failed;
    }

    fn all_failed(&self) -> bool {
        self.calls > 0 && self.failed == self.calls
    }
}

/// The stderr line for a flow whose every ICM call failed. A single failed call
/// only costs its records, but a dead backend must be visible, as it is on the
/// stateless path.
fn outage_notice(flow: &str, outcome: &Outcome) -> Option<String> {
    outcome.all_failed().then(|| {
        format!(
            "llmenv: memory {flow} recall skipped: all {} ICM calls failed",
            outcome.calls
        )
    })
}

/// Print the outage line and the `[LLMENV_CONTEXT]` trace line for one flow.
fn report(flow: &str, outcome: &Outcome, budget: &RecallBudget) {
    if let Some(line) = outage_notice(flow, outcome) {
        eprintln!("{line}");
    }
    // Same env var that gates hook-run's other stderr telemetry (#1261).
    let tracing_enabled = std::env::var_os("LLMENV_TRACE_TIMING").is_some();
    if let Some(line) = budget.trace_line(tracing_enabled) {
        eprintln!("{line}");
    }
}

async fn recall(client: &McpHttpClient, q: Option<RecallQuery>) -> Option<anyhow::Result<String>> {
    Some(Action::RecallQuery(q?).run(client, "", "").await)
}

/// The recall texts for `text`, in budget order: the session project's main
/// recall, the all-projects main recall, keyword fanout, topic fanout. The flag
/// says whether a main recall succeeded.
async fn waves(
    client: &McpHttpClient,
    text: &str,
    keywords: &[String],
    project: Option<&str>,
) -> (Outcome, bool) {
    let scoped = project.map(|p| RecallQuery {
        project: Some(p.to_string()),
        ..query(text, MAIN_LIMIT)
    });
    let keyword = |i: usize| {
        keywords.get(i).map(|k| RecallQuery {
            keyword: Some(k.clone()),
            ..query(text, FANOUT_LIMIT)
        })
    };
    let start = Instant::now();
    let (own, main, kw0, kw1) = tokio::join!(
        recall(client, scoped),
        recall(client, Some(query(text, MAIN_LIMIT))),
        recall(client, keyword(0)),
        recall(client, keyword(1)),
    );
    let main_ok = matches!(own, Some(Ok(_))) || matches!(main, Some(Ok(_)));
    let mut outcome = Outcome::default();
    [own, main, kw0, kw1]
        .into_iter()
        .for_each(|r| outcome.push(r));
    if start.elapsed() < WAVE2_CUTOFF {
        let topics: Vec<String> = outcome
            .texts
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
        outcome.push(t0);
        outcome.push(t1);
    }
    (outcome, main_ok)
}

fn resets_ledger(source: Option<&str>) -> bool {
    super::session_state::context_was_lost(source)
}

/// Run the scope recalls into `budget`, one after another, until it is full.
async fn run_scope(
    ctx: &AdaptiveCtx<'_>,
    scope: Vec<Action>,
    budget: &mut RecallBudget,
) -> Outcome {
    let mut outcome = Outcome::default();
    for action in scope {
        if budget.is_full() {
            break;
        }
        outcome.push(Some(action.run(ctx.client, "", "").await));
        if let Some(text) = outcome.texts.last() {
            budget.add_text(text);
        }
        outcome.texts.clear();
    }
    outcome
}

/// `SessionStart`: reset per `source`, then send the wake-up pack and the scope set.
/// A continued session (resume, fork) gets no wake-up pack (#2142).
pub(super) async fn session_start(
    ctx: &AdaptiveCtx<'_>,
    wake: Action,
    scope: Vec<Action>,
) -> anyhow::Result<String> {
    let reset = resets_ledger(ctx.payload["source"].as_str());
    let project = match &wake {
        Action::WakeUp(args) => args.project.clone(),
        _ => None,
    };
    // One locked update, so a busy lock can never leave a pre-reset sent set in use.
    let sent = ctx
        .store
        .update(ctx.session_id, |l| {
            if reset {
                l.reset();
            }
            if project.is_some() {
                l.project = project;
            }
            l.sent_for(MAIN_AGENT)
        })
        .unwrap_or_default();
    let wake = if super::continues_session(ctx.payload) {
        Ok(String::new())
    } else {
        wake.run(ctx.client, "", "").await
    };
    // Claude Code spills output over about 10 KB to a file, so the scope set gets
    // only the room that the wake-up pack leaves.
    let wake_bytes = wake.as_ref().map_or(0, |t| t.len() + 2);
    let mut budget = RecallBudget::new(RECALL_BUDGET_BYTES.saturating_sub(wake_bytes), sent);
    let scope_len = scope.len();
    let outcome = run_scope(ctx, scope, &mut budget).await;
    report("session_start", &outcome, &budget);
    let wake = match wake {
        Ok(text) => text,
        Err(e) if outcome.all_failed() || scope_len == 0 => return Err(e),
        Err(e) => {
            // eprintln, as in `report`: the default tracing filter is ERROR-only.
            eprintln!(
                "llmenv: memory wake-up skipped: icm_wake_up failed ({e}); \
                 the scope set still goes out"
            );
            String::new()
        }
    };
    let complete = outcome.failed == 0;
    let kept = budget.kept_hashes();
    ctx.store.update(ctx.session_id, |l| {
        l.mark_sent(MAIN_AGENT, kept);
        if complete {
            l.set_scope_sent(MAIN_AGENT);
        }
    });
    let passthrough = if wake.is_empty() {
        Vec::new()
    } else {
        vec![wake]
    };
    Ok(budget.render(passthrough))
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
    let mut outcome = Outcome::default();
    if fallback {
        outcome = run_scope(ctx, scope, &mut budget).await;
    }
    let scope_complete = fallback && outcome.failed == 0;
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
    let mut main_ok = false;
    if !repeated && !text.is_empty() {
        let keywords = relevance::fanout_keywords(ledger.activity());
        let (wave, ok) = waves(ctx.client, &text, &keywords, ledger.project.as_deref()).await;
        wave.texts.iter().for_each(|t| budget.add_text(t));
        main_ok = ok;
        outcome.merge(wave);
    }
    report("turn_start", &outcome, &budget);
    let kept = budget.kept_hashes();
    let now = unix_now();
    ctx.store.update(ctx.session_id, |l| {
        l.mark_sent(MAIN_AGENT, kept);
        l.last_turn_at = now;
        // A failed recall is not a finished query; the skip rule must not reuse it.
        if main_ok {
            l.last_query_hash = Some(query_hash);
        }
        if scope_complete {
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
        ..query(&text, FANOUT_LIMIT)
    };
    let (main, fixes) = tokio::join!(
        recall(ctx.client, Some(query(&text, FAILURE_LIMIT))),
        recall(ctx.client, Some(resolved)),
    );
    let mut outcome = Outcome::default();
    outcome.push(fixes);
    outcome.push(main);
    let mut budget = RecallBudget::new(FAILURE_BUDGET_BYTES, sent);
    outcome.texts.iter().for_each(|t| budget.add_text(t));
    report("post_tool_use_failure", &outcome, &budget);
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
    // A resumed subagent already has an entry; its task was taken at launch, and
    // the queue now holds tasks for new siblings only.
    let task = ctx
        .store
        .update(ctx.session_id, |l| {
            if l.knows_agent(&key) {
                None
            } else {
                l.take_subagent(agent_type, now)
            }
        })
        .flatten();
    let ledger = ctx.store.load(ctx.session_id).unwrap_or_default();
    let text = relevance::subagent_query(
        task.as_ref().map(|t| t.task.as_str()),
        agent_type,
        ledger.activity(),
    );
    let mut budget = RecallBudget::new(SUBAGENT_BUDGET_BYTES, ledger.sent_for(&key));
    let mut outcome = Outcome::default();
    if !text.is_empty() {
        let keywords = relevance::fanout_keywords(ledger.activity());
        (outcome, _) = waves(ctx.client, &text, &keywords, ledger.project.as_deref()).await;
        outcome.texts.iter().for_each(|t| budget.add_text(t));
    }
    report("subagent_start", &outcome, &budget);
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
                    if tool == "Agent"
                        && let Some(id) = call["tool_use_id"].as_str()
                    {
                        l.drop_subagent(id);
                    }
                    l.push_activity(relevance::activity_from_tool_call(
                        tool,
                        &call["tool_input"],
                        now,
                    ));
                }
            });
        }
        HookEvent::SubagentTask if payload["tool_name"].as_str() == Some("Agent") => {
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
#[expect(clippy::unwrap_used, clippy::expect_used, reason = "test code")]
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
            Action::WakeUp(crate::hook_run::action::WakeUpArgs::default()),
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
                Action::WakeUp(crate::hook_run::action::WakeUpArgs::default()),
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

    // #2142: a resumed or forked session already holds the earlier wake-up pack.
    #[tokio::test]
    async fn resume_and_fork_skip_the_wake_up_call() {
        for source in ["resume", "fork"] {
            let server = server_with(&[("llmenv-tag:proj", "[context-p] scope fact")]).await;
            Mock::given(method("POST"))
                .and(body_string_contains("icm_wake_up"))
                .respond_with(ResponseTemplate::new(200).set_body_json(text("wake pack")))
                .expect(0)
                .mount(&server)
                .await;
            let f = fixture(&server);
            let payload = json!({ "source": source });
            let out = session_start(
                &ctx(&f, &payload),
                Action::WakeUp(crate::hook_run::action::WakeUpArgs::default()),
                vec![tag_action("proj")],
            )
            .await
            .unwrap();
            assert!(!out.contains("wake pack"), "{source}: {out}");
            server.verify().await;
        }
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
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(body_string_contains("decisions-p"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;
        for (needle, body) in [
            ("initialize", ""),
            ("\"limit\":10", "[context-p] kept fact"),
        ] {
            Mock::given(method("POST"))
                .and(body_string_contains(needle))
                .respond_with(ResponseTemplate::new(200).set_body_json(text(body)))
                .mount(&server)
                .await;
        }
        let f = fixture(&server);
        f.store.update("s1", |l| l.set_scope_sent(MAIN_AGENT));
        let turn = json!({"prompt": "x"});
        let out = turn_start(&ctx(&f, &turn), vec![]).await.unwrap();
        assert!(out.contains("kept fact"), "{out}");
    }

    #[tokio::test]
    async fn every_adaptive_recall_searches_all_projects() {
        // The default project filter is the ICM server's own cwd, which says
        // nothing about this session when ICM runs remotely (AGENTS.md).
        let server = server_with(&[("\"limit\":10", "[context-p] fact")]).await;
        let f = fixture(&server);
        f.store.update("s1", |l| {
            l.set_scope_sent(MAIN_AGENT);
            l.push_activity(relevance::activity_from_tool_call(
                "Bash",
                &json!({"command": "cargo test"}),
                1,
            ));
        });
        turn_start(&ctx(&f, &json!({"prompt": "why"})), vec![])
            .await
            .unwrap();
        tool_failure(&ctx(&f, &json!({"tool_name": "Bash", "error": "boom"})))
            .await
            .unwrap();
        let recalls: Vec<String> = server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .map(|r| String::from_utf8_lossy(&r.body).into_owned())
            .filter(|b| b.contains("icm_memory_recall"))
            .collect();
        assert!(recalls.len() >= 4, "{recalls:?}");
        for body in &recalls {
            assert!(body.contains("\"project\":\"\""), "{body}");
        }
    }

    async fn failing_server() -> MockServer {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(body_string_contains("initialize"))
            .respond_with(ResponseTemplate::new(200).set_body_json(text("")))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;
        server
    }

    #[tokio::test]
    async fn the_fallback_is_retried_after_every_scope_call_failed() {
        let server = failing_server().await;
        let f = fixture(&server);
        turn_start(&ctx(&f, &json!({"prompt": "hi"})), vec![tag_action("proj")])
            .await
            .unwrap();
        let ledger = f.store.load("s1").unwrap();
        assert!(
            !ledger.scope_sent(MAIN_AGENT),
            "a failed fallback must run again"
        );
        assert_eq!(
            ledger.last_query_hash, None,
            "a failed recall is not a done query"
        );
    }

    #[tokio::test]
    async fn waves_count_every_failed_call() {
        let server = failing_server().await;
        let f = fixture(&server);
        let (outcome, main_ok) = waves(&f.client, "q", &["k".to_string()], None).await;
        assert!(!main_ok);
        assert!(outcome.all_failed(), "{outcome:?}");
        assert_eq!(outcome.calls, 2);
    }

    #[test]
    fn outage_notice_only_when_every_call_failed() {
        let mut outcome = Outcome::default();
        assert_eq!(outage_notice("turn_start", &outcome), None);
        outcome.calls = 3;
        outcome.failed = 2;
        assert_eq!(outage_notice("turn_start", &outcome), None);
        outcome.failed = 3;
        assert_eq!(
            outage_notice("turn_start", &outcome).as_deref(),
            Some("llmenv: memory turn_start recall skipped: all 3 ICM calls failed")
        );
    }

    #[tokio::test]
    async fn session_start_keeps_the_wake_pack_when_a_scope_call_fails() {
        let server = MockServer::start().await;
        for (needle, body) in [("initialize", ""), ("icm_wake_up", "wake pack")] {
            Mock::given(method("POST"))
                .and(body_string_contains(needle))
                .respond_with(ResponseTemplate::new(200).set_body_json(text(body)))
                .mount(&server)
                .await;
        }
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;
        let f = fixture(&server);
        let start = json!({"source": "startup"});
        let out = session_start(
            &ctx(&f, &start),
            Action::WakeUp(crate::hook_run::action::WakeUpArgs::default()),
            vec![tag_action("proj")],
        )
        .await
        .unwrap();
        assert!(out.contains("wake pack"), "{out}");
        assert!(!f.store.load("s1").unwrap().scope_sent(MAIN_AGENT));
    }

    #[tokio::test]
    async fn session_start_scope_budget_leaves_room_for_the_wake_pack() {
        let wake = "w".repeat(7_000);
        let scope: String = (0..10)
            .map(|i| format!("[context-p] scope fact {i} {}", "x".repeat(300)))
            .collect::<Vec<_>>()
            .join("\n");
        let server = server_with(&[("icm_wake_up", &wake), ("llmenv-tag:proj", &scope)]).await;
        let f = fixture(&server);
        let start = json!({"source": "startup"});
        let out = session_start(
            &ctx(&f, &start),
            Action::WakeUp(crate::hook_run::action::WakeUpArgs::default()),
            vec![tag_action("proj")],
        )
        .await
        .unwrap();
        assert!(out.len() <= RECALL_BUDGET_BYTES + 200, "{}", out.len());
        let sent = f.store.load("s1").unwrap().sent_for(MAIN_AGENT).len();
        assert!(sent < 10, "only records that fit are marked sent: {sent}");
    }

    #[tokio::test]
    async fn the_wake_pack_takes_its_own_length_from_the_scope_budget() {
        let wake = "w".repeat(3_000);
        let scope = format!("[context-p] big scope fact {}", "x".repeat(3_000));
        let server = server_with(&[("icm_wake_up", &wake), ("llmenv-tag:proj", &scope)]).await;
        let f = fixture(&server);
        let out = session_start(
            &ctx(&f, &json!({"source": "startup"})),
            Action::WakeUp(crate::hook_run::action::WakeUpArgs::default()),
            vec![tag_action("proj")],
        )
        .await
        .unwrap();
        assert!(
            out.contains("big scope fact"),
            "the record fits: {}",
            out.len()
        );
    }

    async fn start_with_a_failed_wake(
        scope_body: Option<&str>,
        scope: Vec<Action>,
    ) -> anyhow::Result<String> {
        let pairs: Vec<(&str, &str)> = scope_body
            .map(|b| ("llmenv-tag:proj", b))
            .into_iter()
            .collect();
        let server = server_with(&pairs).await;
        let f = fixture(&server);
        session_start(
            &ctx(&f, &json!({"source": "startup"})),
            Action::WakeUp(crate::hook_run::action::WakeUpArgs::default()),
            scope,
        )
        .await
    }

    #[tokio::test]
    async fn a_failed_wake_with_a_working_scope_still_sends_the_scope() {
        let out =
            start_with_a_failed_wake(Some("[context-p] scope fact"), vec![tag_action("proj")])
                .await
                .unwrap();
        assert!(out.contains("scope fact"), "{out}");
    }

    #[tokio::test]
    async fn a_failed_wake_is_an_error_when_the_scope_also_fails() {
        assert!(
            start_with_a_failed_wake(None, vec![tag_action("proj")])
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn a_failed_wake_is_an_error_when_there_is_no_scope() {
        assert!(start_with_a_failed_wake(None, Vec::new()).await.is_err());
    }

    #[tokio::test]
    async fn the_session_project_ranks_first_after_session_start() {
        let server = server_with(&[
            ("\"project\":\"llmenv\"", "[decisions-llmenv] project fact"),
            ("\"limit\":10", "[preferences] global fact"),
        ])
        .await;
        let f = fixture(&server);
        let wake = Action::WakeUp(crate::hook_run::action::WakeUpArgs {
            max_tokens: None,
            project: Some("llmenv".into()),
        });
        session_start(&ctx(&f, &json!({"source": "startup"})), wake, vec![])
            .await
            .unwrap();
        assert_eq!(
            f.store.load("s1").unwrap().project.as_deref(),
            Some("llmenv")
        );
        let out = turn_start(&ctx(&f, &json!({"prompt": "why"})), vec![])
            .await
            .unwrap();
        let project_at = out.find("project fact").expect("project-scoped record");
        let global_at = out.find("global fact").expect("all-projects record");
        assert!(project_at < global_at, "{out}");
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
            HookEvent::SubagentTask,
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

    #[tokio::test]
    async fn a_resumed_subagent_leaves_the_queue_alone() {
        let server = server_with(&[("\"limit\":10", "")]).await;
        let f = fixture(&server);
        f.store
            .update("s1", |l| l.mark_sent("agent-1", ["h".to_string()]));
        record_local(
            HookEvent::SubagentTask,
            &f.store,
            "s1",
            &json!({"tool_name": "Agent", "tool_use_id": "u2",
                    "tool_input": {"subagent_type": "Explore", "prompt": "new sibling task"}}),
        );
        let resumed = json!({"agent_id": "agent-1", "agent_type": "Explore"});
        subagent_start(&ctx(&f, &resumed)).await.unwrap();
        let task = f
            .store
            .update("s1", |l| l.take_subagent("Explore", unix_now()))
            .flatten();
        assert_eq!(task.map(|t| t.task).as_deref(), Some("new sibling task"));
    }

    #[test]
    fn a_finished_agent_call_leaves_the_queue() {
        let dir = tempfile::tempdir().unwrap();
        let store = LedgerStore::new(dir.path());
        record_local(
            HookEvent::SubagentTask,
            &store,
            "s1",
            &json!({"tool_name": "Agent", "tool_use_id": "u1",
                    "tool_input": {"subagent_type": "Explore", "prompt": "denied task"}}),
        );
        record_local(
            HookEvent::PostToolBatch,
            &store,
            "s1",
            &json!({"tool_calls": [{"tool_name": "Agent", "tool_use_id": "u1", "tool_input": {}}]}),
        );
        let task = store
            .update("s1", |l| l.take_subagent("Explore", unix_now()))
            .flatten();
        assert!(task.is_none());
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

    #[test]
    fn record_local_queues_only_agent_tool_tasks() {
        let dir = tempfile::tempdir().unwrap();
        let store = LedgerStore::new(dir.path());
        let task = |tool: &str, id: &str| {
            json!({"tool_name": tool, "tool_use_id": id,
                   "tool_input": {"subagent_type": "Explore", "prompt": "find it"}})
        };
        record_local(HookEvent::SubagentTask, &store, "s1", &task("Bash", "t1"));
        let mut ledger = store.load("s1").unwrap();
        assert!(
            ledger.take_subagent("Explore", unix_now()).is_none(),
            "a non-Agent tool queues nothing"
        );
        record_local(HookEvent::SubagentTask, &store, "s1", &task("Agent", "t2"));
        let mut ledger = store.load("s1").unwrap();
        assert_eq!(
            ledger.take_subagent("Explore", unix_now()).unwrap().task,
            "find it"
        );
    }

    #[test]
    fn outcome_merge_adds_counts_and_texts() {
        let mut a = Outcome::default();
        a.push(Some(Ok("one".to_string())));
        a.push(Some(Err(anyhow::anyhow!("down"))));
        let mut b = Outcome::default();
        b.push(Some(Ok("two".to_string())));
        b.push(Some(Err(anyhow::anyhow!("down"))));
        b.push(Some(Err(anyhow::anyhow!("down"))));
        a.merge(b);
        assert_eq!((a.calls, a.failed), (5, 3));
        assert_eq!(a.texts, ["one", "two"]);
    }

    #[test]
    fn agent_key_refuses_an_unsafe_agent_id() {
        assert_eq!(agent_key(&json!({})), (MAIN_AGENT.to_string(), true));
        assert_eq!(
            agent_key(&json!({"agent_id": "a1"})),
            ("a1".to_string(), true)
        );
        assert_eq!(
            agent_key(&json!({"agent_id": "../x"})),
            (MAIN_AGENT.to_string(), false)
        );
    }

    #[tokio::test]
    async fn subagent_start_without_an_agent_id_does_nothing() {
        let server = server_with(&[]).await;
        let f = fixture(&server);
        let payload = json!({"agent_type": "Explore"});
        let out = subagent_start(&ctx(&f, &payload)).await.unwrap();
        assert!(out.is_empty(), "{out}");
        assert!(
            !f.store.load("s1").unwrap().knows_agent(MAIN_AGENT),
            "no ledger write"
        );
    }

    #[tokio::test]
    async fn turn_start_sends_activity_terms_as_keyword_filters() {
        let server = server_with(&[("\"keyword\":\"recall\"", "[k] keyword fact")]).await;
        let f = fixture(&server);
        f.store.update("s1", |l| {
            l.set_scope_sent(MAIN_AGENT);
            l.push_activity(crate::hook_run::session_ledger::Activity {
                tool: "Read".to_string(),
                target: Some("/r/src/hook_run/recall.rs".to_string()),
                at: 1,
            });
        });
        let payload = json!({"prompt": "work on it"});
        let out = turn_start(&ctx(&f, &payload), Vec::new()).await.unwrap();
        assert!(out.contains("keyword fact"), "{out}");
    }
}
