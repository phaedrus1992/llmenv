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
    let mut cmd = tokio::process::Command::new("claude");
    cmd.arg("-p");
    let mut child = spawn_with_kill_on_drop(cmd)?;

    // Write prompt to stdin and close it
    if let Some(mut stdin) = child.stdin.take() {
        use tokio::io::AsyncWriteExt;
        stdin.write_all(prompt.as_bytes()).await?;
        // Drop stdin so the process can read EOF
        drop(stdin);
    }

    // Wait for output with timeout
    let output = wait_with_timeout_or_kill_group(child, LLM_TIMEOUT).await?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("claude -p exited with {}: {stderr}", output.status);
    }

    let stdout = String::from_utf8(output.stdout)?;
    Ok(stdout.trim().to_string())
}

/// Spawn `cmd` with piped stdio and `kill_on_drop` set. Without it, a child
/// whose future is dropped on timeout (e.g. `tokio::time::timeout` firing on
/// [`call_claude`]'s [`LLM_TIMEOUT`]) keeps running as an orphan — dropping a
/// `Child` handle is not termination (#1093, same root cause as the
/// `mcp-proxy` orphan fixed in #1087).
///
/// Also joins the child to its own process group (mirroring
/// [`crate::mcp::proxy::detach_process_group`]'s pattern) so
/// [`wait_with_timeout_or_kill_group`] can kill the whole group on timeout —
/// `kill_on_drop` alone only signals the direct pid, not any descendants the
/// child spawns (#1165).
fn spawn_with_kill_on_drop(
    mut cmd: tokio::process::Command,
) -> std::io::Result<tokio::process::Child> {
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    {
        cmd.process_group(0);
    }
    cmd.spawn()
}

/// Wait for `child` to exit, or kill its whole process group on `timeout`.
///
/// `kill_on_drop` (set by [`spawn_with_kill_on_drop`]) only signals `child`'s
/// own pid when the returned future is dropped — any descendants it spawned
/// (MCP servers, tool subprocesses) are not in that signal's blast radius and
/// survive as orphans. `spawn_with_kill_on_drop` makes `child` its own
/// process-group leader, so on timeout this sends `SIGKILL` to the whole
/// group (see [`kill_process_group`]) rather than relying on `kill_on_drop`
/// alone.
///
/// # Errors
/// Returns an error if `child` doesn't exit within `timeout` or if waiting on
/// it fails.
async fn wait_with_timeout_or_kill_group(
    child: tokio::process::Child,
    timeout: Duration,
) -> anyhow::Result<std::process::Output> {
    let pid = child.id();
    match tokio::time::timeout(timeout, child.wait_with_output()).await {
        Ok(result) => Ok(result?),
        Err(_elapsed) => {
            if let Some(pid) = pid {
                kill_process_group(pid);
            }
            anyhow::bail!("process (pid {pid:?}) timed out after {timeout:?}");
        }
    }
}

/// Whether `pid` is safe to negate for a group-kill syscall. Rejects `<= 0`
/// (not a valid pid, or 0 = the caller's own group) *and* `1`: negated for
/// `kill(2)`, pid 1 becomes `-1`, which the kernel special-cases as "every
/// process the caller may signal, except pid 1" rather than "process group
/// 1" — the exact broadcast disaster a `pid <= 0` guard alone would miss
/// (#1165, found during pre-pr-review's security-audit pass).
fn is_safe_kill_target(pid: i32) -> bool {
    pid > 1
}

