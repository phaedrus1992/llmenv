//! Adaptive recall flows for the lifecycle hooks (#2249).
//!
//! Design: docs/superpowers/specs/2026-09-27-adaptive-icm-recall-design.md

use std::path::Path;
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::agent_notice;
use crate::hook_run::HookEvent;
use crate::hook_run::action::{Action, RecallQuery, split_recall_records};
use crate::hook_run::recall::{RECALL_BUDGET_BYTES, RecallBudget};
use crate::hook_run::relevance::{self, TurnSignals};
use crate::hook_run::session_ledger::{Ledger, LedgerStore, MAIN_AGENT, record_hash};
use crate::hook_run::session_state::unix_now;
use crate::hook_run::transcript;
use llmenv_mcp::mcp_client::McpHttpClient;

mod cut;
mod session_reset;
mod subagent;
#[cfg(test)]
use cut::cut_items_notice;
use cut::{drop_cut_items, join_sections, note_cut_items, pack_bytes};
use session_reset::{FirstUpdate, first_update_notice, resets_ledger};
use subagent::agent_key;
pub(super) use subagent::subagent_start;

const FAILURE_BUDGET_BYTES: usize = 2_000;
const SUBAGENT_BUDGET_BYTES: usize = 4_000;
const MAIN_LIMIT: u8 = 10;
const FANOUT_LIMIT: u8 = 3;
const FAILURE_LIMIT: u8 = 5;
const TAIL_CHARS: usize = 300;
/// Wave 2 would push a slow backend past the latency that a prompt tolerates.
const WAVE2_CUTOFF: Duration = Duration::from_millis(1_500);
/// The mark icm puts at the end of a record it cut short.
const CUT_MARKER: &str = "[…]";
/// The topic prefix of a scope-context record. The scope set already sends these.
const SCOPE_RECORD: &str = "[llmenv-scope-context]";

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
                tracing::error!("adaptive recall call failed, its records are skipped: {e}");
                agent_notice::record(format!(
                    "llmenv: a memory recall call failed, so its records were not sent: {e:#}. \
                     Fix: run `llmenv doctor` to check the memory server."
                ));
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

/// The notice for a flow whose every ICM call failed. A single failed call
/// only costs its records, but a dead backend must be visible, as it is on the
/// stateless path.
fn outage_notice(flow: &str, outcome: &Outcome) -> Option<String> {
    outcome.all_failed().then(|| {
        format!(
            "llmenv: memory {flow} recall skipped: all {} ICM calls failed. Fix: run `llmenv \
             doctor` to check the memory server.",
            outcome.calls
        )
    })
}

