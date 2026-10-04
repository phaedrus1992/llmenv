//! Line-delimited JSON-RPC over a child's stdio, and the one-line cleaner for a failure
//! reason that a server supplied. Shared by the text-limit probe and the session-start probe.

use anyhow::{Context, anyhow};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

/// The most characters of a failure reason that the notice carries. A server controls part of
/// the text, such as an HTTP error body, and the notice goes into the agent's context.
pub const MAX_REASON_CHARS: usize = 300;

/// Make `text` safe to put in the notice: one line, no control, bidirectional, or zero-width
/// characters, and at most [`MAX_REASON_CHARS`] characters.
pub fn tidy_reason(text: &str) -> String {
    let spaced: String = text
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let one_line = llmenv_util::strip_unsafe_chars(&spaced)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if one_line.chars().count() <= MAX_REASON_CHARS {
        return one_line;
    }
    let kept: String = one_line.chars().take(MAX_REASON_CHARS - 1).collect();
    format!("{kept}…")
}

/// Line-delimited JSON-RPC over a child's stdio: the send and receive path that the text-limit
/// probe uses (#2148). A reader skips lines that are not the reply to the request it sent, so a
/// banner or a notification is not mistaken for a failure.
pub struct StdioRpc<W, R> {
    writer: W,
    lines: tokio::io::Lines<BufReader<tokio::io::Take<R>>>,
    next_id: u64,
}

/// The most output a probe reads from one server (4 MiB). A server that never stops writing cannot
/// stall doctor or fill its memory.
const MAX_STDIO_OUTPUT_BYTES: u64 = 4_194_304;

