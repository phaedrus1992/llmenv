use std::time::Duration;

use serde_json::json;
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::*;

async fn run(
    event: HookEvent,
    payload: &serde_json::Value,
    adaptive_recall: bool,
) -> tempfile::TempDir {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "jsonrpc": "2.0", "id": 1,
            "result": {"content": [{"type": "text", "text": ""}]}
        })))
        .mount(&server)
        .await;
    let client = McpHttpClient::test_new(server.uri(), Duration::from_secs(2)).unwrap();
    let state_dir = tempfile::tempdir().unwrap();
    run_event_memory(MemoryCall {
        event,
        client: &client,
        settings: Some(&MemoryHookSettings {
            adaptive_recall,
            global_topics: vec![],
            ..MemoryHookSettings::default()
        }),
        session_id: Some("s1"),
        state_dir: Some(state_dir.path().to_path_buf()),
        payload,
        actions: Vec::new(),
        scope: Vec::new(),
        wake: WakeUpArgs::default(),
        query: "",
        store_content: "",
    })
    .await
    .unwrap();
    state_dir
}

fn wrote_ledger(state_dir: &tempfile::TempDir) -> bool {
    state_dir.path().join("recall_session/s1.json").is_file()
}

fn adaptive_events() -> [(HookEvent, serde_json::Value); 4] {
    [
        (HookEvent::SessionStart, json!({"source": "startup"})),
        (HookEvent::TurnStart, json!({"prompt": "hi"})),
        (
            HookEvent::PostToolUseFailure,
            json!({"tool_name": "Bash", "error": "x"}),
        ),
        (
            HookEvent::SubagentStart,
            json!({"agent_id": "a1", "agent_type": "Explore"}),
        ),
    ]
}

#[tokio::test]
async fn adaptive_events_take_the_ledger_flow() {
    for (event, payload) in adaptive_events() {
        let dir = run(event, &payload, true).await;
        assert!(wrote_ledger(&dir), "{event:?} must use the adaptive flow");
    }
}

#[tokio::test]
async fn adaptive_recall_off_keeps_the_stateless_flow() {
    for (event, payload) in adaptive_events() {
        let dir = run(event, &payload, false).await;
        assert!(!wrote_ledger(&dir), "{event:?} must stay stateless");
    }
}

fn git_repo_with_origin() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    for args in [
        &["init", "-q"][..],
        &["remote", "add", "origin", "git@github.com:me/wake-repo.git"],
    ] {
        let ok = std::process::Command::new("git")
            .args(args)
            .current_dir(dir.path())
            .status()
            .unwrap()
            .success();
        assert!(ok, "git {args:?}");
    }
    dir
}

#[test]
fn project_lookup_runs_for_session_start_and_stateless_turn_start_only() {
    let repo = git_repo_with_origin();
    let cwd = repo.path().to_str().unwrap();
    let payload = json!({"cwd": cwd});
    let adaptive = Some(MemoryHookSettings {
        adaptive_recall: true,
        global_topics: vec![],
        ..MemoryHookSettings::default()
    });
    let start = wake_args(
        HookEvent::SessionStart,
        adaptive.as_ref(),
        &payload,
        "/",
        Some("s1"),
    )
    .ok()
    .and_then(|w| w.project);
    assert_eq!(start.as_deref(), Some("wake-repo"));
    // #2253: the stateless TurnStart recall is scoped to the session project.
    let stateless = wake_args(HookEvent::TurnStart, None, &payload, "/", Some("s1"))
        .ok()
        .and_then(|w| w.project);
    assert_eq!(stateless.as_deref(), Some("wake-repo"));
    let adaptive_turn = wake_args(
        HookEvent::TurnStart,
        adaptive.as_ref(),
        &payload,
        "/",
        Some("s1"),
    )
    .ok()
    .and_then(|w| w.project);
    assert_eq!(adaptive_turn, None, "the ledger holds the project");
    // Adaptive is on, but no session id means the stateless path runs. It must still scope.
    let no_session = wake_args(HookEvent::TurnStart, adaptive.as_ref(), &payload, "/", None)
        .ok()
        .and_then(|w| w.project);
    assert_eq!(
        no_session.as_deref(),
        Some("wake-repo"),
        "no ledger, so no project otherwise"
    );
    let other = wake_args(HookEvent::PostToolUse, None, &payload, "/", None)
        .ok()
        .and_then(|w| w.project);
    assert_eq!(other, None);
}

#[test]
fn wake_args_refuses_to_guess_a_project_when_git_cannot_start() {
    let payload = serde_json::json!({ "cwd": "/no/such/dir/for/llmenv-hook" });
    let err = wake_args(HookEvent::SessionStart, None, &payload, "/", None)
        .err()
        .map(|e| format!("{e:#}"))
        .unwrap_or_default();
    assert!(
        err.contains("cannot run git") && err.contains("/no/such/dir/for/llmenv-hook"),
        "the agent must see the git failure and the directory: {err}"
    );
}

#[test]
fn stateless_turn_start_recall_carries_the_session_project() {
    let wake = WakeUpArgs {
        max_tokens: None,
        project: Some("llmenv".into()),
    };
    let turn = dispatch(
        HookEvent::TurnStart,
        &[],
        &[],
        &BTreeMap::new(),
        Some(&wake),
    );
    assert_eq!(turn, vec![Action::Recall("llmenv".into())]);
}

#[test]
fn hook_cwd_prefers_the_payload_then_the_fallback() {
    assert_eq!(
        hook_cwd(&json!({"cwd": "/from/payload"}), "/fallback"),
        std::path::PathBuf::from("/from/payload")
    );
    assert_eq!(
        hook_cwd(&json!({}), "/fallback"),
        std::path::PathBuf::from("/fallback")
    );
}
