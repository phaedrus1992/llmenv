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

mod failure_and_subagent;
mod project_memories;
mod session_start;
mod turn_start;

async fn server_with(pairs: &[(&str, &str)]) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(body_string_contains("initialize"))
        .respond_with(crate::hook_run::test_recall::ToolReply(("").to_string()))
        .mount(&server)
        .await;
    for (needle, body) in pairs {
        Mock::given(method("POST"))
            .and(body_string_contains(*needle))
            .respond_with(crate::hook_run::test_recall::ToolReply((*body).to_string()))
            .mount(&server)
            .await;
    }
    server
}

struct Fixture {
    _dir: tempfile::TempDir,
    store: LedgerStore,
    client: McpHttpClient,
    topics: Vec<String>,
}

fn fixture(server: &MockServer) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let store = LedgerStore::new(dir.path());
    let client = McpHttpClient::test_new(server.uri(), Duration::from_secs(2)).unwrap();
    Fixture {
        _dir: dir,
        store,
        client,
        topics: crate::config::Memory::default().global_topics,
    }
}

fn ctx<'a>(f: &'a Fixture, payload: &'a serde_json::Value) -> AdaptiveCtx<'a> {
    AdaptiveCtx {
        client: &f.client,
        store: &f.store,
        session_id: "s1",
        payload,
        global_topics: &f.topics,
        state_dir: f._dir.path(),
    }
}

fn tag_action(tag: &str) -> Action {
    let mut queries = crate::hook_run::tag_recall_queries(&[tag.to_string()]).unwrap();
    Action::RecallTag(queries.remove(0))
}

async fn failing_server() -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(body_string_contains("initialize"))
        .respond_with(crate::hook_run::test_recall::ToolReply(("").to_string()))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;
    server
}

#[test]
fn outcome_merge_adds_counts_and_texts() {
    use crate::hook_run::action::Recalled;
    let mut a = Outcome::default();
    a.push(Some(Ok(Reply::Records(vec![Recalled::from_text(
        "[t] one",
    )]))));
    a.push(Some(Err(anyhow::anyhow!("down"))));
    let mut b = Outcome::default();
    b.push(Some(Ok(Reply::Records(vec![Recalled::from_text(
        "[t] two",
    )]))));
    b.push(Some(Ok(Reply::Text("[t] not a recall".to_string()))));
    b.push(Some(Err(anyhow::anyhow!("down"))));
    b.push(Some(Err(anyhow::anyhow!("down"))));
    a.merge(b);
    assert_eq!(
        (a.calls, a.failed),
        (6, 4),
        "a text reply is a failed recall"
    );
    assert_eq!(a.text(), "[t] one\n[t] two");
}
