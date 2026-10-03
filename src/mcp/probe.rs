//! Measure the text an MCP server sends to the model (#2148).
//!
//! Claude Code cuts each tool description and each server's `initialize` instructions to a limit
//! (2,048 characters by default). The cut is silent. This module asks a server for that text and
//! counts it. Design: docs/design/issue-2148-mcp-description-cap.md

use std::time::Duration;

use anyhow::{Context, anyhow};
use serde_json::{Value, json};
use tokio::process::Command;

use crate::hook_run::mcp_client::{
    InitializeInfo, MAX_TOOL_PAGES, McpHttpClient, ToolSummary, parse_tools_page,
};
use crate::hook_run::mcp_health::StdioRpc;
use crate::mcp::resolve::{ResolvedKind, ResolvedMcp};

/// Claude Code's default cut for MCP tool descriptions and server instructions.
const CLAUDE_MCP_TEXT_LIMIT: usize = 2048;

/// How long one server may take to connect and answer both calls.
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// The limit `CLAUDE_CODE_MAX_MCP_DESCRIPTION_LENGTH` sets: 1 to 9 ASCII digits, not zero.
/// `None` for any other value, which Claude Code ignores.
pub(crate) fn parse_limit(value: &str) -> Option<usize> {
    (1..=9)
        .contains(&value.len())
        .then_some(value)
        .filter(|v| v.bytes().all(|b| b.is_ascii_digit()))
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|n| *n > 0)
}

/// The limit Claude Code applies, given the variable's value.
pub(crate) fn effective_limit(env_value: Option<&str>) -> usize {
    env_value
        .and_then(parse_limit)
        .unwrap_or(CLAUDE_MCP_TEXT_LIMIT)
}

/// The size of the text one server sends.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct McpTextReport {
    pub server: String,
    pub instructions_chars: Option<usize>,
    /// Tool name and description size in characters, in the server's order.
    pub tools: Vec<(String, usize)>,
}

/// Count the text of one server. A tool with no description counts as 0.
fn measure(server: &str, instructions: Option<&str>, tools: &[ToolSummary]) -> McpTextReport {
    McpTextReport {
        server: server.to_string(),
        instructions_chars: instructions.map(|text| text.chars().count()),
        tools: tools
            .iter()
            .map(|t| {
                let chars = t.description.as_deref().map_or(0, |d| d.chars().count());
                // A server controls the name, and doctor prints it to a terminal.
                (crate::util::strip_unsafe_chars(&t.name), chars)
            })
            .collect(),
    }
}

/// Probe `mcp` and measure its text. Only `initialize` and `tools/list` are called.
///
/// # Errors
/// The transport is not probed, the server does not answer in [`PROBE_TIMEOUT`], or a call fails.
pub(crate) async fn probe(mcp: &ResolvedMcp) -> anyhow::Result<McpTextReport> {
    match &mcp.kind {
        ResolvedKind::Remote {
            url,
            transport: crate::config::McpTransport::Http,
        } => {
            let work = async {
                let client = McpHttpClient::with_headers(url.clone(), PROBE_TIMEOUT, &mcp.headers)?;
                let info = client.initialize_info().await?;
                let tools = client.list_tools().await?;
                Ok(measure(&mcp.name, info.instructions.as_deref(), &tools))
            };
            tokio::time::timeout(PROBE_TIMEOUT, work)
                .await
                .unwrap_or_else(|_| Err(timeout_error()))
        }
        ResolvedKind::Remote { transport, .. } => {
            Err(anyhow!("transport {transport:?} is not probed"))
        }
        ResolvedKind::Stdio { command, args, env } => {
            let (info, tools) = probe_stdio(command, args, env).await?;
            Ok(measure(&mcp.name, info.instructions.as_deref(), &tools))
        }
    }
}

fn timeout_error() -> anyhow::Error {
    anyhow!("no answer within {} ms", PROBE_TIMEOUT.as_millis())
}