/// Print the outage line and the `[LLMENV_CONTEXT]` trace line for one flow.
fn report(flow: &str, outcome: &Outcome, budget: &RecallBudget) {
    // The engine drops a hook's stderr, so the outage goes to the agent as a notice.
    if let Some(line) = outage_notice(flow, outcome) {
        agent_notice::record_logged(line);
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

/// The session project's own memories for the wake-up pack. The icm wake-up fills its budget
/// with global records first, so this project-scoped recall runs beside it (#2756).
async fn project_memories(client: &McpHttpClient, project: &str) -> String {
    let q = RecallQuery {
        project: Some(project.to_string()),
        ..query(project, MAIN_LIMIT)
    };
    match Action::RecallQuery(q).run(client, "", "").await {
        Ok(text) => {
            let records: Vec<String> = split_recall_records(&text)
                .into_iter()
                .filter(|r| !r.starts_with(SCOPE_RECORD))
                .collect();
            let (kept, cut) = drop_cut_items(&records.join("\n"));
            note_cut_items(cut);
            kept
        }
        Err(e) => {
            tracing::error!("session start: project memories not recalled for {project}: {e:#}");
            agent_notice::record(format!(
                "llmenv: project memories were skipped at session start: icm recall failed ({}). \
                 Fix: run `llmenv doctor` to check the memory server.",
                llmenv_mcp::stdio_rpc::tidy_reason(&format!("{e:#}"))
            ));
            String::new()
        }
    }
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

/// Notice for a context reset that the ledger did not apply. Records seen before the
/// reset stay filtered from recall, so the agent must hear about it.
const RESET_UNDONE: &str = "llmenv: the recall ledger was not reset after a context reset (the \
     state file was locked or unreadable). Records seen before the reset may stay filtered from \
     recall. Fix: run `llmenv doctor` to check the state directory.";

/// The notice for scope recalls that the output budget cut off, or `None` when none were cut
/// (#2766). The recalls are unknown to the agent, not empty, so the notice says so.
fn scope_cut_notice(skipped: usize) -> Option<String> {
    (skipped > 0).then(|| {
        format!(
            "llmenv: {skipped} recall action(s) were skipped because the output budget was full. \
             Those recalls are unknown, not empty. Fix: store shorter memories, so more recalls \
             fit the budget."
        )
    })
}

/// Run the scope recalls into `budget`, one after another, until it is full.
async fn run_scope(
    ctx: &AdaptiveCtx<'_>,
    scope: Vec<Action>,
    budget: &mut RecallBudget,
) -> Outcome {
    let mut outcome = Outcome::default();
    let mut actions = scope.into_iter();
    while !budget.is_full() {
        let Some(action) = actions.next() else {
            break;
        };
        outcome.push(Some(action.run(ctx.client, "", "").await));
        if let Some(text) = outcome.texts.last() {
            budget.add_text(text);
        }
        outcome.texts.clear();
    }
    if let Some(notice) = scope_cut_notice(actions.len()) {
        agent_notice::record(notice);
    }
    outcome
}

/// `SessionStart`: reset per `source`, then send the wake-up pack and the scope set.
/// A continued session (resume, fork) gets no wake-up pack (#2142).
pub(super) async fn session_start(
    ctx: &AdaptiveCtx<'_>,
    wake: Action,
    scope: Vec<Action>,
) -> anyhow::Result<super::HookText> {
    let reset = resets_ledger(ctx.payload["source"].as_str());
    let project = match &wake {
        Action::WakeUp(args) => args.project.clone(),
        _ => None,
    };
    let recall_project = project.clone();
    // One locked update, so a busy lock can never leave a pre-reset sent set in use.
    let resume = super::resume_cache::ResumeCache::from_payload(ctx.payload);
    let updated = ctx.store.update_reporting_save(ctx.session_id, |l| {
        l.resume_cache = Some(resume);
        if reset {
            l.reset();
        }
        if project.is_some() {
            l.project = project;
        }
        l.sent_for(MAIN_AGENT)
    });
    if let Some(notice) = first_update_notice(FirstUpdate::of(updated.as_ref()), reset) {
        tracing::error!("session start ledger update failed (reset requested: {reset}): {notice}");
        agent_notice::record(notice);
    }
    let sent = updated.map(|(sent, _)| sent).unwrap_or_default();
    // A resumed session already holds its pack, so it makes neither call (#2142). A failed
    // wake-up call must not discard the project memories that did load, so they are kept on the
    // error path below (#2756).
    let (wake, own) = if super::continues_session(ctx.payload) {
        (Ok(String::new()), String::new())
    } else {
        let own = match &recall_project {
            Some(p) => project_memories(ctx.client, p).await,
            None => String::new(),
        };
        let wake = wake.run(ctx.client, "", "").await.map(|t| {
            let (kept, cut) = drop_cut_items(&t);
            note_cut_items(cut);
            join_sections(&kept, &own)
        });
        (wake, own)
    };
    // Claude Code spills output over about 10 KB to a file, so the scope set gets
    // only the room that the wake-up pack leaves.
    let wake_bytes = pack_bytes(&wake, &own);
    let mut budget = RecallBudget::new(RECALL_BUDGET_BYTES.saturating_sub(wake_bytes), sent);
    let scope_len = scope.len();
    let outcome = run_scope(ctx, scope, &mut budget).await;
    report("session_start", &outcome, &budget);
    let wake = match wake {
        Ok(text) => text,
        Err(e) if (outcome.all_failed() || scope_len == 0) && own.is_empty() => return Err(e),
        Err(e) => {
            // The agent is told: stderr from a hook never reaches it.
            tracing::error!(
                "session start: icm_wake_up failed, the wake-up pack is skipped: {e:#}"
            );
            agent_notice::record(format!(
                "llmenv: memory wake-up skipped: icm_wake_up failed ({}); the scope set still \
                 goes out. Fix: run `llmenv doctor` to check the memory server.",
                llmenv_mcp::stdio_rpc::tidy_reason(&format!("{e:#}"))
            ));
            own
        }
    };
    let complete = outcome.failed == 0;
    let kept = budget.kept_hashes();
    note_unless_saved(
        ctx.store.update(ctx.session_id, |l| {
            l.mark_sent(MAIN_AGENT, kept);
            if complete {
                l.set_scope_sent(MAIN_AGENT);
            }
        }),
        RECALL_STATE_UNSAVED,
    );
    let passthrough = if wake.is_empty() {
        Vec::new()
    } else {
        vec![wake]
    };
    Ok(budget.hook_text(passthrough))
}

/// Notice for a ledger update that did not run: the next recall may repeat records (#2729).
const RECALL_STATE_UNSAVED: &str = "llmenv: recall state was not saved (the state file was locked \
     or unreadable), so some records may repeat. Fix: run `llmenv doctor` to check the state \
     directory.";

/// Notice for tool activity that was not recorded: a later recall runs without it (#2729).
const ACTIVITY_UNSAVED: &str = "llmenv: recent tool activity was not saved (the state file was \
     locked or unreadable), so later recall may miss it. Fix: run `llmenv doctor` to check the \
     state directory.";

/// Tells the agent when a ledger update did not run (unsafe id, busy lock, or unreadable file).
fn note_unless_saved<T>(saved: Option<T>, notice: &str) {
    if saved.is_none() {
        agent_notice::record(notice);
    }
}

/// Tells the agent that recall was skipped, so it is not read as "nothing relevant" (#2664).
fn note_skipped() {
    agent_notice::record(
        "llmenv: recall was skipped: the recall ledger cannot be read or updated. Fix: run \
         `llmenv doctor` to check the state directory.",
    );
}

/// The session's recall ledger, or a logged reason to skip recall. An empty default would forget
/// what this session already received, so the hook injects nothing rather than repeat it (#2711).
fn ledger_or_skip(store: &LedgerStore, session_id: &str, hook: &str) -> Option<Ledger> {
    let ledger = store.load(session_id);
    if ledger.is_none() {
        tracing::error!(
            "{hook}: recall skipped, the recall ledger cannot be read \
             (unsafe session id, busy lock, or unreadable file)"
        );
        note_skipped();
    }
    ledger
}

/// `TurnStart`: relevance recall, with the scope set as a fallback when no
/// `SessionStart` ran in this epoch.
pub(super) async fn turn_start(
    ctx: &AdaptiveCtx<'_>,
    scope: Vec<Action>,
) -> anyhow::Result<super::HookText> {
    let Some(ledger) = ledger_or_skip(ctx.store, ctx.session_id, "turn_start") else {
        return Ok(super::HookText::default());
    };
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
    note_unless_saved(
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
        }),
        RECALL_STATE_UNSAVED,
    );
    Ok(budget.hook_text(Vec::new()))
}

/// `PostToolUseFailure`: record the error, then inject related memories once.
pub(super) async fn tool_failure(ctx: &AdaptiveCtx<'_>) -> anyhow::Result<super::HookText> {
    let tool = ctx.payload["tool_name"].as_str().unwrap_or_default();
    let error = ctx.payload["error"].as_str().unwrap_or_default();
    let now = unix_now();
    // One lock covers the error record and the read. Without the read, a busy or
    // unreadable ledger would drop the record with no log (#2723).
    let Some(ledger) = ctx.store.update(ctx.session_id, |l| {
        l.push_error(tool, error, now);
        l.clone()
    }) else {
        tracing::error!(
            "tool_failure: session {} tool {tool}: error not recorded and recall skipped, \
             the recall ledger cannot be updated (unsafe session id, busy lock, or unreadable file)",
            ctx.session_id
        );
        note_skipped();
        return Ok(super::HookText::default());
    };
    let (key, record) = agent_key(ctx.payload);
    let sent = ledger.sent_for(&key);
    let text = relevance::error_query(tool, error);
    let resolved = RecallQuery {
        topic: Some("errors-resolved".to_string()),
        ..query(&text, FANOUT_LIMIT)
    };
    // The session project comes first, as in `waves`. The all-projects main recall stays
    // after the shared `errors-resolved` fix, because that topic is meant to cross projects.
    let scoped = ledger.project.as_deref().map(|p| RecallQuery {
        project: Some(p.to_string()),
        ..query(&text, FAILURE_LIMIT)
    });
    let (own, fixes, main) = tokio::join!(
        recall(ctx.client, scoped),
        recall(ctx.client, Some(resolved)),
        recall(ctx.client, Some(query(&text, FAILURE_LIMIT))),
    );
    let mut outcome = Outcome::default();
    outcome.push(own);
    outcome.push(fixes);
    outcome.push(main);
    let mut budget = RecallBudget::new(FAILURE_BUDGET_BYTES, sent);
    outcome.texts.iter().for_each(|t| budget.add_text(t));
    report("post_tool_use_failure", &outcome, &budget);
    if record {
        let kept = budget.kept_hashes();
        note_unless_saved(
            ctx.store
                .update(ctx.session_id, |l| l.mark_sent(&key, kept)),
            RECALL_STATE_UNSAVED,
        );
    }
    Ok(budget.hook_text(Vec::new()))
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
            note_unless_saved(
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
                }),
                ACTIVITY_UNSAVED,
            );
        }
        HookEvent::SubagentTask if payload["tool_name"].as_str() == Some("Agent") => {
            let input = &payload["tool_input"];
            let id = payload["tool_use_id"].as_str().unwrap_or_default();
            let kind = input["subagent_type"].as_str().unwrap_or("general-purpose");
            let prompt = input["prompt"].as_str().unwrap_or_default();
            note_unless_saved(
                store.update(session_id, |l| l.queue_subagent(id, kind, prompt, now)),
                ACTIVITY_UNSAVED,
            );
        }
        _ => {}
    }
}