impl<W, R> StdioRpc<W, R>
where
    W: tokio::io::AsyncWrite + Unpin,
    R: tokio::io::AsyncRead + Unpin,
{
    pub fn new(writer: W, reader: R) -> Self {
        Self {
            writer,
            lines: BufReader::new(tokio::io::AsyncReadExt::take(
                reader,
                MAX_STDIO_OUTPUT_BYTES,
            ))
            .lines(),
            next_id: 0,
        }
    }

    /// Send a request and return its `result`. The writer stays open between requests, because
    /// some servers stop at end of input.
    ///
    /// # Errors
    /// A write failure, an end of output before the reply, a JSON-RPC `error`, or a reply with
    /// no `result`.
    pub async fn request(&mut self, method: &str, params: Value) -> anyhow::Result<Value> {
        let id = self.next_id;
        self.next_id += 1;
        // A closed pipe means the server is gone. Waiting on its output would only hide that
        // behind a timeout.
        if let Err(e) = self
            .send(&json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }))
            .await
        {
            return Err(anyhow!(
                "the server exited before answering MCP {method} (write failed: {e})"
            ));
        }
        let mut skipped = 0usize;
        while let Some(line) = self
            .lines
            .next_line()
            .await
            .context("cannot read server output")?
        {
            let Ok(reply) = serde_json::from_str::<Value>(line.trim()) else {
                skipped += 1;
                continue;
            };
            // A request or notification from the server has a `method`; it is not our reply.
            if reply.get("id") != Some(&json!(id)) || reply.get("method").is_some() {
                continue;
            }
            if let Some(error) = reply.get("error") {
                let message = error
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("no message");
                let code = error
                    .get("code")
                    .map_or(String::new(), |c| format!(" (code {c})"));
                return Err(anyhow!("MCP {method} returned an error: {message}{code}"));
            }
            return reply
                .get("result")
                .cloned()
                .ok_or_else(|| anyhow!("MCP {method} reply has no result"));
        }
        let note = if skipped > 0 {
            format!(" ({skipped} lines of output were not JSON)")
        } else {
            String::new()
        };
        Err(anyhow!(
            "the server exited before answering MCP {method}{note}"
        ))
    }

    /// Send a notification, which has no reply.
    ///
    /// # Errors
    /// A write failure.
    pub async fn notify(&mut self, method: &str) -> anyhow::Result<()> {
        self.send(&json!({ "jsonrpc": "2.0", "method": method }))
            .await
            .with_context(|| format!("cannot send MCP {method}"))
    }

    async fn send(&mut self, message: &Value) -> std::io::Result<()> {
        self.writer
            .write_all(format!("{message}\n").as_bytes())
            .await?;
        self.writer.flush().await
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use std::time::Duration;

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

    /// A broken exchange waits for a reply forever, so bound it: the test then fails at once.
    async fn quickly<T>(future: impl std::future::Future<Output = T>) -> T {
        tokio::time::timeout(Duration::from_secs(3), future)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn stdio_rpc_matches_the_reply_by_id_and_returns_its_result() {
        let (client_w, server_r) = tokio::io::duplex(4096);
        let (mut server_w, client_r) = tokio::io::duplex(4096);
        tokio::spawn(async move {
            let mut lines = BufReader::new(server_r).lines();
            let line = lines.next_line().await.unwrap().unwrap();
            let request: Value = serde_json::from_str(&line).unwrap();
            assert_eq!(request["method"], "tools/list");
            assert_eq!(request["params"], json!({"a": 1}));
            server_w
                .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":7,\"result\":{\"wrong\":true}}\n")
                .await
                .unwrap();
            server_w
                .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":0,\"result\":{\"right\":true}}\n")
                .await
                .unwrap();
            std::future::pending::<()>().await;
        });
        let mut rpc = StdioRpc::new(client_w, client_r);
        let result = quickly(rpc.request("tools/list", json!({"a": 1})))
            .await
            .unwrap();
        assert_eq!(result, json!({"right": true}));
    }

    #[tokio::test]
    async fn stdio_rpc_sends_a_notification_without_an_id() {
        let (client_w, server_r) = tokio::io::duplex(4096);
        let (_server_w, client_r) = tokio::io::duplex(4096);
        let mut rpc = StdioRpc::new(client_w, client_r);
        quickly(rpc.notify("notifications/initialized"))
            .await
            .unwrap();
        let mut lines = BufReader::new(server_r).lines();
        let line = quickly(lines.next_line()).await.unwrap().unwrap();
        let sent: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(sent["method"], "notifications/initialized");
        assert!(sent.get("id").is_none());
    }

    #[tokio::test]
    async fn stdio_rpc_skips_server_requests_and_counts_non_json_lines() {
        let (client_w, _server_r) = tokio::io::duplex(4096);
        let (mut server_w, client_r) = tokio::io::duplex(4096);
        server_w
            .write_all(b"banner\n{\"jsonrpc\":\"2.0\",\"id\":0,\"method\":\"ping\"}\n")
            .await
            .unwrap();
        drop(server_w);
        let mut rpc = StdioRpc::new(client_w, client_r);
        let err = quickly(rpc.request("initialize", json!({})))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("exited before answering"), "{err}");
        assert!(err.contains("1 lines of output were not JSON"), "{err}");
    }

    #[tokio::test]
    async fn stdio_rpc_names_the_error_code() {
        let (client_w, _server_r) = tokio::io::duplex(4096);
        let (mut server_w, client_r) = tokio::io::duplex(4096);
        server_w
            .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":0,\"error\":{\"code\":-32601,\"message\":\"nope\"}}\n")
            .await
            .unwrap();
        let mut rpc = StdioRpc::new(client_w, client_r);
        let err = quickly(rpc.request("tools/list", json!({})))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("nope") && err.contains("-32601"), "{err}");
    }

    mod props {
        use super::super::{MAX_REASON_CHARS, tidy_reason};
        use proptest::prelude::*;

        proptest! {
            #[test]
            fn tidy_reason_is_bounded_single_line_and_idempotent(text in ".*") {
                let tidy = tidy_reason(&text);
                prop_assert!(tidy.chars().count() <= MAX_REASON_CHARS);
                prop_assert!(!tidy.chars().any(char::is_control));
                prop_assert_eq!(tidy_reason(&tidy), tidy);
            }
        }
    }
}