/// Start the server, run the handshake and `tools/list` on its stdio, and stop it.
async fn probe_stdio(
    command: &str,
    args: &[String],
    env: &std::collections::BTreeMap<String, String>,
) -> anyhow::Result<(InitializeInfo, Vec<ToolSummary>)> {
    let mut child = Command::new(command)
        .args(args)
        .envs(env)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        // Its own process group, so the stop below also reaches what a wrapper such as `npx`
        // started.
        .process_group(0)
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("cannot start `{command}`"))?;
    let stdin = child.stdin.take().context("child stdin is not piped")?;
    let stdout = child.stdout.take().context("child stdout is not piped")?;
    let mut stderr = child.stderr.take();
    // The timeout wraps the exchange only, so the stop below always runs: a dropped future would
    // leave the server and what it started running.
    let result = tokio::time::timeout(PROBE_TIMEOUT, stdio_exchange(StdioRpc::new(stdin, stdout)))
        .await
        .unwrap_or_else(|_| Err(timeout_error()));
    stop_group(&mut child, command).await;
    match result {
        Ok(ok) => Ok(ok),
        Err(e) => Err(match stderr_tail(stderr.as_mut()).await {
            Some(tail) => e.context(format!("the server wrote: {tail}")),
            None => e,
        }),
    }
}

/// Stop the child and its process group. `kill_on_drop` is only the backstop for an early return.
async fn stop_group(child: &mut tokio::process::Child, command: &str) {
    if let Some(pid) = child.id() {
        let group = format!("-{pid}");
        match Command::new("kill")
            .args(["-TERM", "--", &group])
            .output()
            .await
        {
            Ok(out) if out.status.success() => {}
            Ok(_) | Err(_) => tracing::debug!(command, "process group signal failed"),
        }
    }
    if let Err(e) = child.kill().await {
        tracing::warn!(command, error = %e, "cannot stop the MCP text probe process");
    }
}

/// What the stopped server left on stderr, as one clean line.
async fn stderr_tail(stderr: Option<&mut tokio::process::ChildStderr>) -> Option<String> {
    use tokio::io::AsyncReadExt as _;
    let stderr = stderr?;
    let mut buf = Vec::new();
    // The child is stopped, so the pipe ends at its last write. The timeout is a backstop for a
    // grandchild that still holds the pipe open.
    let _ = tokio::time::timeout(
        Duration::from_millis(200),
        (&mut *stderr).take(2048).read_to_end(&mut buf),
    )
    .await;
    let text = crate::hook_run::mcp_health::tidy_reason(&String::from_utf8_lossy(&buf));
    (!text.is_empty()).then_some(text)
}

/// The `initialize` and `tools/list` exchange, over any line-delimited JSON-RPC pipe.
async fn stdio_exchange<W, R>(
    mut rpc: StdioRpc<W, R>,
) -> anyhow::Result<(InitializeInfo, Vec<ToolSummary>)>
where
    W: tokio::io::AsyncWrite + Unpin,
    R: tokio::io::AsyncRead + Unpin,
{
    let init = rpc
        .request(
            "initialize",
            json!({
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": { "name": "llmenv-doctor", "version": env!("CARGO_PKG_VERSION") }
            }),
        )
        .await?;
    let info = InitializeInfo {
        instructions: init
            .get("instructions")
            .and_then(Value::as_str)
            .map(String::from),
    };
    rpc.notify("notifications/initialized").await?;
    let mut tools = Vec::new();
    let mut cursor: Option<String> = None;
    for _ in 0..MAX_TOOL_PAGES {
        let params = cursor
            .as_ref()
            .map_or_else(|| json!({}), |c| json!({ "cursor": c }));
        let page = rpc.request("tools/list", params).await?;
        let (listed, next) = parse_tools_page(&page)?;
        tools.extend(listed);
        cursor = next;
        if cursor.is_none() {
            return Ok((info, tools));
        }
    }
    Err(anyhow!(
        "tools/list returned more than {MAX_TOOL_PAGES} pages; stopped at the cap"
    ))
}

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "test code")]
mod tests {
    use super::*;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    /// A broken exchange waits for a reply forever, so bound it: the test then fails at once.
    async fn quickly<T>(future: impl std::future::Future<Output = T>) -> T {
        tokio::time::timeout(Duration::from_secs(3), future)
            .await
            .unwrap()
    }

    fn tool(name: &str, description: Option<&str>) -> ToolSummary {
        ToolSummary {
            name: name.into(),
            description: description.map(String::from),
        }
    }

