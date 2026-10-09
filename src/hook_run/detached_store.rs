//! Detaches the WebFetch/WebSearch ICM memory store call into a background
//! child process so a PostToolUse hook returns immediately instead of blocking
//! on the MCP network round trip. `handle_web_fetch_post_tool_use` in
//! `hook_run/mod.rs` is the parent-side launcher; `run_icm_store` is the child
//! entrypoint, wired to the hidden `llmenv icm-store` command.

use std::path::Path;
use std::time::Duration;

use crate::hook_run::checkpoint;
use crate::hook_run::idempotency::Guard;
use crate::hook_run::mcp_client::McpHttpClient;

/// Payload fields the parent adds for idempotency (#2397). The child removes them before the ICM
/// call, since they are not arguments of `icm_memory_store`.
pub(crate) const REQUEST_ID_FIELD: &str = "llmenv_request_id";
pub(crate) const REQUEST_KEY_FIELD: &str = "llmenv_request_key";

/// Per-call network timeout for the detached child's ICM memory store call.
const STORE_TIMEOUT: Duration = Duration::from_secs(5);

/// Child entrypoint: parse the `{content, topic, importance}` stdin payload,
/// resolve the active memory backend the same way a hook process would, and
/// store the memory. There's no terminal to write to, so on error this logs via
/// `tracing::error!` and the parent (`handle_web_fetch_post_tool_use`) points
/// the child's stderr at a bounded log — `error!` rather than `warn!` because
/// the default `EnvFilter` (`RUST_LOG` unset) is ERROR-only and dropped the
/// warning before it could reach that log (#1133).
///
/// # Errors
/// Malformed payload, no active memory backend, an invalid backend URL, or
/// the MCP call itself failing.
pub fn run_icm_store(payload_json: &str, checkpoint: Option<&Path>) -> anyhow::Result<()> {
    // The error is logged here and not in `run_icm_store_with`: a test of that function would
    // otherwise reach this callsite outside a subscriber and break the capture test below.
    run_icm_store_with(payload_json, checkpoint, run_icm_store_inner).inspect_err(|e| {
        tracing::error!("icm-store: detached store failed: {e}");
    })
}

/// [`run_icm_store`] with the store call injected, so a test needs no memory backend.
fn run_icm_store_with(
    payload_json: &str,
    checkpoint: Option<&Path>,
    store: impl FnOnce(serde_json::Value) -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    // The parent can fail to pipe all of the payload, so the checkpoint backs it up.
    let parsed = checkpoint::inputs_or_checkpoint::<serde_json::Value>(payload_json, checkpoint);
    let unparseable = parsed.is_err();
    let result = parsed.and_then(store);
    match &result {
        // A payload that does not parse cannot succeed on a retry, so its checkpoint goes too.
        Ok(()) => checkpoint::finish(checkpoint),
        Err(_) => {
            if unparseable {
                checkpoint::finish(checkpoint);
            }
        }
    }
    result
}

fn run_icm_store_inner(args: serde_json::Value) -> anyhow::Result<()> {
    let config_path = crate::paths::config_path()?;
    let config = crate::config::Config::load(&config_path)?;
    let env = crate::scope::matcher::Env::detect_for_config(&config)?;
    let active = crate::scope::evaluate(&config, &env);
    let config_dir = config_path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("config path has no parent"))?;
    let url = crate::hook_run::memory_url(&config, config_dir, &active)?.into_url()?;
    let client = McpHttpClient::new(url, STORE_TIMEOUT)
        .map_err(|e| anyhow::anyhow!("invalid memory backend URL: {e}"))?;

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let state_dir = crate::paths::state_dir().ok();
    rt.block_on(store_once(&client, state_dir.as_deref(), args))
}

