//! Measure the text an MCP server sends to the model (#2148).
//!
//! Claude Code cuts each tool description and each server's `initialize` instructions to a limit
//! (2,048 characters by default). The cut is silent. This module asks a server for that text and
//! counts it. Design: docs/design/issue-2148-mcp-description-cap.md

use std::time::Duration;

use anyhow::{Context, anyhow};
use serde_json::{Value, json};
use tokio::process::Command;

use crate::hook_run::mcp_client::{InitializeInfo, MAX_TOOL_PAGES, McpHttpClient, ToolSummary};
use crate::hook_run::mcp_health::StdioRpc;
use crate::mcp::resolve::{ResolvedKind, ResolvedMcp};

/// Claude Code's default cut for MCP tool descriptions and server instructions.
pub(crate) const CLAUDE_MCP_TEXT_LIMIT: usize = 2048;

/// How long one server may take to connect and answer both calls.
pub(crate) const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// The limit `CLAUDE_CODE_MAX_MCP_DESCRIPTION_LENGTH` sets: 1 to 9 ASCII digits, not zero.
/// Anything else is ignored, as Claude Code ignores it.
pub(crate) fn effective_limit(env_value: Option<&str>) -> usize {
    env_value
        .filter(|v| (1..=9).contains(&v.len()) && v.bytes().all(|b| b.is_ascii_digit()))
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|n| *n > 0)
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
pub(crate) fn measure(
    server: &str,
    instructions: Option<&str>,
    tools: &[ToolSummary],
) -> McpTextReport {
    McpTextReport {
        server: server.to_string(),
        instructions_chars: instructions.map(|text| text.chars().count()),
        tools: tools
            .iter()
            .map(|t| {
                let chars = t.description.as_deref().map_or(0, |d| d.chars().count());
                (t.name.clone(), chars)
            })
            .collect(),
    }
}

/// Probe `mcp` and measure its text. Only `initialize` and `tools/list` are called.
///
/// # Errors
/// The transport is not probed, the server does not answer in [`PROBE_TIMEOUT`], or a call fails.
pub(crate) async fn probe(mcp: &ResolvedMcp) -> anyhow::Result<McpTextReport> {
    let work = async {
        match &mcp.kind {
            ResolvedKind::Remote {
                url,
                transport: crate::config::McpTransport::Http,
            } => {
                let client = McpHttpClient::with_headers(url.clone(), PROBE_TIMEOUT, &mcp.headers)?;
                let info = client.initialize_info().await?;
                let tools = client.list_tools().await?;
                Ok(measure(&mcp.name, info.instructions.as_deref(), &tools))
            }
            ResolvedKind::Remote { transport, .. } => {
                Err(anyhow!("transport {transport:?} is not probed"))
            }
            ResolvedKind::Stdio { command, args, env } => {
                let (info, tools) = probe_stdio(command, args, env).await?;
                Ok(measure(&mcp.name, info.instructions.as_deref(), &tools))
            }
        }
    };
    tokio::time::timeout(PROBE_TIMEOUT, work)
        .await
        .unwrap_or_else(|_| Err(anyhow!("no answer within {} ms", PROBE_TIMEOUT.as_millis())))
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
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("cannot start `{command}`"))?;
    let stdin = child.stdin.take().context("child stdin is not piped")?;
    let stdout = child.stdout.take().context("child stdout is not piped")?;
    let result = stdio_exchange(StdioRpc::new(stdin, stdout)).await;
    // Reap the process now. `kill_on_drop` is only the backstop for an early return.
    if let Err(e) = child.kill().await {
        tracing::warn!(command, error = %e, "cannot stop the MCP text probe process");
    }
    result
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
        let listed = page
            .get("tools")
            .and_then(Value::as_array)
            .context("tools/list reply has no tools array")?;
        tools.extend(listed.iter().filter_map(|t| {
            Some(ToolSummary {
                name: t.get("name")?.as_str()?.to_string(),
                description: t
                    .get("description")
                    .and_then(Value::as_str)
                    .map(String::from),
            })
        }));
        cursor = page
            .get("nextCursor")
            .and_then(Value::as_str)
            .filter(|c| !c.is_empty())
            .map(String::from);
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
        stdio_exchange(StdioRpc::new(client_w, client_r)).await
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
        let err = stdio_exchange(StdioRpc::new(client_w, client_r))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("exited before answering"), "{err}");
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
        let err = stdio_exchange(StdioRpc::new(client_w, client_r))
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
