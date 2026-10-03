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

use crate::cli::ProxyStart;
use crate::mcp::resolve::{
    CODEBASE_MEMORY_MCP_NAME, MEMORY_MCP_NAME, ResolvedKind, ResolvedMcp, codebase_memory_paths,
    resolve_codebase_memory_entries, resolve_mcps,
};
use llmenv_mcp::mcp_client::McpHttpClient;

/// How long one probe waits for an `initialize` answer.
/// A healthy local server answers in milliseconds. The wedged codebase-memory daemon in #2358
/// stayed silent for over 12 s, so any value past a few seconds separates the two.
/// Probes run in parallel, so this is also the worst-case delay a session start pays.
pub(crate) const DEFAULT_PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// The most characters of a failure reason that the notice carries. A server controls part of
/// the text, such as an HTTP error body, and the notice goes into the agent's context.
const MAX_REASON_CHARS: usize = 300;

/// Make `text` safe to put in the notice: one line, no control, bidirectional, or zero-width
/// characters, and at most [`MAX_REASON_CHARS`] characters.
pub(super) fn tidy_reason(text: &str) -> String {
    let spaced: String = text
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let one_line = crate::util::strip_unsafe_chars(&spaced)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if one_line.chars().count() <= MAX_REASON_CHARS {
        return one_line;
    }
    let kept: String = one_line.chars().take(MAX_REASON_CHARS - 1).collect();
    format!("{kept}…")
}

/// A managed server that failed its probe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DownServer {
    pub(crate) name: String,
    pub(crate) reason: String,
}

/// Probe one server with a real MCP `initialize`.
async fn probe(server: &ResolvedMcp, timeout: Duration) -> anyhow::Result<()> {
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
    if let Err(e) = child.kill().await {
        tracing::warn!(command, error = %e, "cannot stop the MCP health probe process");
    }
    answered.unwrap_or_else(|_| {
        Err(anyhow!(
            "`{command}` did not answer MCP initialize within {} ms",
            timeout.as_millis()
        ))
    })
}

/// Send `initialize` to the child and wait for the reply.
async fn handshake(child: &mut Child, command: &str) -> anyhow::Result<()> {
    let stdin = child.stdin.take().context("child stdin is not piped")?;
    let stdout = child.stdout.take().context("child stdout is not piped")?;
    handshake_io(stdin, stdout, command).await
}

/// Send `initialize` on `writer` and wait for the reply with id 0 on `reader`. Lines that are
/// not that reply are skipped, so a banner or a notification is not mistaken for a failure.
/// The writer stays open until the reply arrives, because some servers stop at end of input.
async fn handshake_io<W, R>(mut writer: W, reader: R, command: &str) -> anyhow::Result<()>
where
    W: tokio::io::AsyncWrite + Unpin,
    R: tokio::io::AsyncRead + Unpin,
{
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
    let sent = async {
        writer.write_all(format!("{request}\n").as_bytes()).await?;
        writer.flush().await
    }
    .await;
    // A closed pipe means the server is gone. Waiting on its output would only hide that
    // behind the timeout message.
    if let Err(e) = sent {
        return Err(anyhow!(
            "`{command}` exited before answering MCP initialize (write failed: {e})"
        ));
    }
    let mut lines = BufReader::new(reader).lines();
    while let Some(line) = lines
        .next_line()
        .await
        .context("cannot read server output")?
    {
        if let Some(reply) = parse_reply(&line) {
            return reply;
        }
    }
    Err(anyhow!(
        "`{command}` exited before answering MCP initialize"
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
    find_down_with(servers, move |server| async move {
        probe(&server, timeout).await
    })
    .await
}

/// [`find_down`] with the probe injected, so a test can make a probe crash.
async fn find_down_with<F, Fut>(servers: &[ResolvedMcp], probe: F) -> Vec<DownServer>
where
    F: Fn(ResolvedMcp) -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<()>> + Send + 'static,
{
    // Each handle keeps its server name, so a probe task that crashes still names its server.
    let handles: Vec<_> = servers
        .iter()
        .cloned()
        .map(|server| (server.name.clone(), tokio::spawn(probe(server))))
        .collect();
    let mut down = Vec::new();
    for (name, handle) in handles {
        match handle.await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                tracing::debug!(server = %name, error = %format!("{e:#}"), "MCP health probe failed");
                down.push(DownServer {
                    name,
                    reason: tidy_reason(&format!("{e:#}")),
                });
            }
            Err(crashed) => down.push(DownServer {
                name,
                reason: format!("health probe crashed: {crashed}"),
            }),
        }
    }
    down
}

