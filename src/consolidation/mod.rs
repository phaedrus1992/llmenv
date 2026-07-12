//! Post-session reflective memory consolidation (R5).
//!
//! ## LLM backends
//!
//! Two backends configured via `consolidation.backend`:
//!
//! - **`claude-cli`** (default) — calls `claude -p` as a subprocess. Works with
//!   a Claude subscription; no `ANTHROPIC_API_KEY` needed.
//! - **`anthropic-api`** — calls the Anthropic Messages API directly via HTTP.
//!   Requires `ANTHROPIC_API_KEY` and `ANTHROPIC_MODEL` env vars.
//!
//! ICM's `icm_memory_consolidate` MCP tool exists but requires both `topic`
//! and `summary` parameters and simply merges a topic's memories into one
//! record — it does **not** perform LLM summarization, so we handle that here.
//!
//! The pipeline:
//! 1. Recall recent memories from ICM (no type filter — broadest recall).
//! 2. Precondition: ≥3 records, otherwise skip with a diagnostic.
//! 3. Build ExpeL-inspired prompt from memory summaries.
//! 4. Call the configured LLM backend (120s timeout).
//! 5. Parse bullet-point rules from the response.
//! 6. Store each rule as `type: semantic`, `importance: high`.
//!
//! All failures are fail-soft: `tracing::warn!`, return `Ok(summary)`.

use std::process::Stdio;
use std::time::Duration;

use crate::hook_run::mcp_client::McpHttpClient;

/// Hard timeout for the LLM backend call.
const LLM_TIMEOUT: Duration = Duration::from_secs(120);
/// Minimum episodic records needed to trigger consolidation.
const MIN_RECORDS: usize = 3;
/// Maximum character length for a single rule bullet.
const MAX_RULE_LENGTH: usize = 500;
/// Default model for the `anthropic-api` backend.
const DEFAULT_MODEL: &str = "claude-sonnet-5-20250624";

/// ExpeL-inspired consolidation prompt (spec R5).
///
/// `{max_rules}` is substituted with `max_rules_per_session`.
/// `{summaries}` is substituted with the memory content.
const CONSOLIDATION_PROMPT: &str = "\
You are analyzing a collection of session memories from a software \
development tool.

Review the following session observations and extract 0-{max_rules} standing \
development rules or patterns that an LLM agent should follow in future \
sessions.

Focus on:
- Recurring patterns about how the project works
- Configuration or tool decisions that should persist
- Project conventions and preferences
- Gotchas and pitfalls to avoid
- Important decisions made during the session

Output each rule as a single bullet point starting with \"- \". Be specific \
and actionable.
Output nothing if no new rules emerge.

Session observations:
{summaries}";

/// A parsed memory record from the ICM recall output.
#[derive(Debug)]
struct MemoryRecord {
    summary: String,
}

/// Parse the non-compact `icm_memory_recall` output into structured records.
/// Extracts the `summary` field from each record.
fn parse_recall_output(text: &str) -> Vec<MemoryRecord> {
    let mut records = Vec::new();
    let mut current_summary: Option<String> = None;
    let mut in_record = false;

    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("--- ") && trimmed.ends_with(" ---") {
            if let Some(s) = current_summary.take() {
                records.push(MemoryRecord { summary: s });
            }
            in_record = true;
            current_summary = None;
        } else if in_record && let Some(rest) = trimmed.strip_prefix("summary:") {
            current_summary = Some(rest.trim().to_string());
        }
    }

    // Finalize the last record
    if let Some(s) = current_summary {
        records.push(MemoryRecord { summary: s });
    }

    records
}

/// Build the prompt body for the Anthropic API call.
fn build_prompt(config: &crate::config::Config, summaries: &[String]) -> String {
    let max_rules = config
        .features
        .as_ref()
        .and_then(|f| f.memory.iter().find_map(|m| m.consolidation.as_ref()))
        .map_or(10, |c| c.max_rules_per_session);

    let summaries_text = summaries.join("\n---\n");
    CONSOLIDATION_PROMPT
        .replace("{max_rules}", &max_rules.to_string())
        .replace("{summaries}", &summaries_text)
}

/// Call `claude -p` as a subprocess, piping the prompt to stdin.
///
/// This works with a Claude subscription (no `ANTHROPIC_API_KEY` needed).
///
/// # Errors
/// Returns `anyhow::Error` if the process fails to start, times out, or exits
/// with a non-zero status.
async fn call_claude(prompt: &str) -> anyhow::Result<String> {
    let mut child = tokio::process::Command::new("claude")
        .arg("-p")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    // Write prompt to stdin and close it
    if let Some(mut stdin) = child.stdin.take() {
        use tokio::io::AsyncWriteExt;
        stdin.write_all(prompt.as_bytes()).await?;
        // Drop stdin so the process can read EOF
        drop(stdin);
    }

    // Wait for output with timeout
    let output = tokio::time::timeout(LLM_TIMEOUT, child.wait_with_output()).await??;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("claude -p exited with {}: {stderr}", output.status);
    }

    let stdout = String::from_utf8(output.stdout)?;
    Ok(stdout.trim().to_string())
}

