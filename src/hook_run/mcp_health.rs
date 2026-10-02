//! `SessionStart` health probe for the MCP servers llmenv manages (#2358).
//!
//! A managed server that is dead or wedged used to cost the session its memory and code
//! graph with no sign. Each probe sends a real MCP `initialize`, so a process that holds
//! its socket but never answers fails the probe.

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, anyhow};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::task::JoinSet;

use crate::hook_run::mcp_client::McpHttpClient;
use crate::mcp::resolve::{
    CODEBASE_MEMORY_MCP_NAME, MEMORY_MCP_NAME, ResolvedKind, ResolvedMcp, codebase_memory_paths,
    resolve_codebase_memory_entries, resolve_mcps,
};

/// How long one probe waits for an `initialize` answer.
/// A healthy local server answers in milliseconds. The wedged codebase-memory daemon in #2358
/// stayed silent for over 12 s, so any value past a few seconds separates the two.
/// Probes run in parallel, so this is also the worst-case delay a session start pays.
pub(crate) const DEFAULT_PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// A managed server that failed its probe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DownServer {
    pub(crate) name: String,
    pub(crate) reason: String,
}

/// Probe one server with a real MCP `initialize`.
pub(crate) async fn probe(server: &ResolvedMcp, timeout: Duration) -> anyhow::Result<()> {
    match &server.kind {
        ResolvedKind::Remote { url, .. } => McpHttpClient::new(url.clone(), timeout)?.probe().await,
        ResolvedKind::Stdio { command, args, env } => {
            probe_stdio(command, args, env, timeout).await
        }
    }
}

async fn probe_stdio(
    command: &str,
    args: &[String],
    env: &BTreeMap<String, String>,
    timeout: Duration,
) -> anyhow::Result<()> {
    let mut child = Command::new(command)
        .args(args)
        .envs(env)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("cannot start `{command}`"))?;
    let answered = tokio::time::timeout(timeout, handshake(&mut child, command)).await;
    // Reap the probe process now. `kill_on_drop` is only the backstop for an early return.
    let _ = child.kill().await;
    answered.unwrap_or_else(|_| {
        Err(anyhow!(
            "`{command}` did not answer MCP initialize within {} ms",
            timeout.as_millis()
        ))
    })
}

/// Send `initialize` and wait for the reply with id 0. Lines that are not that reply are
/// skipped, so a banner or a notification on stdout is not mistaken for a failure.
async fn handshake(child: &mut Child, command: &str) -> anyhow::Result<()> {
    let mut stdin = child.stdin.take().context("child stdin is not piped")?;
    let stdout = child.stdout.take().context("child stdout is not piped")?;
    let request = json!({
        "jsonrpc": "2.0",
        "id": 0,
        "method": "initialize",
        "params": {
            "protocolVersion": "2025-06-18",
            "capabilities": {},
            "clientInfo": { "name": "llmenv-health", "version": env!("CARGO_PKG_VERSION") }
        }
    });
    // A server that already exited fails this write. The read below then reports the exit.
    let write = async {
        stdin.write_all(format!("{request}\n").as_bytes()).await?;
        stdin.flush().await
    }
    .await;
    let mut lines = BufReader::new(stdout).lines();
    while let Some(line) = lines
        .next_line()
        .await
        .context("cannot read server output")?
    {
        if let Some(reply) = parse_reply(&line) {
            return reply;
        }
    }
    let note = write
        .err()
        .map(|e| format!(" (write failed: {e})"))
        .unwrap_or_default();
    Err(anyhow!(
        "`{command}` exited before answering MCP initialize{note}"
    ))
}

/// Read one stdout line as the `initialize` reply. `None` means the line is not that reply.
fn parse_reply(line: &str) -> Option<anyhow::Result<()>> {
    let value: Value = serde_json::from_str(line.trim()).ok()?;
    if value.get("id") != Some(&json!(0)) {
        return None;
    }
    if let Some(error) = value.get("error") {
        let message = error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("no message");
        return Some(Err(anyhow!("MCP initialize returned an error: {message}")));
    }
    Some(match value.get("result") {
        Some(_) => Ok(()),
        None => Err(anyhow!("MCP initialize reply has no result")),
    })
}

/// Probe every server in parallel and return the ones that failed, in input order.
pub(crate) async fn find_down(servers: &[ResolvedMcp], timeout: Duration) -> Vec<DownServer> {
    let mut probes = JoinSet::new();
    for (index, server) in servers.iter().cloned().enumerate() {
        probes.spawn(async move {
            let outcome = probe(&server, timeout).await;
            (index, server.name, outcome)
        });
    }
    let mut down = Vec::new();
    while let Some(joined) = probes.join_next().await {
        match joined {
            Ok((index, name, Err(e))) => down.push((
                index,
                DownServer {
                    name,
                    reason: format!("{e:#}"),
                },
            )),
            Ok(_) => {}
            Err(e) => tracing::error!(error = %e, "MCP health probe task failed"),
        }
    }
    down.sort_by_key(|(index, _)| *index);
    down.into_iter().map(|(_, server)| server).collect()
}