/// The memory and codebase-memory servers that the active scopes resolve to.
pub(crate) fn managed_servers(
    config: &crate::config::Config,
    config_dir: &Path,
    active: &crate::scope::ActiveScopes,
) -> anyhow::Result<Vec<ResolvedMcp>> {
    let merged = crate::memory::resolve::merged_memory(config, config_dir, active)?;
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
    let servers = match managed_servers(config, config_dir, active) {
        Ok(servers) => servers,
        Err(e) => {
            tracing::warn!(error = %e, "MCP health check could not run");
            return Some(unresolved_notice(&e));
        }
    };
    if servers.is_empty() {
        return None;
    }
    let mut down = rt.block_on(find_down(&servers, DEFAULT_PROBE_TIMEOUT));
    if down.iter().any(|d| d.name == MEMORY_MCP_NAME) {
        let outcome = crate::cli::ensure_local_memory_proxy(config, config_dir, active);
        if outcome == ProxyStart::Started {
            // This call started the proxy that was down, so ask it again before reporting.
            let memory: Vec<ResolvedMcp> = servers
                .iter()
                .filter(|s| s.name == MEMORY_MCP_NAME)
                .cloned()
                .collect();
            let after = rt.block_on(find_down(&memory, DEFAULT_PROBE_TIMEOUT));
            down = merge_in_order(&servers, down, after);
        } else if let Some(note) = restart_note(&outcome) {
            for d in down.iter_mut().filter(|d| d.name == MEMORY_MCP_NAME) {
                d.reason = format!("{}; {note}", d.reason);
            }
        }
    }
    down_notice(&down)
}

/// Replace the memory server's entry in `down` with its result after a restart, and keep the
/// servers in the order of `servers`.
fn merge_in_order(
    servers: &[ResolvedMcp],
    down: Vec<DownServer>,
    memory_after: Vec<DownServer>,
) -> Vec<DownServer> {
    servers
        .iter()
        .filter_map(|server| {
            let source = if server.name == MEMORY_MCP_NAME {
                &memory_after
            } else {
                &down
            };
            source.iter().find(|d| d.name == server.name).cloned()
        })
        .collect()
}

/// Why llmenv did not restart the memory proxy, when the reader needs to know.
fn restart_note(outcome: &ProxyStart) -> Option<String> {
    match outcome {
        ProxyStart::Started => None,
        ProxyStart::Failed(cause) => Some(format!("llmenv could not start the proxy: {cause}")),
        ProxyStart::AlreadyRunning => Some(
            "the proxy process runs but does not answer, so llmenv did not restart it".to_string(),
        ),
        ProxyStart::NotLocal => {
            Some("this host does not serve memory, so llmenv did not try to restart it".to_string())
        }
    }
}

/// The notice for a config that stops the managed servers from resolving. Without it the check
/// would turn itself off, and the agent would take silence for health.
fn unresolved_notice(error: &anyhow::Error) -> String {
    format!(
        "llmenv: MCP health check could not run: {}\n  Fix: run `llmenv doctor` to see the \
         config problem.\n",
        tidy_reason(&format!("{error:#}"))
    )
}