/// Make a non-streaming call to the Anthropic Messages API.
///
/// Requires `ANTHROPIC_API_KEY` and (optionally) `ANTHROPIC_MODEL` env vars.
///
/// # Errors
/// Returns `anyhow::Error` on HTTP failure, timeout, or malformed response.
async fn call_anthropic_api(prompt: &str) -> anyhow::Result<String> {
    let api_key = std::env::var("ANTHROPIC_API_KEY")?;
    let model = std::env::var("ANTHROPIC_MODEL").unwrap_or_else(|_| DEFAULT_MODEL.to_string());

    let client = reqwest::Client::builder().timeout(LLM_TIMEOUT).build()?;

    let body = serde_json::json!({
        "model": model,
        "max_tokens": 4096,
        "messages": [{
            "role": "user",
            "content": prompt
        }]
    });

    let resp = client
        .post("https://api.anthropic.com/v1/messages")
        .header("x-api-key", &api_key)
        .header("anthropic-version", "2023-06-01")
        .header("content-type", "application/json")
        .json(&body)
        .send()
        .await?;

    let status = resp.status();
    if !status.is_success() {
        let text = resp.text().await.unwrap_or_else(|_| "(no body)".into());
        anyhow::bail!("Anthropic API returned {status}: {text}");
    }

    let json: serde_json::Value = resp.json().await?;
    let text = json["content"]
        .as_array()
        .and_then(|arr| arr.first())
        .and_then(|block| block["text"].as_str())
        .ok_or_else(|| anyhow::anyhow!("unexpected Anthropic API response shape"))?;

    Ok(text.to_string())
}

/// Parse bullet-point rules from the model's text output.
///
/// Returns lines that start with `- ` (dash-space), trimming whitespace.
/// Empty output → no rules → no store calls (success, not an error).
fn parse_bullets(text: &str) -> Vec<String> {
    text.lines()
        .map(|l| l.trim())
        .filter(|l| l.starts_with("- ") && l.len() > 2)
        .map(|l| {
            let rule = l[2..].trim();
            if rule.len() > MAX_RULE_LENGTH {
                // Ponytail: truncate overlong rules with a marker.
                format!("{}… (truncated)", &rule[..MAX_RULE_LENGTH])
            } else {
                rule.to_string()
            }
        })
        .collect()
}

/// Store a single consolidation rule via `icm_memory_store`.
async fn store_rule(client: &McpHttpClient, rule: &str) -> anyhow::Result<()> {
    let args = serde_json::json!({
        "content": rule,
        "topic": "llmenv-consolidation",
        "type": "semantic",
        "importance": "high",
    });
    client.call_tool("icm_memory_store", args).await?;
    Ok(())
}