/// Send `SIGKILL` to `pid`'s whole process group. `pid` must be a
/// process-group leader (its own pgid), as [`spawn_with_kill_on_drop`]
/// arranges via `process_group(0)` — killing an arbitrary pid's group could
/// otherwise take out unrelated siblings.
///
/// Goes through `rustix::process::kill_process_group` (a direct syscall)
/// rather than fork+exec'ing the `kill` binary: `claude -p` may already have
/// exited and been reaped by the time this runs (`kill_on_drop`'s own
/// drop-time kill fires first), so its pid could in principle be recycled
/// for an unrelated process before we signal it — a syscall closes that
/// window far tighter than paying `kill`'s fork+exec latency first would.
/// Best-effort: a failure here just means the timeout error below is the
/// only signal.
fn kill_process_group(pid: u32) {
    #[cfg(unix)]
    {
        let Ok(pid_i32) = i32::try_from(pid) else {
            return;
        };
        if !is_safe_kill_target(pid_i32) {
            return;
        }
        let Some(pid) = rustix::process::Pid::from_raw(pid_i32) else {
            return;
        };
        let _ = rustix::process::kill_process_group(pid, rustix::process::Signal::KILL);
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
    }
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
        let text = resp
            .text()
            .await
            .inspect_err(
                |e| tracing::warn!(error = %e, url = "https://api.anthropic.com/v1/messages", "failed to read consolidation error response body"),
            )
            .unwrap_or_else(|_| "(no body)".into());
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
            if rule.chars().count() > MAX_RULE_LENGTH {
                // Ponytail: truncate overlong rules with a marker. Truncates
                // by character count, not byte length — MAX_RULE_LENGTH is a
                // character bound, and byte-slicing panics when a multi-byte
                // char straddles the cut point (#1166).
                let truncated: String = rule.chars().take(MAX_RULE_LENGTH).collect();
                format!("{truncated}… (truncated)")
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
pub(crate) async fn run(
    config: &crate::config::Config,
    client: &McpHttpClient,
) -> anyhow::Result<String> {
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
#[expect(clippy::expect_used, reason = "test code")]
mod tests {
    use proptest::prelude::*;

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

    // #1166: MAX_RULE_LENGTH is documented as a *character* bound, but the
    // truncation sliced by *byte* index — a multi-byte char straddling byte
    // index MAX_RULE_LENGTH panics ("byte index is not a char boundary").
    // "x" (1 byte) then "é" (2 bytes) repeated puts the boundary mid-char.
    #[test]
    fn parse_bullets_truncates_multibyte_rule_without_panicking() {
        let long = "x".to_string() + &"é".repeat(MAX_RULE_LENGTH);
        let text = format!("- {long}");
        let bullets = parse_bullets(&text);
        assert_eq!(bullets.len(), 1);
        assert!(bullets[0].ends_with("… (truncated)"));
        assert_eq!(
            bullets[0].chars().count(),
            MAX_RULE_LENGTH + "… (truncated)".chars().count(),
            "truncation must count characters, not bytes"
        );
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

    /// Builds a minimal `Config` carrying only `max_rules_per_session`, the
    /// one field `build_prompt` reads.
    fn config_with_max_rules(max_rules_per_session: u32) -> crate::config::Config {
        let yaml = format!(
            "features:\n  memory:\n    - server_host: h\n      port: 1\n      \
             consolidation:\n        max_rules_per_session: {max_rules_per_session}\n"
        );
        serde_yaml::from_str(&yaml).expect("valid Config fixture YAML")
    }

    // -- child-process timeout lifecycle (#1093) --

    /// A timed-out LLM-backend child must not be orphaned: dropping the
    /// timeout future (which drops the `Child`) has to actually kill the
    /// process, not just close our handle to it. Uses `sleep 30` in place of
    /// `claude` so the test doesn't depend on the `claude` binary being
    /// installed.
    #[tokio::test]
    async fn timeout_kills_the_child_instead_of_orphaning_it() {
        let mut cmd = tokio::process::Command::new("sleep");
        cmd.arg("30");
        let mut child = spawn_with_kill_on_drop(cmd).expect("spawn sleep 30");
        let pid = child.id().expect("child has a pid");

        let result = tokio::time::timeout(Duration::from_millis(50), child.wait()).await;
        assert!(result.is_err(), "`sleep 30` should not exit within 50ms");

        drop(child); // triggers kill_on_drop if set

        // kill_on_drop's SIGKILL + async reap isn't instantaneous — poll with
        // a generous bound rather than asserting immediately after drop.
        let mut still_alive = true;
        for _ in 0..100 {
            if crate::mcp::proxy::is_alive(pid) != Some(true) {
                still_alive = false;
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(
            !still_alive,
            "pid {pid} was still alive 2s after dropping the timed-out child"
        );
    }

    /// #1165: `kill_on_drop` only signals the direct child pid — `claude -p`'s
    /// own descendants (MCP servers, tool subprocesses) are not touched by it.
    /// Uses `sh -c "sleep 30 & wait"` as a stand-in that spawns its own child,
    /// unlike the bare `sleep 30` above, so it can actually catch this: a fix
    /// that only kills the direct pid leaves the grandchild running.
    #[cfg(unix)]
    #[tokio::test]
    async fn timeout_kills_the_whole_process_group_not_just_the_direct_child() {
        let mut cmd = tokio::process::Command::new("sh");
        cmd.arg("-c").arg("sleep 30 & wait");
        let child = spawn_with_kill_on_drop(cmd).expect("spawn sh");
        let pid = child.id().expect("child has a pid");

        // Give the grandchild (`sleep 30`) time to actually spawn and join
        // the group before the timeout fires.
        tokio::time::sleep(Duration::from_millis(200)).await;

        let result = wait_with_timeout_or_kill_group(child, Duration::from_millis(50)).await;
        assert!(result.is_err(), "sh should not exit within 50ms");

        // No process anywhere should still carry this pgid — proves the
        // whole group (the direct `sh` child and its `sleep 30` grandchild)
        // was reaped, not just whatever kill_on_drop already covered.
        let mut group_alive = true;
        for _ in 0..100 {
            if !any_process_has_pgid(pid) {
                group_alive = false;
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(
            !group_alive,
            "process group {pid} still has members 2s after timeout"
        );
    }

    #[cfg(unix)]
    fn any_process_has_pgid(pgid: u32) -> bool {
        let Ok(out) = std::process::Command::new("ps")
            .args(["-eo", "pgid="])
            .output()
        else {
            return false;
        };
        let pgid = pgid.to_string();
        String::from_utf8_lossy(&out.stdout)
            .split_whitespace()
            .any(|p| p == pgid)
    }

    // #1165 (found during pre-pr-review, security-audit): a raw pid of 1
    // negated for a group-kill syscall becomes `kill(-1, sig)`, which the
    // kernel special-cases as "signal every process the caller may sign for,
    // except pid 1" — the same broadcast disaster a naive `pid <= 0` guard
    // was meant to prevent, just reached via a different value.
    #[test]
    fn is_safe_kill_target_rejects_broadcast_self_group_and_init() {
        assert!(!is_safe_kill_target(-5), "negative pid must be rejected");
        assert!(
            !is_safe_kill_target(0),
            "pid 0 (caller's own group) must be rejected"
        );
        assert!(
            !is_safe_kill_target(1),
            "pid 1 (would broadcast as -1) must be rejected"
        );
        assert!(is_safe_kill_target(2), "an ordinary pid must be accepted");
        assert!(
            is_safe_kill_target(12345),
            "an ordinary pid must be accepted"
        );
    }

    proptest! {
        /// Memory summaries recalled from ICM are arbitrary text as far as
        /// this function is concerned. No input should make the
        /// `.replace()` chain panic.
        #[test]
        fn build_prompt_never_panics(
            max_rules_per_session in 0u32..1000,
            summaries in proptest::collection::vec(".{0,50}", 0..5),
        ) {
            let config = config_with_max_rules(max_rules_per_session);
            let _ = build_prompt(&config, &summaries);
        }

        /// Every placeholder the prompt template declares (`{max_rules}`,
        /// `{summaries}`) must be fully consumed by the `.replace()` chain —
        /// none should survive into the built prompt.
        #[test]
        fn build_prompt_consumes_all_declared_placeholders(
            max_rules_per_session in 0u32..1000,
            junk in "[^{}]{0,10}",
        ) {
            let config = config_with_max_rules(max_rules_per_session);
            let out = build_prompt(&config, &[junk]);
            for token in ["{max_rules}", "{summaries}"] {
                prop_assert!(!out.contains(token), "placeholder {token} left unconsumed in {out:?}");
            }
        }

        /// The *substituted* values must be correct, not just that the
        /// placeholders are gone (that's the "consumed" test above). The
        /// `max_rules_per_session` number must render literally, and the
        /// summaries must appear joined by the same `\n---\n` delimiter
        /// `build_prompt` uses internally (#862).
        #[test]
        fn build_prompt_substitutes_correct_values(
            max_rules_per_session in 0u32..1000,
            summaries in proptest::collection::vec(".{0,50}", 0..5),
        ) {
            let config = config_with_max_rules(max_rules_per_session);
            let out = build_prompt(&config, &summaries);
            prop_assert!(
                out.contains(&max_rules_per_session.to_string()),
                "max_rules value {max_rules_per_session} missing from {out:?}"
            );
            let joined = summaries.join("\n---\n");
            prop_assert!(
                out.contains(&joined),
                "joined summaries {joined:?} missing from {out:?}"
            );
        }
    }

    // ===== #1166: property-test coverage for parse_bullets and parse_recall_output =====

    /// A single simulated line of `icm_memory_recall` output.
    fn arb_recall_line() -> impl Strategy<Value = String> {
        prop_oneof![
            Just("--- record ---".to_string()),
            "summary: .{0,20}".prop_map(|s| s),
            ".{0,20}".prop_map(|s| s),
        ]
    }

    proptest! {
        /// The model's raw text response is arbitrary as far as `parse_bullets`
        /// is concerned — no input should panic. Multi-line, multi-byte
        /// content is exactly the case #1166 found panicking.
        #[test]
        fn parse_bullets_never_panics(
            lines in proptest::collection::vec(".{0,600}", 0..10),
        ) {
            let text = lines.join("\n");
            let _ = parse_bullets(&text);
        }

        /// Every returned bullet is bounded by MAX_RULE_LENGTH characters
        /// (plus the truncation marker) regardless of the input's byte/char
        /// composition — the invariant the byte/char confusion violated.
        #[test]
        fn parse_bullets_never_exceeds_max_length(
            rule in ".{0,600}",
        ) {
            let text = format!("- {rule}");
            let bullets = parse_bullets(&text);
            let marker_len = "… (truncated)".chars().count();
            for bullet in &bullets {
                prop_assert!(
                    bullet.chars().count() <= MAX_RULE_LENGTH + marker_len,
                    "bullet {bullet:?} exceeds the length bound"
                );
            }
        }

        /// Arbitrary recall-output text — including delimiter lines with no
        /// `summary:` field, and `summary:` lines outside any delimiter —
        /// must never panic.
        #[test]
        fn parse_recall_output_never_panics(
            lines in proptest::collection::vec(arb_recall_line(), 0..10),
        ) {
            let text = lines.join("\n");
            let _ = parse_recall_output(&text);
        }

        /// A record can only be produced once a `--- ... ---` delimiter has
        /// opened it, so the output can never contain more records than
        /// there are delimiter lines in the input — true regardless of how
        /// many (or few) `summary:` lines follow each one.
        #[test]
        fn parse_recall_output_never_exceeds_delimiter_count(
            lines in proptest::collection::vec(arb_recall_line(), 0..10),
        ) {
            // Match parse_recall_output's own delimiter predicate, not the
            // literal synthetic delimiter string — a random ".{0,20}" junk
            // line could otherwise coincidentally match "--- ... ---" too.
            let delimiter_count = lines
                .iter()
                .filter(|l| {
                    let t = l.trim();
                    t.starts_with("--- ") && t.ends_with(" ---")
                })
                .count();
            let text = lines.join("\n");
            let records = parse_recall_output(&text);
            prop_assert!(records.len() <= delimiter_count);
        }
    }
}