/// The memory and codebase-memory servers that the active scopes resolve to.
fn managed_servers(
    config: &crate::config::Config,
    config_dir: &Path,
    active: &crate::scope::ActiveScopes,
) -> anyhow::Result<Vec<ResolvedMcp>> {
    let merged = super::merged_memory(config, config_dir, active)?;
    let mut servers: Vec<ResolvedMcp> =
        resolve_mcps(&config.mcp, &merged.memory, &merged.host, &active.tags)
            .context("cannot resolve the memory MCP server")?
            .into_iter()
            .filter(|m| m.name == MEMORY_MCP_NAME)
            .collect();
    let entries = config
        .features
        .as_ref()
        .map(|f| f.codebase_memory.as_slice())
        .unwrap_or_default();
    if !entries.is_empty() {
        let (project_root, state_dir) = codebase_memory_paths()?;
        let cbm = resolve_codebase_memory_entries(entries, &active.tags, &project_root, &state_dir)
            .context("cannot resolve the codebase-memory MCP server")?;
        servers.extend(cbm);
    }
    Ok(servers)
}

/// Probe the managed servers and build the notice for the ones that are down. A failure to
/// resolve the servers is logged and skipped: a hook must never block the session.
pub(super) fn session_start_notice(
    rt: &tokio::runtime::Runtime,
    config: &crate::config::Config,
    config_dir: &Path,
    active: &crate::scope::ActiveScopes,
) -> Option<String> {
    let servers = managed_servers(config, config_dir, active)
        .inspect_err(|e| eprintln!("llmenv: MCP health check skipped: {e:#}"))
        .ok()?;
    if servers.is_empty() {
        return None;
    }
    let mut down = rt.block_on(find_down(&servers, DEFAULT_PROBE_TIMEOUT));
    if down.iter().any(|d| d.name == MEMORY_MCP_NAME)
        && crate::cli::ensure_local_memory_proxy(config, config_dir, active)
    {
        // This call started the proxy that was down, so ask it again before reporting.
        let memory: Vec<ResolvedMcp> = servers
            .into_iter()
            .filter(|s| s.name == MEMORY_MCP_NAME)
            .collect();
        down.retain(|d| d.name != MEMORY_MCP_NAME);
        down.extend(rt.block_on(find_down(&memory, DEFAULT_PROBE_TIMEOUT)));
    }
    down_notice(&down)
}

/// The context text that tells the agent which servers are down and how to bring them back.
pub(crate) fn down_notice(down: &[DownServer]) -> Option<String> {
    if down.is_empty() {
        return None;
    }
    let mut text = String::from("llmenv: MCP health check failed at session start.\n");
    for server in down {
        let (effect, fix) = effect_and_fix(&server.name);
        text.push_str(&format!(
            "- {}: {}\n  Effect: {effect}\n  Fix: {fix}\n",
            server.name, server.reason
        ));
    }
    Some(text)
}

