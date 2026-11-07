//! The ICM store of a WebFetch or WebSearch result (#579).

use std::io::Write;

use serde_json::json;

use super::{checkpoint, detached_store, idempotency, json_or_empty};
use crate::session_log::{detached_child_log_path, redirect_stderr_to_detached_log};

/// Tool name constants for WebFetch and WebSearch tools.
const TOOL_NAME_WEBFETCH: &str = "WebFetch";
const TOOL_NAME_WEBSEARCH: &str = "WebSearch";

/// The topic of a fetched page with no known project. ICM matches a project against the topic,
/// so a page of a known project is stored under `web-fetch-<project>` (#2911).
const WEB_FETCH_TOPIC: &str = "web-fetch";

/// Whether `topic` holds fetched pages, with or without a project.
pub(super) fn is_web_fetch_topic(topic: &str) -> bool {
    topic == WEB_FETCH_TOPIC || topic.starts_with("web-fetch-")
}

fn web_fetch_topic(project: Option<&str>) -> String {
    match project {
        Some(project) if !project.is_empty() => format!("{WEB_FETCH_TOPIC}-{project}"),
        _ => WEB_FETCH_TOPIC.to_string(),
    }
}

/// Build ICM memory store arguments for a WebFetch/WebSearch PostToolUse event.
/// Returns `None` if the payload is not a WebFetch/WebSearch tool result.
///
/// # Format
/// The stored memory carries topic `web-fetch-<project>` (or `web-fetch` when `project` is
/// `None`) and importance `low`, so it decays quickly and can be bulk-cleared via
/// `icm_memory_forget_topic`.
#[must_use]
fn web_fetch_store_args(
    payload: &serde_json::Value,
    project: Option<&str>,
) -> Option<serde_json::Value> {
    let tool_name = web_tool(payload)?;
    let is_search = tool_name == TOOL_NAME_WEBSEARCH;
    let source_field = if is_search { "query" } else { "url" };
    let source_value = payload["tool_input"][source_field]
        .as_str()
        .unwrap_or("unknown");
    let label = if is_search { "Query" } else { "URL" };
    let response = payload["tool_response"]
        .as_str()
        .map_or_else(|| json_or_empty(&payload["tool_response"]), String::from);
    let needs_indicator = response.chars().count() > 1000;
    let mut truncated: String = response.chars().take(1000).collect();
    if needs_indicator {
        truncated.push_str("... (truncated)");
    }
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    let content = without_record_starts(&format!(
        "{label}: {source_value}\nTool: {tool_name}\nFetched at (epoch): {timestamp}\nContent preview:\n{truncated}"
    ));
    // The id is derived from the event, so a re-spawn or a resumed job sends the same one (#2397).
    let session = payload["session_id"].as_str().unwrap_or_default();
    let event = payload["tool_use_id"]
        .as_str()
        .map_or_else(|| timestamp.to_string(), String::from);
    // `content` holds the fetch time, so the id uses its stable parts: with a `tool_use_id` the
    // same tool call gives the same id at any later second.
    let request_id =
        idempotency::request_id(&[session, "icm-store", &event, source_value, &truncated]);
    Some(json!({
        "content": content,
        "topic": web_fetch_topic(project),
        "importance": "low",
        detached_store::REQUEST_ID_FIELD: request_id,
        detached_store::REQUEST_KEY_FIELD: session,
    }))
}

/// Handle PostToolUse for WebFetch/WebSearch by spawning a detached child
/// that stores the fetched content in ICM with fast-falloff memory
/// (importance: low, topic: web-fetch). The hook returns immediately instead of
/// blocking on the MCP round trip. Best-effort — failures are logged at debug
/// level and never propagated to the caller. The child's stderr goes to the
/// shared bounded log rather than `/dev/null` so its own failures are
/// diagnosable (#1133).
///
/// Returns the spawned [`std::process::Child`] purely so callers such as
/// tests can reap it (#1095) — production intentionally drops it unwaited,
/// identical to the previous behavior, since the child is
/// process-group-detached and outlives this process regardless.
pub(super) fn handle_web_fetch_post_tool_use(
    payload: &serde_json::Value,
) -> Option<std::process::Child> {
    handle_web_fetch_in(checkpoint::spawner_state_dir().as_deref(), payload)
}