#[cfg(test)]
#[expect(clippy::unwrap_used, clippy::expect_used, reason = "test code")]
mod tests {
    #[test]
    fn ledger_or_skip_gives_no_ledger_for_an_unsafe_session_id() {
        // An unsafe id has no ledger file, so `load` returns None. The caller must skip recall,
        // not treat the missing ledger as an empty one that repeats every record.
        let dir = tempfile::tempdir().expect("test");
        let store = LedgerStore::new(dir.path());
        assert!(ledger_or_skip(&store, "../escape", "test").is_none());
    }

    #[test]
    fn a_scope_set_cut_by_the_budget_names_the_skipped_count() {
        assert_eq!(scope_cut_notice(0), None);
        let notice = scope_cut_notice(3).expect("a cut scope set is noticed");
        assert!(notice.contains("3 recall action(s)"), "{notice}");
        assert!(
            notice.contains("Fix:"),
            "the notice names the fix: {notice}"
        );
    }

    #[test]
    fn a_failed_first_update_is_noticed_even_without_a_reset() {
        // The first update holds the only copy of the sent set for this session. A later
        // successful update does not undo a lost first update, so the notice must come from
        // the first update alone.
        assert_eq!(first_update_notice(FirstUpdate::Saved, false), None);
        assert_eq!(
            first_update_notice(FirstUpdate::NotApplied, false),
            Some(RECALL_STATE_UNSAVED)
        );
        assert_eq!(
            first_update_notice(FirstUpdate::NotApplied, true),
            Some(RESET_UNDONE)
        );
        assert_eq!(first_update_notice(FirstUpdate::Saved, true), None);
    }