/// The context text that tells the agent which servers are down and how to bring them back.
fn down_notice(down: &[DownServer]) -> Option<String> {
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

pub(crate) fn effect_and_fix(name: &str) -> (&'static str, String) {
    match name {
        MEMORY_MCP_NAME => (
            "Memory recall and store do not work in this session.",
            "Run `llmenv export > /dev/null` to restart a stopped proxy. If the proxy runs but \
             does not answer, stop it with `pkill -f 'mcp-proxy .*-- icm serve'` first. That \
             matches the proxy by its command line, so it cannot hit a reused process id."
                .to_string(),
        ),
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

    /// For a server that must answer: long enough that CPU load cannot fail the test.
    const GENEROUS: Duration = Duration::from_secs(10);
    /// For a server that never answers: the test waits this long, so keep it short.
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
            always_load: None,
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
        probe(&answers(INIT_REPLY), GENEROUS)
            .await
            .expect("probe ok");
    }

    #[tokio::test]
    async fn stdio_probe_fails_when_server_never_answers() {
        let err = probe(&sh("exec sleep 5"), SHORT).await.expect_err("wedged");
        assert!(err.to_string().contains("did not answer"), "got: {err}");
    }

    #[tokio::test]
    async fn stdio_probe_fails_when_process_exits_without_answering() {
        let err = probe(&sh("exit 3"), GENEROUS).await.expect_err("exited");
        assert!(err.to_string().contains("exited"), "got: {err}");
    }

    #[tokio::test]
    async fn stdio_probe_fails_when_command_is_missing() {
        let kind = ResolvedKind::Stdio {
            command: "llmenv-no-such-command".to_string(),
            args: Vec::new(),
            env: BTreeMap::new(),
        };
        let err = probe(&server("x", kind), GENEROUS)
            .await
            .expect_err("missing");
        assert!(err.to_string().contains("cannot start"), "got: {err}");
    }

    #[tokio::test]
    async fn stdio_probe_reports_a_jsonrpc_error_reply() {
        let reply = r#"{"jsonrpc":"2.0","id":0,"error":{"code":-32600,"message":"nope"}}"#;
        let err = probe(&answers(reply), GENEROUS)
            .await
            .expect_err("error reply");
        assert!(err.to_string().contains("nope"), "got: {err}");
    }

    #[tokio::test]
    async fn stdio_probe_passes_configured_env_to_the_server() {
        let script =
            format!("read line; [ \"$LLMENV_PROBE_X\" = 1 ] && printf '%s\\n' '{INIT_REPLY}'");
        let env = BTreeMap::from([("LLMENV_PROBE_X".to_string(), "1".to_string())]);
        probe(&sh_env(&script, env), GENEROUS)
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
            sh("exit 3"),
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
        let down = find_down(&servers, GENEROUS).await;
        let names: Vec<&str> = down.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(names, ["stdio-test", "gone"], "{down:?}");
        assert!(down.iter().all(|d| !d.reason.is_empty()));
    }

    /// A writer whose pipe is already closed.
    struct ClosedPipe;

    impl tokio::io::AsyncWrite for ClosedPipe {
        fn poll_write(
            self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
            _: &[u8],
        ) -> std::task::Poll<std::io::Result<usize>> {
            std::task::Poll::Ready(Err(std::io::ErrorKind::BrokenPipe.into()))
        }
        fn poll_flush(
            self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }
        fn poll_shutdown(
            self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }
    }

    #[tokio::test]
    async fn a_failed_write_is_reported_at_once_not_after_the_timeout() {
        // The reader never ends and never answers, like a wedged server with stdin closed.
        let (_keep_open, reader) = tokio::io::duplex(64);
        let outcome = tokio::time::timeout(
            Duration::from_secs(2),
            handshake_io(ClosedPipe, reader, "srv"),
        )
        .await
        .expect("the handshake must not wait for the reader after a failed write");
        let err = outcome.expect_err("failed write");
        assert!(err.to_string().contains("exited"), "{err}");
        assert!(err.to_string().contains("write failed"), "{err}");
    }

    #[tokio::test]
    async fn a_probe_that_crashes_counts_as_down_and_the_order_is_kept() {
        let servers = [
            server("ok-1", stdio_kind("true")),
            server("boom", stdio_kind("true")),
            server("late", stdio_kind("true")),
        ];
        let down = find_down_with(&servers, |s| async move {
            match s.name.as_str() {
                "boom" => panic!("probe bug"),
                "late" => Err(anyhow::anyhow!("no answer")),
                _ => Ok(()),
            }
        })
        .await;
        let names: Vec<&str> = down.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(names, ["boom", "late"]);
        assert!(down[0].reason.contains("crashed"), "{}", down[0].reason);
    }

    fn stdio_kind(command: &str) -> ResolvedKind {
        ResolvedKind::Stdio {
            command: command.to_string(),
            args: Vec::new(),
            env: BTreeMap::new(),
        }
    }

    #[test]
    fn restart_note_explains_each_outcome_that_leaves_memory_down() {
        let failed = restart_note(&ProxyStart::Failed("no mcp-proxy on PATH".to_string()));
        assert!(
            failed
                .as_deref()
                .is_some_and(|n| n.contains("no mcp-proxy on PATH"))
        );
        assert!(failed.is_some_and(|n| n.contains("could not start")));
        let running = restart_note(&ProxyStart::AlreadyRunning).expect("note");
        assert!(running.contains("does not answer"), "{running}");
        let remote = restart_note(&ProxyStart::NotLocal).expect("note");
        assert!(remote.contains("does not serve memory"), "{remote}");
        assert_eq!(restart_note(&ProxyStart::Started), None);
    }

    fn down_named(name: &str, reason: &str) -> DownServer {
        DownServer {
            name: name.to_string(),
            reason: reason.to_string(),
        }
    }

    #[test]
    fn merge_in_order_puts_the_memory_result_back_in_its_place() {
        let servers = [
            server(MEMORY_MCP_NAME, stdio_kind("true")),
            server(CODEBASE_MEMORY_MCP_NAME, stdio_kind("true")),
        ];
        let before = vec![
            down_named(MEMORY_MCP_NAME, "refused"),
            down_named(CODEBASE_MEMORY_MCP_NAME, "stuck"),
        ];
        let still_down = merge_in_order(
            &servers,
            before.clone(),
            vec![down_named(MEMORY_MCP_NAME, "wedged")],
        );
        let names: Vec<&str> = still_down.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(names, [MEMORY_MCP_NAME, CODEBASE_MEMORY_MCP_NAME]);
        assert_eq!(still_down[0].reason, "wedged");
        let recovered = merge_in_order(&servers, before, Vec::new());
        assert_eq!(recovered, [down_named(CODEBASE_MEMORY_MCP_NAME, "stuck")]);
    }

    #[test]
    fn tidy_reason_makes_one_clean_line() {
        let messy = "HTTP 500:\n<html>\u{1b}[31mboom\u{202E}\u{200B}</html>\r\n\tend";
        assert_eq!(tidy_reason(messy), "HTTP 500: <html> [31mboom</html> end");
    }

    #[test]
    fn tidy_reason_caps_the_length_and_marks_the_cut() {
        let long = "x".repeat(MAX_REASON_CHARS * 3);
        let tidy = tidy_reason(&long);
        assert_eq!(tidy.chars().count(), MAX_REASON_CHARS);
        assert!(tidy.ends_with('…'));
        let exact = "y".repeat(MAX_REASON_CHARS);
        assert_eq!(tidy_reason(&exact), exact);
    }

    #[tokio::test]
    async fn find_down_cleans_a_server_supplied_reason() {
        let servers = [server("srv", stdio_kind("true"))];
        let down = find_down_with(&servers, |_| async {
            Err(anyhow::anyhow!("body:\n{}\u{1b}[2J", "z".repeat(5_000)))
        })
        .await;
        assert!(
            down[0].reason.chars().count() <= MAX_REASON_CHARS,
            "{}",
            down[0].reason
        );
        assert!(
            !down[0].reason.chars().any(char::is_control),
            "{:?}",
            down[0].reason
        );
    }

    #[test]
    fn unresolved_notice_names_the_cause_and_the_fix() {
        let text = unresolved_notice(&anyhow::anyhow!("two entries are active"));
        assert!(text.contains("could not run"), "{text}");
        assert!(text.contains("two entries are active"), "{text}");
        assert!(text.contains("llmenv doctor"), "{text}");
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
            text.contains("pkill -f"),
            "ICM stop must match by command line: {text}"
        );
        assert!(
            !text.contains("kill $(cat"),
            "a recycled pid must not be signalled: {text}"
        );
        assert!(
            text.contains("cbm-daemon-internal"),
            "CBM fix missing: {text}"
        );
    }

    mod props {
        use super::super::parse_reply;
        use super::super::{MAX_REASON_CHARS, tidy_reason};
        use proptest::prelude::*;
        use serde_json::json;

        proptest! {
            #[test]
            fn a_reply_with_id_zero_and_a_result_is_success(
                result in prop_oneof![
                    Just(json!({})),
                    any::<i64>().prop_map(|n| json!(n)),
                    ".*".prop_map(|text| json!(text)),
                ],
            ) {
                let line = json!({"jsonrpc": "2.0", "id": 0, "result": result}).to_string();
                prop_assert!(matches!(parse_reply(&line), Some(Ok(()))));
            }

            #[test]
            fn an_error_wins_over_a_result_in_the_same_reply(msg in "[a-z ]{1,20}") {
                let line = json!({
                    "jsonrpc": "2.0", "id": 0, "result": {}, "error": {"code": -1, "message": msg}
                })
                .to_string();
                prop_assert!(matches!(parse_reply(&line), Some(Err(_))));
            }

            #[test]
            fn tidy_reason_is_bounded_single_line_and_idempotent(text in ".*") {
                let tidy = tidy_reason(&text);
                prop_assert!(tidy.chars().count() <= MAX_REASON_CHARS);
                prop_assert!(!tidy.chars().any(char::is_control));
                prop_assert_eq!(tidy_reason(&tidy), tidy);
            }

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