    #[test]
    fn the_limit_comes_from_one_to_nine_digits_else_the_default() {
        for (value, want) in [
            (None, 2048),
            (Some("4096"), 4096),
            (Some("0"), 2048),
            (Some("abc"), 2048),
            (Some("4096 "), 2048),
            (Some(""), 2048),
            (Some("99999999999"), 2048),
            (Some("999999999"), 999_999_999),
            (Some("-5"), 2048),
            (Some("١٢٣"), 2048),
        ] {
            assert_eq!(effective_limit(value), want, "{value:?}");
        }
    }

    #[test]
    fn measure_counts_characters_and_treats_a_missing_description_as_zero() {
        let report = measure(
            "srv",
            Some(&"é".repeat(2048)),
            &[tool("a", Some("héllo")), tool("b", None)],
        );
        assert_eq!(report.server, "srv");
        assert_eq!(report.instructions_chars, Some(2048));
        assert_eq!(report.tools, [("a".to_string(), 5), ("b".to_string(), 0)]);
        assert_eq!(measure("s", None, &[]).instructions_chars, None);
    }

    /// A stub server on the far end of a pipe pair: it answers `initialize` and two pages of
    /// `tools/list`, and prints a banner line first.
    async fn stub_server<R, W>(reader: R, mut writer: W, pages: usize)
    where
        R: tokio::io::AsyncRead + Unpin,
        W: tokio::io::AsyncWrite + Unpin,
    {
        let mut lines = BufReader::new(reader).lines();
        writer.write_all(b"starting up\n").await.unwrap();
        while let Some(line) = lines.next_line().await.unwrap() {
            let request: Value = serde_json::from_str(&line).unwrap();
            let Some(id) = request.get("id").cloned() else {
                continue;
            };
            let result = match request["method"].as_str().unwrap() {
                "initialize" => json!({ "instructions": "use me well" }),
                _ => {
                    let page = if request["params"]["cursor"].is_null() {
                        1
                    } else {
                        2
                    };
                    let mut result = json!({
                        "tools": [{ "name": format!("t{page}"), "description": "d".repeat(page) }]
                    });
                    if page < pages {
                        result["nextCursor"] = "next".into();
                    }
                    result
                }
            };
            let reply = json!({ "jsonrpc": "2.0", "id": id, "result": result });
            writer
                .write_all(format!("{reply}\n").as_bytes())
                .await
                .unwrap();
        }
    }

    async fn exchange(pages: usize) -> anyhow::Result<(InitializeInfo, Vec<ToolSummary>)> {
        let (client_w, server_r) = tokio::io::duplex(4096);
        let (server_w, client_r) = tokio::io::duplex(4096);
        tokio::spawn(stub_server(server_r, server_w, pages));
        quickly(stdio_exchange(StdioRpc::new(client_w, client_r))).await
    }