fn effect_and_fix(name: &str) -> (&'static str, String) {
    match name {
        MEMORY_MCP_NAME => {
            let pidfile = crate::mcp::proxy::default_pid_path()
                .map_or_else(|_| "the mcp-proxy pidfile".to_string(), |p| p.display().to_string());
            (
                "Memory recall and store do not work in this session.",
                format!(
                    "Run `llmenv export > /dev/null` to restart a stopped proxy. If the proxy runs \
                     but does not answer, stop it with `kill $(cat {pidfile})` first."
                ),
            )
        }
        CODEBASE_MEMORY_MCP_NAME => (
            "Code graph tools do not work in this session.",
            "Stop the stuck daemon with `pkill -f cbm-daemon-internal`, then run `/mcp` to reconnect."
                .to_string(),
        ),
        _ => (
            "Its tools do not work in this session.",
            "Check the server command, then run `/mcp` to reconnect.".to_string(),
        ),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::mcp::resolve::ResolvedMcp;
    use wiremock::matchers::method;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const SHORT: Duration = Duration::from_millis(300);
    const INIT_REPLY: &str = r#"{"jsonrpc":"2.0","id":0,"result":{}}"#;

    fn sh(script: &str) -> ResolvedMcp {
        sh_env(script, BTreeMap::new())
    }

    fn sh_env(script: &str, env: BTreeMap<String, String>) -> ResolvedMcp {
        server(
            "stdio-test",
            ResolvedKind::Stdio {
                command: "sh".to_string(),
                args: vec!["-c".to_string(), script.to_string()],
                env,
            },
        )
    }

    fn server(name: &str, kind: ResolvedKind) -> ResolvedMcp {
        ResolvedMcp {
            name: name.to_string(),
            kind,
            headers: BTreeMap::new(),
            timeout: None,
            disabled_tools: Vec::new(),
            mcp_permissions: None,
            memory_hook: None,
        }
    }

    fn answers(reply: &str) -> ResolvedMcp {
        sh(&format!("read line; printf '%s\\n' '{reply}'"))
    }

    #[tokio::test]
    async fn stdio_probe_passes_when_server_answers_initialize() {
        probe(&answers(INIT_REPLY), SHORT).await.expect("probe ok");
    }

    #[tokio::test]
    async fn stdio_probe_fails_when_server_never_answers() {
        let err = probe(&sh("sleep 5"), SHORT).await.expect_err("wedged");
        assert!(err.to_string().contains("did not answer"), "got: {err}");
    }

    #[tokio::test]
    async fn stdio_probe_fails_when_process_exits_without_answering() {
        let err = probe(&sh("exit 3"), SHORT).await.expect_err("exited");
        assert!(err.to_string().contains("exited"), "got: {err}");
    }

    #[tokio::test]
    async fn stdio_probe_fails_when_command_is_missing() {
        let kind = ResolvedKind::Stdio {
            command: "llmenv-no-such-command".to_string(),
            args: Vec::new(),
            env: BTreeMap::new(),
        };
        let err = probe(&server("x", kind), SHORT).await.expect_err("missing");
        assert!(err.to_string().contains("cannot start"), "got: {err}");
    }

    #[tokio::test]
    async fn stdio_probe_reports_a_jsonrpc_error_reply() {
        let reply = r#"{"jsonrpc":"2.0","id":0,"error":{"code":-32600,"message":"nope"}}"#;
        let err = probe(&answers(reply), SHORT)
            .await
            .expect_err("error reply");
        assert!(err.to_string().contains("nope"), "got: {err}");
    }

    #[tokio::test]
    async fn stdio_probe_passes_configured_env_to_the_server() {
        let script =
            format!("read line; [ \"$LLMENV_PROBE_X\" = 1 ] && printf '%s\\n' '{INIT_REPLY}'");
        let env = BTreeMap::from([("LLMENV_PROBE_X".to_string(), "1".to_string())]);
        probe(&sh_env(&script, env), SHORT)
            .await
            .expect("env reached the server");
    }

    #[tokio::test]
    async fn remote_probe_passes_when_server_answers() {
        let mock = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).insert_header("mcp-session-id", "s"))
            .mount(&mock)
            .await;
        let kind = ResolvedKind::Remote {
            url: mock.uri(),
            transport: llmenv_config::McpTransport::Http,
        };
        probe(&server(MEMORY_MCP_NAME, kind), Duration::from_secs(2))
            .await
            .expect("up");
    }

    #[tokio::test]
    async fn find_down_returns_only_failures_in_input_order() {
        let servers = [
            sh("sleep 5"),
            answers(INIT_REPLY),
            server(
                "gone",
                ResolvedKind::Stdio {
                    command: "llmenv-no-such-command".to_string(),
                    args: Vec::new(),
                    env: BTreeMap::new(),
                },
            ),
        ];
        let down = find_down(&servers, SHORT).await;
        let names: Vec<&str> = down.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(names, ["stdio-test", "gone"]);
        assert!(down.iter().all(|d| !d.reason.is_empty()));
    }

    #[test]
    fn down_notice_is_none_when_nothing_is_down() {
        assert_eq!(down_notice(&[]), None);
    }

    #[test]
    fn down_notice_names_each_server_reason_and_fix() {
        let down = [
            DownServer {
                name: MEMORY_MCP_NAME.to_string(),
                reason: "connection refused".into(),
            },
            DownServer {
                name: CODEBASE_MEMORY_MCP_NAME.to_string(),
                reason: "timed out".into(),
            },
        ];
        let text = down_notice(&down).expect("notice");
        assert!(text.contains("connection refused") && text.contains("timed out"));
        assert!(text.contains("llmenv export"), "ICM fix missing: {text}");
        assert!(
            text.contains("cbm-daemon-internal"),
            "CBM fix missing: {text}"
        );
    }

    mod props {
        use super::super::parse_reply;
        use proptest::prelude::*;
        use serde_json::json;

        proptest! {
            #[test]
            fn parse_reply_never_panics(line in ".*") {
                let _ = parse_reply(&line);
            }

            #[test]
            fn an_error_reply_carries_its_message(msg in "[a-zA-Z0-9 ]{1,30}") {
                let line = json!({"jsonrpc": "2.0", "id": 0, "error": {"code": -1, "message": msg}})
                    .to_string();
                let err = parse_reply(&line).expect("is the reply").expect_err("error reply");
                prop_assert!(err.to_string().contains(&msg));
            }

            #[test]
            fn a_reply_for_another_id_is_skipped(id in 1u32..10_000) {
                let line = json!({"jsonrpc": "2.0", "id": id, "result": {}}).to_string();
                prop_assert!(parse_reply(&line).is_none());
            }
        }
    }
}