    #[test]
    fn a_reset_whose_save_fails_is_reported_as_undone() {
        // The reset was applied in memory but never reached disk, so the agent must hear it.
        assert_eq!(
            first_update_notice(FirstUpdate::SaveFailed, true),
            Some(RESET_UNDONE)
        );
    }

    #[test]
    fn first_update_maps_the_store_result() {
        assert_eq!(FirstUpdate::of::<()>(None), FirstUpdate::NotApplied);
        assert_eq!(FirstUpdate::of(Some(&((), true))), FirstUpdate::Saved);
        assert_eq!(FirstUpdate::of(Some(&((), false))), FirstUpdate::SaveFailed);
    }

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
        .unwrap()
        .text();
        assert!(
            out.contains("wake pack") && out.contains("scope fact"),
            "{out}"
        );
        let turn = json!({"prompt": "work on recall"});
        let out = turn_start(&ctx(&f, &turn), vec![tag_action("proj")])
            .await
            .unwrap()
            .text();
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
            .unwrap()
            .text();
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
            .unwrap()
            .text();
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
            .unwrap()
            .text();
        assert!(out.contains("scope fact"), "{out}");
        assert!(f.store.load("s1").unwrap().scope_sent(MAIN_AGENT));
    }

    #[tokio::test]
    async fn a_repeated_query_with_no_new_activity_makes_no_relevance_calls() {
        let server = server_with(&[("\"limit\":10", "[context-p] fact")]).await;
        let f = fixture(&server);
        f.store.update("s1", |l| l.set_scope_sent(MAIN_AGENT));
        let turn = json!({"prompt": "same"});
        turn_start(&ctx(&f, &turn), vec![]).await.unwrap().text();
        let calls_before = server.received_requests().await.unwrap().len();
        turn_start(&ctx(&f, &turn), vec![]).await.unwrap().text();
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
        let out = turn_start(&ctx(&f, &turn), vec![]).await.unwrap().text();
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
        let out = turn_start(&ctx(&f, &turn), vec![]).await.unwrap().text();
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
            .unwrap()
            .text();
        tool_failure(&ctx(&f, &json!({"tool_name": "Bash", "error": "boom"})))
            .await
            .unwrap()
            .text();
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
            .unwrap()
            .text();
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
            Some(
                "llmenv: memory turn_start recall skipped: all 3 ICM calls failed. Fix: run \
                 `llmenv doctor` to check the memory server."
            )
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
        .unwrap()
        .text();
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
        .unwrap()
        .text();
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
        .unwrap()
        .text();
        assert!(
            out.contains("big scope fact"),
            "the record fits: {}",
            out.len()
        );
    }

    async fn start_with_a_failed_wake(
        scope_body: Option<&str>,
        scope: Vec<Action>,
    ) -> anyhow::Result<crate::hook_run::HookText> {
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
                .unwrap()
                .text();
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
            .unwrap()
            .text();
        assert_eq!(
            f.store.load("s1").unwrap().project.as_deref(),
            Some("llmenv")
        );
        let out = turn_start(&ctx(&f, &json!({"prompt": "why"})), vec![])
            .await
            .unwrap()
            .text();
        let project_at = out.find("project fact").expect("project-scoped record");
        let global_at = out.find("global fact").expect("all-projects record");
        assert!(project_at < global_at, "{out}");
    }

    #[tokio::test]
    async fn tool_failure_injects_errors_once() {
        let server = server_with(&[("errors-resolved", "[errors-resolved] fix: pin mcp<2")]).await;
        let f = fixture(&server);
        let payload = json!({"tool_name": "Bash", "error": "Exit code 1\nImportError request_ctx"});
        let first = tool_failure(&ctx(&f, &payload)).await.unwrap().text();
        assert!(first.contains("pin mcp<2"), "{first}");
        assert_eq!(f.store.load("s1").unwrap().errors().len(), 1);
        let second = tool_failure(&ctx(&f, &payload)).await.unwrap().text();
        assert!(!second.contains("pin mcp<2"), "already sent: {second}");
    }

    #[tokio::test]
    async fn session_start_adds_project_memories_and_drops_cut_records() {
        let server = server_with(&[
            (
                "icm_wake_up",
                "## Identity\n[preferences] global fact\n[preferences] cut mid […]",
            ),
            (
                "\"project\":\"proj-a\"",
                "[decisions-a] project fact\n[llmenv-scope-context] scope blob\n[d] cut […]",
            ),
        ])
        .await;
        let f = fixture(&server);
        let wake = Action::WakeUp(crate::hook_run::action::WakeUpArgs {
            max_tokens: None,
            project: Some("proj-a".into()),
        });
        let out = session_start(&ctx(&f, &json!({"source": "startup"})), wake, vec![])
            .await
            .unwrap()
            .text();
        assert!(out.contains("project fact"), "{out}");
        assert!(out.contains("global fact"), "{out}");
        assert!(
            !out.contains("scope blob"),
            "scope records come from the scope set: {out}"
        );
        assert!(!out.contains("cut"), "a cut record is dropped whole: {out}");
    }

    #[tokio::test]
    async fn wake_up_drops_only_the_cut_bullet() {
        // icm writes wake-up items as bullets under headings, not as `[topic]` records.
        let server = server_with(&[(
            "icm_wake_up",
            "# ICM Wake-up (project: proj-a)\n\n## Identity\n- kept preference\n- cut item in the fyi re […]\n",
        )])
        .await;
        let f = fixture(&server);
        let wake = Action::WakeUp(crate::hook_run::action::WakeUpArgs {
            max_tokens: None,
            project: Some("proj-a".into()),
        });
        let out = session_start(&ctx(&f, &json!({"source": "startup"})), wake, vec![])
            .await
            .unwrap()
            .text();
        assert!(out.contains("## Identity"), "{out}");
        assert!(out.contains("kept preference"), "{out}");
        assert!(
            !out.contains("cut item"),
            "a cut bullet is dropped alone: {out}"
        );
        let notice = agent_notice::current().unwrap_or_default();
        assert!(
            notice.contains("1 memory item(s) were dropped"),
            "the drop must reach the agent: {notice}"
        );
    }

    #[test]
    fn the_cut_notice_names_the_count_and_appears_only_when_items_were_cut() {
        assert_eq!(cut_items_notice(0), None);
        assert!(cut_items_notice(1).is_some_and(|n| n.contains("1 memory item(s)")));
        assert!(cut_items_notice(2).is_some_and(|n| n.contains("2 memory item(s)")));
    }

    #[test]
    fn the_pack_budget_counts_the_project_memories_when_wake_up_fails() {
        let failed: anyhow::Result<String> = Err(anyhow::anyhow!("icm down"));
        assert_eq!(pack_bytes(&failed, "abc"), 5);
        let loaded: anyhow::Result<String> = Ok("hello".to_string());
        assert_eq!(pack_bytes(&loaded, ""), 7);
    }

    #[tokio::test]
    async fn a_resumed_session_skips_the_project_recall() {
        // A resumed session already holds its pack, so the project recall must not run.
        let server = server_with(&[("\"project\":\"proj-a\"", "[decisions-a] project fact")]).await;
        let f = fixture(&server);
        let wake = Action::WakeUp(crate::hook_run::action::WakeUpArgs {
            max_tokens: None,
            project: Some("proj-a".into()),
        });
        let out = session_start(&ctx(&f, &json!({"source": "resume"})), wake, vec![])
            .await
            .unwrap()
            .text();
        assert!(
            !out.contains("project fact"),
            "a resume sends no pack: {out}"
        );
    }

    #[tokio::test]
    async fn a_failed_project_recall_keeps_the_wake_up_pack() {
        let server = server_with(&[("icm_wake_up", "## Identity\n- global fact")]).await;
        let f = fixture(&server);
        let wake = Action::WakeUp(crate::hook_run::action::WakeUpArgs {
            max_tokens: None,
            project: Some("proj-b".into()),
        });
        let out = session_start(&ctx(&f, &json!({"source": "startup"})), wake, vec![])
            .await
            .unwrap()
            .text();
        assert!(
            out.contains("global fact"),
            "the wake-up pack still goes out: {out}"
        );
        let notice = agent_notice::current().unwrap_or_default();
        assert!(
            notice.contains("project memories were skipped"),
            "the skipped project recall must reach the agent: {notice}"
        );
    }

    #[tokio::test]
    async fn a_failed_wake_up_keeps_the_project_memories() {
        // The wake-up mock is registered first, so the wake-up request gets its 500 response.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(body_string_contains("icm_wake_up"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(body_string_contains("\"project\":\"proj-a\""))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(text("[decisions-a] project fact")),
            )
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(body_string_contains("initialize"))
            .respond_with(ResponseTemplate::new(200).set_body_json(text("")))
            .mount(&server)
            .await;
        let f = fixture(&server);
        let wake = Action::WakeUp(crate::hook_run::action::WakeUpArgs {
            max_tokens: None,
            project: Some("proj-a".into()),
        });
        let out = session_start(&ctx(&f, &json!({"source": "startup"})), wake, vec![])
            .await
            .unwrap()
            .text();
        assert!(
            out.contains("project fact"),
            "the project memories must survive: {out}"
        );
        let notice = agent_notice::current().unwrap_or_default();
        assert!(
            notice.contains("wake-up skipped"),
            "the failed wake-up must reach the agent: {notice}"
        );
    }

    #[tokio::test]
    async fn tool_failure_ranks_the_session_project_first() {
        let server = server_with(&[
            ("\"project\":\"proj-a\"", "[own] fix for proj-a"),
            ("errors-resolved", "[fix] shared fix"),
        ])
        .await;
        let f = fixture(&server);
        f.store
            .update("s1", |l| l.project = Some("proj-a".to_string()));
        let payload = json!({"tool_name": "Bash", "error": "Exit code 1\nImportError request_ctx"});
        let out = tool_failure(&ctx(&f, &payload)).await.unwrap().text();
        let own = out.find("[own]");
        let fix = out.find("[fix]");
        assert!(
            matches!((own, fix), (Some(o), Some(f)) if o < f),
            "the project record must come first, and both must be present: {out}"
        );
    }

    #[test]
    fn tool_failure_logs_when_the_error_cannot_be_recorded() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let logs = crate::test_log_capture::capture_logs(|| {
            rt.block_on(async {
                let server = server_with(&[]).await;
                let f = fixture(&server);
                // A directory where the ledger file belongs makes the read fail, not miss.
                std::fs::create_dir_all(f._dir.path().join("recall_session/s1.json")).unwrap();
                let payload = json!({"tool_name": "Bash", "error": "boom"});
                let out = tool_failure(&ctx(&f, &payload)).await.unwrap().text();
                assert!(out.is_empty(), "{out}");
            });
        });
        assert!(logs.contains("error not recorded"), "{logs}");
    }

    #[tokio::test]
    async fn invalid_agent_id_records_nothing() {
        let server = server_with(&[("errors-resolved", "[errors-resolved] fix")]).await;
        let f = fixture(&server);
        let payload = json!({"tool_name": "Bash", "error": "boom", "agent_id": "../evil"});
        let out = tool_failure(&ctx(&f, &payload)).await.unwrap().text();
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
        let out = subagent_start(&ctx(&f, &payload)).await.unwrap().text();
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
        subagent_start(&ctx(&f, &resumed)).await.unwrap().text();
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
    async fn an_unreadable_ledger_keeps_the_pending_subagent_task() {
        use std::os::unix::fs::PermissionsExt;
        let server = server_with(&[("\"limit\":10", "")]).await;
        let f = fixture(&server);
        record_local(
            HookEvent::SubagentTask,
            &f.store,
            "s1",
            &json!({"tool_name": "Agent", "tool_use_id": "u3",
                    "tool_input": {"subagent_type": "Explore", "prompt": "queued task"}}),
        );
        let path = f._dir.path().join("recall_session").join("s1.json");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();
        let payload = json!({"agent_id": "agent-1", "agent_type": "Explore"});
        let out = subagent_start(&ctx(&f, &payload)).await.unwrap().text();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(out, "", "recall is skipped when the ledger cannot be read");
        // The skipped call must not consume the task, because SubagentStart does not fire again.
        let task = f
            .store
            .update("s1", |l| l.take_subagent("Explore", unix_now()))
            .flatten();
        assert!(task.is_some(), "the pending task must stay queued");
    }

    #[tokio::test]
    async fn subagent_start_without_an_agent_id_does_nothing() {
        let server = server_with(&[]).await;
        let f = fixture(&server);
        let payload = json!({"agent_type": "Explore"});
        let out = subagent_start(&ctx(&f, &payload)).await.unwrap().text();
        assert!(out.is_empty(), "{out}");
        let notice = crate::agent_notice::current().unwrap_or_default();
        assert!(notice.contains("has no usable agent_id"), "{notice}");
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
        let out = turn_start(&ctx(&f, &payload), Vec::new())
            .await
            .unwrap()
            .text();
        assert!(out.contains("keyword fact"), "{out}");
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::expect_fun_call)]
mod skip_notice_tests {
    use super::{LedgerStore, ledger_or_skip, note_unless_saved};

    #[test]
    fn a_skipped_recall_tells_the_agent() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = LedgerStore::new(dir.path());
        assert!(ledger_or_skip(&store, "../unsafe", "turn_start").is_none());
        let text = crate::agent_notice::current().unwrap_or_default();
        assert!(
            text.contains("recall was skipped: the recall ledger"),
            "got: {text}"
        );
    }

    #[test]
    fn an_update_that_did_not_run_tells_the_agent_and_a_saved_one_does_not() {
        note_unless_saved(None::<()>, "adaptive test: update did not run");
        note_unless_saved(Some(()), "adaptive test: update was saved");
        let text = crate::agent_notice::current().unwrap_or_default();
        assert!(
            text.contains("adaptive test: update did not run"),
            "got: {text}"
        );
        assert!(
            !text.contains("adaptive test: update was saved"),
            "got: {text}"
        );
    }
}
