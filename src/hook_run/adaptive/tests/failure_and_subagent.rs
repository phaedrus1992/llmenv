use super::*;

/// A JSON recall reply: one memory per `(topic, summary, keywords)`.
fn memories(items: &[(&str, &str, &[&str])]) -> String {
    let list: Vec<serde_json::Value> = items
        .iter()
        .map(|(topic, summary, keywords)| {
            json!({
                "id": format!("{topic}/{summary}"), "topic": topic, "summary": summary,
                "keywords": keywords, "importance": "medium", "score": 1.0,
            })
        })
        .collect();
    serde_json::Value::Array(list).to_string()
}

#[tokio::test]
async fn tool_failure_injects_errors_once() {
    let fix = memories(&[("errors-resolved", "fix: pin mcp<2", &["request_ctx"])]);
    let server = server_with(&[("errors-resolved", &fix)]).await;
    let f = fixture(&server);
    let payload = json!({"tool_name": "Bash", "error": "Exit code 1\nImportError request_ctx"});
    let first = tool_failure(&ctx(&f, &payload)).await.unwrap().text();
    assert!(first.contains("pin mcp<2"), "{first}");
    assert_eq!(f.store.load("s1").unwrap().errors().len(), 1);
    let second = tool_failure(&ctx(&f, &payload)).await.unwrap().text();
    assert!(!second.contains("pin mcp<2"), "already sent: {second}");
}

/// A user who drops `errors-resolved` from `global_topics` gets no cross-project read on a
/// tool failure (#2950).
#[tokio::test]
async fn tool_failure_reads_errors_resolved_only_as_a_global_topic() {
    let fix = memories(&[("errors-resolved", "fix: pin mcp<2", &["request_ctx"])]);
    let server = server_with(&[("errors-resolved", &fix)]).await;
    let mut f = fixture(&server);
    f.topics = vec!["preferences".to_string()];
    let payload = json!({"tool_name": "Bash", "error": "Exit code 1\nImportError request_ctx"});
    let out = tool_failure(&ctx(&f, &payload)).await.unwrap().text();
    assert!(!out.contains("pin mcp<2"), "{out}");
    let asked = server.received_requests().await.unwrap();
    assert!(
        asked
            .iter()
            .all(|r| !String::from_utf8_lossy(&r.body).contains("errors-resolved")),
        "no recall names errors-resolved"
    );
}

#[tokio::test]
async fn tool_failure_ranks_the_session_project_first() {
    let own = memories(&[("own", "fix for proj-a: ImportError", &[])]);
    let shared = memories(&[("fix", "shared fix", &["importerror"])]);
    let server = server_with(&[
        ("\"project\":\"proj-a\"", &own),
        ("errors-resolved", &shared),
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

/// ICM ranks the best of a bad set near 1.0, so only a shared token lets a memory in (#2921).
#[tokio::test]
async fn tool_failure_drops_a_memory_that_shares_no_token_with_the_error() {
    let reply = memories(&[
        (
            "errors-resolved",
            "Semgrep rules need a pinned ruleset",
            &["semgrep"],
        ),
        ("errors-resolved", "pin mcp<2 for request_ctx", &[]),
    ]);
    let server = server_with(&[("errors-resolved", &reply)]).await;
    let f = fixture(&server);
    let payload = json!({"tool_name": "Bash", "error": "Exit code 1\nImportError request_ctx"});
    let out = tool_failure(&ctx(&f, &payload)).await.unwrap().text();
    assert!(out.contains("pin mcp<2"), "{out}");
    assert!(!out.contains("Semgrep"), "{out}");
    let asked = server.received_requests().await.unwrap();
    assert!(
        asked
            .iter()
            .any(|r| String::from_utf8_lossy(&r.body).contains(r#""format":"json""#)),
        "the failure recall asks for JSON"
    );
}

#[test]
fn tool_failure_logs_when_the_error_cannot_be_recorded() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let server = server_with(&[]).await;
        let f = fixture(&server);
        // A directory where the ledger file belongs makes the read fail, not miss.
        std::fs::create_dir_all(f._dir.path().join("recall_session/s1.json")).unwrap();
        let payload = json!({"tool_name": "Bash", "error": "boom"});
        let out = tool_failure(&ctx(&f, &payload)).await.unwrap().text();
        assert!(out.is_empty(), "{out}");
    });
    // The agent notice is the observable result. A captured log is not: tracing caches the
    // interest of a call site for the whole process, so a test that runs on another thread at the
    // same time can hide the line from this thread's subscriber (#3394). `record_logged` writes
    // the same text to the log; its own test covers that.
    let notices = crate::agent_notice::current().unwrap_or_default();
    assert!(notices.contains("error not recorded"), "{notices}");
}

#[tokio::test]
async fn invalid_agent_id_records_nothing() {
    let fix = memories(&[("errors-resolved", "fix for boom", &[])]);
    let server = server_with(&[("errors-resolved", &fix)]).await;
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

/// An Agent call with no id or no prompt is not queued: an empty id would merge two subagents.
#[test]
fn an_agent_call_without_an_id_or_a_prompt_is_not_queued() {
    let dir = tempfile::tempdir().unwrap();
    let store = LedgerStore::new(dir.path());
    store.update("s1", |_| {});
    for payload in [
        json!({"tool_name": "Agent", "tool_input": {"subagent_type": "Explore", "prompt": "find x"}}),
        json!({"tool_name": "Agent", "tool_use_id": "", "tool_input": {"subagent_type": "Explore", "prompt": "find x"}}),
        json!({"tool_name": "Agent", "tool_use_id": "t1", "tool_input": {"subagent_type": "Explore"}}),
    ] {
        record_local(HookEvent::SubagentTask, &store, "s1", &payload);
    }
    let mut ledger = store.load("s1").unwrap();
    assert!(ledger.take_subagent("Explore", unix_now()).is_none());
}

/// An error with no distinctive token cannot pass the gate, so no recall runs (#2921).
#[tokio::test]
async fn a_failure_with_no_distinctive_token_sends_no_recall() {
    let server = server_with(&[]).await;
    let f = fixture(&server);
    let payload = json!({"tool_name": "Bash", "error": "Exit code 1\nerror: failed with 2"});
    let out = tool_failure(&ctx(&f, &payload)).await.unwrap().text();
    assert!(out.is_empty(), "{out}");
    let asked = server.received_requests().await.unwrap_or_default();
    assert!(
        asked
            .iter()
            .all(|r| !String::from_utf8_lossy(&r.body).contains("icm_memory_recall")),
        "no recall request"
    );
    assert_eq!(
        f.store.load("s1").unwrap().errors().len(),
        1,
        "the error is still recorded"
    );
}