/// Run post-session consolidation if enabled.
///
/// Recalls recent memories from the ICM backend, preconditions ≥3 records,
/// calls the Anthropic Messages API for distillation, and stores the
/// resulting rules as semantic/high memories.
///
/// # Errors
/// All errors are caught and logged via `tracing::warn!` — this function
/// always returns `Ok(summary)` to match the fail-soft contract.
pub async fn run(config: &crate::config::Config, client: &McpHttpClient) -> anyhow::Result<String> {
    let Some(cc) = config
        .features
        .as_ref()
        .and_then(|f| f.memory.iter().find_map(|m| m.consolidation.as_ref()))
        .filter(|c| c.enabled)
    else {
        return Ok(String::new());
    };

    tracing::info!(
        max_rules = cc.max_rules_per_session,
        backend = ?cc.backend,
        "running post-session consolidation"
    );

    // Step 1: Recall recent memories
    let recall_result = tracing::debug_span!("consolidation_recall")
        .in_scope(|| async {
            client
                .call_tool(
                    "icm_memory_recall",
                    serde_json::json!({
                        "query": "",
                        "limit": 50,
                    }),
                )
                .await
        })
        .await;

    let output = match recall_result {
        Ok(out) => out,
        Err(e) => {
            let msg = format!("consolidation: recall failed (fail-soft): {e}");
            tracing::warn!("{msg}");
            return Ok(msg);
        }
    };

    let records = parse_recall_output(&output);

    // Step 2: Precondition check
    if records.len() < MIN_RECORDS {
        let msg = format!(
            "consolidation: skipping — only {} record(s) found, need at least {MIN_RECORDS}",
            records.len(),
        );
        tracing::debug!("{msg}");
        return Ok(msg);
    }

    tracing::info!(
        count = records.len(),
        "consolidation: recalling {} memory records",
        records.len(),
    );

    // Collect summaries for the prompt
    let summaries: Vec<String> = records.iter().map(|r| r.summary.clone()).collect();

    // Step 3: Build the prompt
    let prompt = build_prompt(config, &summaries);

    // Step 4: Call the configured LLM backend
    let llm_result = tracing::debug_span!("consolidation_llm_call")
        .in_scope(|| async {
            use crate::config::ConsolidationBackend;
            match cc.backend {
                ConsolidationBackend::ClaudeCli => call_claude(&prompt).await,
                ConsolidationBackend::AnthropicApi => call_anthropic_api(&prompt).await,
            }
        })
        .await;

    let llm_output = match llm_result {
        Ok(out) => out,
        Err(e) => {
            let msg = format!("consolidation: LLM call failed (fail-soft): {e}");
            tracing::warn!("{msg}");
            return Ok(msg);
        }
    };

    // Step 5: Parse bullet points
    let rules = parse_bullets(&llm_output);

    if rules.is_empty() {
        let msg = format!(
            "consolidation: LLM returned no rules (parsed {} records, {:.0} tokens)",
            records.len(),
            prompt.len() as f64 / 4.0,
        );
        tracing::debug!("{msg}");
        return Ok(msg);
    }

    // Enforce max_rules client-side (spec R5)
    let max_rules = cc.max_rules_per_session as usize;
    let rules: Vec<&str> = rules.iter().map(|s| s.as_str()).take(max_rules).collect();

    // Step 6: Store each rule
    let mut stored = 0usize;
    for rule in &rules {
        match store_rule(client, rule).await {
            Ok(()) => stored += 1,
            Err(e) => {
                tracing::warn!("consolidation: failed to store rule (fail-soft): {e}");
            }
        }
    }

    let msg = format!(
        "consolidation: distilled {} memory records into {} semantic rule(s) \
         (backend: {:?}, rules stored: {stored})",
        records.len(),
        rules.len(),
        cc.backend,
    );
    tracing::info!("{msg}");
    Ok(msg)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn parse_recall_output_empty() {
        assert!(parse_recall_output("").is_empty());
    }

    #[test]
    fn parse_recall_output_single_record() {
        let text = "--- abc123 ---\n  topic: test\n  importance: high\n  weight: 0.85\n  summary: observed that the project uses Rust\n  keywords: test\n  score: 0.9\n";
        let records = parse_recall_output(text);
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].summary, "observed that the project uses Rust");
    }

    #[test]
    fn parse_recall_output_multiple_records() {
        let text = "--- id-1 ---\n  summary: first observation\n  weight: 0.1\n--- id-2 ---\n  summary: second observation\n  weight: 0.99\n";
        let records = parse_recall_output(text);
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].summary, "first observation");
        assert_eq!(records[1].summary, "second observation");
    }

    #[test]
    fn parse_bullets_empty_text() {
        assert!(parse_bullets("").is_empty());
    }

    #[test]
    fn parse_bullets_only_prose() {
        let text = "This is just a paragraph of text.\nNo bullet points here.";
        assert!(parse_bullets(text).is_empty());
    }

    #[test]
    fn parse_bullets_single() {
        let text = "- Use Rust for all new projects";
        let bullets = parse_bullets(text);
        assert_eq!(bullets, vec!["Use Rust for all new projects"]);
    }

    #[test]
    fn parse_bullets_multiple() {
        let text = "- First rule\n- Second rule\nSome prose in between\n- Third rule";
        let bullets = parse_bullets(text);
        assert_eq!(bullets, vec!["First rule", "Second rule", "Third rule"]);
    }

    #[test]
    fn parse_bullets_respects_max_rule_length() {
        let long = "x".repeat(MAX_RULE_LENGTH + 10);
        let text = format!("- {long}");
        let bullets = parse_bullets(&text);
        assert_eq!(bullets.len(), 1);
        assert!(bullets[0].ends_with("… (truncated)"));
        assert!(bullets[0].len() <= MAX_RULE_LENGTH + "… (truncated)".len());
    }

    #[test]
    fn parse_bullets_strips_leading_dash_space() {
        let text = "-  hello world";
        let bullets = parse_bullets(text);
        assert_eq!(bullets, vec!["hello world"]);
    }

    #[test]
    fn parse_recall_output_missing_summary_skipped() {
        let text = "--- id-1 ---\n  importance: high\n  weight: 0.5\n";
        let records = parse_recall_output(text);
        assert_eq!(records.len(), 0);
    }

    #[test]
    fn parse_recall_output_empty_summary_creates_record() {
        let text = "--- id-1 ---\n  summary:\n  weight: 0.5\n";
        let records = parse_recall_output(text);
        assert_eq!(records.len(), 1);
        assert!(records[0].summary.is_empty());
    }
}