    #[tokio::test]
    async fn the_stdio_exchange_reads_instructions_and_every_page() {
        let (info, tools) = exchange(2).await.unwrap();
        assert_eq!(info.instructions.as_deref(), Some("use me well"));
        let names: Vec<_> = tools.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, ["t1", "t2"]);
        assert_eq!(tools[1].description.as_deref(), Some("dd"));
    }

    #[tokio::test]
    async fn the_stdio_exchange_with_one_page_stops_there() {
        assert_eq!(exchange(1).await.unwrap().1.len(), 1);
    }

    #[tokio::test]
    async fn a_server_that_closes_its_output_gives_an_error() {
        let (client_w, _server_r) = tokio::io::duplex(4096);
        let (server_w, client_r) = tokio::io::duplex(4096);
        drop(server_w);
        let err = quickly(stdio_exchange(StdioRpc::new(client_w, client_r)))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("exited before answering"), "{err}");
        // No line of output was skipped, so the error must not claim any.
        assert!(!err.contains("not JSON"), "{err}");
    }

    #[tokio::test]
    async fn a_json_rpc_error_reply_is_an_error_with_its_message() {
        let (client_w, server_r) = tokio::io::duplex(4096);
        let (mut server_w, client_r) = tokio::io::duplex(4096);
        tokio::spawn(async move {
            let _keep_open = server_r;
            // A reply for another id comes first and must be skipped.
            server_w
                .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":99,\"result\":{}}\n")
                .await
                .unwrap();
            server_w
                .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":0,\"error\":{\"message\":\"nope\"}}\n")
                .await
                .unwrap();
            std::future::pending::<()>().await;
        });
        let err = quickly(stdio_exchange(StdioRpc::new(client_w, client_r)))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("initialize") && err.contains("nope"), "{err}");
    }

    #[tokio::test]
    async fn a_real_child_process_is_measured_and_stopped() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("stub.sh");
        let pid_file = dir.path().join("pid");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\necho $$ > '{}'\nwhile IFS= read -r line; do\n  case \"$line\" in\n    *'\"method\":\"initialize\"'*) echo '{{\"jsonrpc\":\"2.0\",\"id\":0,\"result\":{{\"instructions\":\"hello\"}}}}' ;;\n    *'\"method\":\"tools/list\"'*) echo '{{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{{\"tools\":[{{\"name\":\"x\",\"description\":\"abc\"}}]}}}}' ;;\n  esac\ndone\n",
                pid_file.display()
            ),
        )
        .unwrap();
        let mcp = ResolvedMcp {
            always_load: None,
            name: "stub".into(),
            kind: ResolvedKind::Stdio {
                command: "sh".into(),
                args: vec![script.display().to_string()],
                env: Default::default(),
            },
            headers: Default::default(),
            timeout: None,
            disabled_tools: vec![],
            mcp_permissions: None,
            memory_hook: None,
        };
        let report = probe(&mcp).await.unwrap();
        assert_eq!(report.instructions_chars, Some(5));
        assert_eq!(report.tools, [("x".to_string(), 3)]);
        let pid: i32 = std::fs::read_to_string(&pid_file)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        // Signal 0 checks that the process exists. The probe killed it, so it must not.
        let alive = std::process::Command::new("kill")
            .args(["-0", &pid.to_string()])
            .output()
            .unwrap()
            .status
            .success();
        assert!(!alive, "the probe left process {pid} running");
    }

    fn stdio_server(script_body: &str, dir: &std::path::Path) -> ResolvedMcp {
        let script = dir.join("stub.sh");
        std::fs::write(&script, format!("#!/bin/sh\n{script_body}")).unwrap();
        ResolvedMcp {
            always_load: None,
            name: "stub".into(),
            kind: ResolvedKind::Stdio {
                command: "sh".into(),
                args: vec![script.display().to_string()],
                env: Default::default(),
            },
            headers: Default::default(),
            timeout: None,
            disabled_tools: vec![],
            mcp_permissions: None,
            memory_hook: None,
        }
    }

    fn alive(pid: &str) -> bool {
        std::process::Command::new("kill")
            .args(["-0", pid])
            .output()
            .unwrap()
            .status
            .success()
    }

    #[tokio::test]
    async fn a_server_that_fails_to_start_shows_what_it_wrote_to_stderr() {
        let dir = tempfile::tempdir().unwrap();
        let mcp = stdio_server("echo 'missing API_TOKEN' >&2\nexit 1\n", dir.path());
        let err = format!("{:#}", probe(&mcp).await.unwrap_err());
        assert!(err.contains("missing API_TOKEN"), "{err}");
        assert!(err.contains("exited before answering"), "{err}");
    }

    #[tokio::test]
    async fn a_silent_server_times_out_and_is_still_stopped() {
        let dir = tempfile::tempdir().unwrap();
        let pid_file = dir.path().join("pid");
        let mcp = stdio_server(
            &format!("echo $$ > '{}'\nsleep 300\n", pid_file.display()),
            dir.path(),
        );
        let err = probe(&mcp).await.unwrap_err().to_string();
        assert!(err.contains("no answer within"), "{err}");
        let pid = std::fs::read_to_string(&pid_file)
            .unwrap()
            .trim()
            .to_string();
        let mut gone = false;
        for _ in 0..50 {
            if !alive(&pid) {
                gone = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(gone, "the probe left process {pid} running");
    }

    #[tokio::test]
    async fn the_probe_stops_a_process_the_server_started() {
        let dir = tempfile::tempdir().unwrap();
        let pid_file = dir.path().join("grandchild");
        let mcp = stdio_server(
            &format!(
                "sleep 300 &\necho $! > '{}'\nwhile IFS= read -r line; do\n  case \"$line\" in\n    *'\"method\":\"initialize\"'*) echo '{{\"jsonrpc\":\"2.0\",\"id\":0,\"result\":{{}}}}' ;;\n    *'\"method\":\"tools/list\"'*) echo '{{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{{\"tools\":[]}}}}' ;;\n  esac\ndone\n",
                pid_file.display()
            ),
            dir.path(),
        );
        probe(&mcp).await.unwrap();
        let pid = std::fs::read_to_string(&pid_file)
            .unwrap()
            .trim()
            .to_string();
        // The group signal is delivered at once, but the process may take a moment to exit.
        let mut gone = false;
        for _ in 0..50 {
            if !alive(&pid) {
                gone = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(gone, "the probe left process {pid} running");
    }

    #[tokio::test]
    async fn a_malformed_tool_in_a_stdio_reply_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let mcp = stdio_server(
            "while IFS= read -r line; do\n  case \"$line\" in\n    *'\"method\":\"initialize\"'*) echo '{\"jsonrpc\":\"2.0\",\"id\":0,\"result\":{}}' ;;\n    *'\"method\":\"tools/list\"'*) echo '{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"tools\":[{\"description\":\"x\"}]}}' ;;\n  esac\ndone\n",
            dir.path(),
        );
        let err = probe(&mcp).await.unwrap_err().to_string();
        assert!(err.contains("no string name"), "{err}");
    }

    #[test]
    fn tool_names_lose_control_and_invisible_characters() {
        let report = measure(
            "s",
            None,
            &[tool("a\u{1b}]52;c;x\u{7}\nb\u{202e}", Some("d"))],
        );
        assert_eq!(report.tools[0].0, "a]52;c;xb");
    }

    proptest::proptest! {
        #[test]
        fn the_limit_is_always_positive_and_follows_the_digit_rule(value in "\\PC{0,14}") {
            let got = effective_limit(Some(&value));
            proptest::prop_assert!(got > 0);
            let valid = (1..=9).contains(&value.len())
                && value.bytes().all(|b| b.is_ascii_digit())
                && value.parse::<usize>().is_ok_and(|n| n > 0);
            if valid {
                proptest::prop_assert_eq!(got, value.parse::<usize>().unwrap());
            } else {
                proptest::prop_assert_eq!(got, CLAUDE_MCP_TEXT_LIMIT);
            }
        }

        #[test]
        fn measure_counts_chars_keeps_order_and_zero_for_no_description(
            descs in proptest::collection::vec(proptest::option::of("\\PC{0,40}"), 0..8),
            instructions in proptest::option::of("\\PC{0,60}"),
        ) {
            let tools: Vec<ToolSummary> = descs
                .iter()
                .enumerate()
                .map(|(i, d)| tool(&format!("t{i}"), d.as_deref()))
                .collect();
            let report = measure("s", instructions.as_deref(), &tools);
            proptest::prop_assert_eq!(report.instructions_chars, instructions.map(|t| t.chars().count()));
            proptest::prop_assert_eq!(report.tools.len(), descs.len());
            for (i, (name, chars)) in report.tools.iter().enumerate() {
                proptest::prop_assert_eq!(name.clone(), format!("t{i}"));
                proptest::prop_assert_eq!(*chars, descs[i].as_deref().map_or(0, |d| d.chars().count()));
            }
        }
    }

    #[tokio::test]
    async fn a_command_that_does_not_exist_is_an_error_naming_it() {
        let mcp = ResolvedMcp {
            always_load: None,
            name: "gone".into(),
            kind: ResolvedKind::Stdio {
                command: "llmenv-no-such-binary".into(),
                args: vec![],
                env: Default::default(),
            },
            headers: Default::default(),
            timeout: None,
            disabled_tools: vec![],
            mcp_permissions: None,
            memory_hook: None,
        };
        let err = probe(&mcp).await.unwrap_err().to_string();
        assert!(err.contains("llmenv-no-such-binary"), "{err}");
    }

    #[tokio::test]
    async fn an_sse_server_is_not_probed() {
        let mcp = ResolvedMcp {
            always_load: None,
            name: "sse".into(),
            kind: ResolvedKind::Remote {
                url: "http://127.0.0.1:1/sse".into(),
                transport: crate::config::McpTransport::Sse,
            },
            headers: Default::default(),
            timeout: None,
            disabled_tools: vec![],
            mcp_permissions: None,
            memory_hook: None,
        };
        let err = probe(&mcp).await.unwrap_err().to_string();
        assert!(err.contains("not probed"), "{err}");
    }
}