/// Store `args` unless the request id they carry was already stored, and record the id after the
/// call succeeds. The id also goes to ICM as the keyword `request:<id>`, so a server-side dedup can
/// use it once rtk-ai/icm accepts a request id (#2397).
async fn store_once(
    client: &McpHttpClient,
    state_dir: Option<&Path>,
    mut args: serde_json::Value,
) -> anyhow::Result<()> {
    let mut take = |field: &str| {
        args.as_object_mut()
            .and_then(|o| o.remove(field))
            .and_then(|v| v.as_str().map(str::to_string))
            .unwrap_or_default()
    };
    let id = take(REQUEST_ID_FIELD);
    let key = take(REQUEST_KEY_FIELD);
    let guard = Guard::new(state_dir, &key, &id);
    if guard.as_ref().is_some_and(Guard::already_done) {
        return Ok(());
    }
    if !id.is_empty() {
        args["keywords"] = serde_json::json!([format!("request:{id}")]);
    }
    client.call_tool("icm_memory_store", args).await?;
    if let Some(guard) = &guard {
        guard.done();
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use wiremock::matchers::method;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn ok_body() -> serde_json::Value {
        serde_json::json!({"jsonrpc":"2.0","id":1,
            "result":{"content":[{"type":"text","text":"stored"}]}})
    }

    fn payload(id: &str) -> serde_json::Value {
        serde_json::json!({
            "content": "c", "topic": "web-fetch", "importance": "low",
            REQUEST_ID_FIELD: id, REQUEST_KEY_FIELD: "sess-1",
        })
    }

    async fn calls(server: &MockServer) -> Vec<serde_json::Value> {
        server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .filter_map(|r| serde_json::from_slice(&r.body).ok())
            .filter(|b: &serde_json::Value| b["method"] == "tools/call")
            .collect()
    }

    #[test]
    fn a_truncated_payload_runs_the_job_from_its_checkpoint_inputs() {
        let dir = tempfile::tempdir().unwrap();
        let inputs = serde_json::json!({ "content": "c" });
        let cp = checkpoint::Checkpoint::new(checkpoint::JobKind::IcmStore, inputs.clone(), None);
        let file = checkpoint::write(dir.path(), &cp).unwrap().unwrap();
        run_icm_store_with("{\"content\": \"", Some(&file), |args| {
            assert_eq!(args, inputs);
            Ok(())
        })
        .unwrap();
        assert!(!file.exists(), "success deletes the file");
    }

    #[test]
    fn a_payload_that_does_not_parse_and_has_no_checkpoint_is_an_error() {
        let err = run_icm_store_with("not json", None, |_| Ok(())).unwrap_err();
        assert!(err.to_string().contains("expected"), "{err}");
    }

    #[test]
    fn a_failed_store_keeps_its_checkpoint_and_a_successful_one_deletes_it() {
        let dir = tempfile::tempdir().unwrap();
        let cp =
            checkpoint::Checkpoint::new(checkpoint::JobKind::IcmStore, serde_json::json!({}), None);
        let file = checkpoint::write(dir.path(), &cp).unwrap().unwrap();
        run_icm_store_with("{}", Some(&file), |_| anyhow::bail!("backend down")).unwrap_err();
        assert!(
            file.exists(),
            "a failure a later run may fix keeps the file"
        );
        run_icm_store_with("{}", Some(&file), |_| Ok(())).unwrap();
        assert!(!file.exists(), "success deletes the file");
    }

    #[tokio::test]
    async fn the_same_payload_stores_once_and_carries_the_request_keyword() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(ok_body()))
            .mount(&server)
            .await;
        let client = McpHttpClient::test_new(server.uri(), Duration::from_secs(2)).unwrap();
        let dir = tempfile::tempdir().unwrap();
        store_once(&client, Some(dir.path()), payload("abc"))
            .await
            .unwrap();
        store_once(&client, Some(dir.path()), payload("abc"))
            .await
            .unwrap();
        let sent = calls(&server).await;
        assert_eq!(sent.len(), 1, "the second run must make no call");
        let args = &sent[0]["params"]["arguments"];
        assert_eq!(args["keywords"], serde_json::json!(["request:abc"]));
        assert!(
            args.get(REQUEST_ID_FIELD).is_none(),
            "internal fields never reach ICM"
        );
        assert!(args.get(REQUEST_KEY_FIELD).is_none());
    }

    #[tokio::test]
    async fn a_failed_call_records_nothing_and_is_retried() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(ok_body()))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(wiremock::matchers::body_string_contains("tools/call"))
            .respond_with(ResponseTemplate::new(500))
            .with_priority(1)
            .mount(&server)
            .await;
        let client = McpHttpClient::test_new(server.uri(), Duration::from_secs(2)).unwrap();
        let dir = tempfile::tempdir().unwrap();
        assert!(
            store_once(&client, Some(dir.path()), payload("abc"))
                .await
                .is_err()
        );
        assert!(
            store_once(&client, Some(dir.path()), payload("abc"))
                .await
                .is_err()
        );
        assert_eq!(
            calls(&server).await.len(),
            2,
            "a failure must not mark the id as stored"
        );
    }

    #[tokio::test]
    async fn a_payload_without_an_id_is_stored_every_time() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(ok_body()))
            .mount(&server)
            .await;
        let client = McpHttpClient::test_new(server.uri(), Duration::from_secs(2)).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let old = serde_json::json!({"content": "c", "topic": "web-fetch"});
        store_once(&client, Some(dir.path()), old.clone())
            .await
            .unwrap();
        store_once(&client, Some(dir.path()), old).await.unwrap();
        let sent = calls(&server).await;
        assert_eq!(sent.len(), 2);
        assert!(sent[0]["params"]["arguments"].get("keywords").is_none());
    }

    // #1133: this child's only report channel is its (now log-redirected)
    // stderr, and the default `EnvFilter` with `RUST_LOG` unset is ERROR-only —
    // a `warn!` here was dropped before it could reach that log.
    //
    // The malformed-payload rejection is asserted in the same test on purpose:
    // `tracing` caches a callsite's interest globally on first hit, so a
    // sibling test reaching this `error!` outside any subscriber would make the
    // capture order-dependent.
    #[test]
    fn run_icm_store_rejects_malformed_payload_json_and_logs_at_error_level() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("events.jsonl");
        let err = crate::session_log::tracing_layer::capture_file_logs_at(
            &log,
            tracing_subscriber::filter::LevelFilter::ERROR,
            || run_icm_store("not json", None).unwrap_err(),
        );

        assert!(err.to_string().to_lowercase().contains("expected"));
        let body = std::fs::read_to_string(&log).unwrap_or_default();
        assert!(
            body.contains("detached store failed"),
            "the failure must log at a level the default EnvFilter passes: {body}"
        );
    }
}