/// The `llmenv icm-store` child, before its stderr log and process group are set.
pub(super) fn icm_store_command(
    exe: std::path::PathBuf,
    checkpoint: Option<&std::path::Path>,
) -> std::process::Command {
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("icm-store")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null());
    if let Some(path) = checkpoint {
        cmd.arg("--checkpoint").arg(path);
    }
    cmd
}

/// [`handle_web_fetch_post_tool_use`] with the state dir for the checkpoint given (#2396).
pub(super) fn handle_web_fetch_in(
    state_dir: Option<&std::path::Path>,
    payload: &serde_json::Value,
) -> Option<std::process::Child> {
    // The project lookup runs git, so it waits until the call is known to be a web fetch.
    web_tool(payload)?;
    let args = web_fetch_store_args(payload, session_project(payload).as_deref())?;
    let payload_json = match serde_json::to_string(&args) {
        Ok(json) => json,
        Err(e) => {
            crate::agent_notice::record_logged(format!(
                "llmenv: the fetched page is not stored in memory, because its store request \
                 cannot be built: {e}. Fix: report this as an llmenv bug."
            ));
            return None;
        }
    };
    // Spawns this same binary as a detached child. The path is this process's own image, not input.
    // nosemgrep: rust.lang.security.current-exe.current-exe
    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(e) => {
            crate::agent_notice::record_logged(format!(
                "llmenv: the fetched page is not stored in memory, because llmenv cannot find its \
                 own program: {e}. Fix: run `llmenv doctor`."
            ));
            return None;
        }
    };
    let checkpoint = checkpoint::begin(
        state_dir,
        checkpoint::JobKind::IcmStore,
        &args,
        payload["session_id"].as_str(),
    );
    let mut cmd = icm_store_command(exe, checkpoint.as_deref());
    redirect_stderr_to_detached_log(&mut cmd, detached_child_log_path);
    crate::mcp::proxy::detach_process_group(&mut cmd);
    let Ok(mut child) = crate::trace::spawn_counted("icm-store", &mut cmd) else {
        // The checkpoint written above stays, so a later session can run the store again (#3506).
        crate::agent_notice::record_logged(
            "llmenv: the fetched page is not stored in memory, because the store job cannot \
             start. Its checkpoint stays, so a later session can run it again. Fix: run \
             `llmenv doctor`.",
        );
        return None;
    };
    if let Some(mut stdin) = child.stdin.take()
        && let Err(e) = stdin.write_all(payload_json.as_bytes())
    {
        crate::agent_notice::record_logged(format!(
            "llmenv: the fetched page is not stored in memory, because it cannot be sent to the \
             store job: {e}. Fix: run `llmenv doctor`."
        ));
        // The child would get no page and record a second notice, so it is stopped (#3506).
        if let Err(kill) = child.kill() {
            tracing::error!(error = %kill, "cannot stop the store job that got no page");
        }
        return None;
    }
    // Not waited on by the caller: the child is process-group-detached and
    // outlives us.
    Some(child)
}

/// The name of a WebFetch or WebSearch call, or `None` for any other tool.
fn web_tool(payload: &serde_json::Value) -> Option<&str> {
    payload["tool_name"]
        .as_str()
        .filter(|name| [TOOL_NAME_WEBFETCH, TOOL_NAME_WEBSEARCH].contains(name))
}

/// `text` with a space before each line that opens with `[`. A recall splits its text into
/// records at `[<topic>] ` lines, so a page line such as `[1] Smith` would start a record that the
/// web-page filter of an all-projects recall does not see as part of the page (#2911).
fn without_record_starts(text: &str) -> String {
    text.split('\n')
        .map(|line| {
            if line.starts_with('[') {
                format!(" {line}")
            } else {
                line.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The project of the session that fetched the page. A page with no project is stored, but no
/// recall across projects returns it, so the agent is told when the project is unknown.
fn session_project(payload: &serde_json::Value) -> Option<String> {
    let fallback =
        std::env::current_dir().map_or_else(|_| String::new(), |d| d.display().to_string());
    let cwd = super::hook_cwd(payload, &fallback);
    crate::memory::project::session_project(&cwd)
        .inspect_err(|e| {
            crate::agent_notice::record_logged(format!(
                "llmenv: a fetched page was stored with no project, so no project recall \
                 returns it: {e:#}. Fix: run the session inside the project folder"
            ));
        })
        .ok()
}

#[cfg(test)]
#[expect(clippy::unwrap_used, clippy::expect_used, reason = "test code")]
mod tests;
