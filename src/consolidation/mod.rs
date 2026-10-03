//! Post-session reflective memory consolidation (R5).
//!
//! ## LLM backends
//!
//! Two backends configured via `consolidation.backend`:
//!
//! - **`claude-cli`** (default) — calls `claude -p` as a subprocess. Works with
//!   a Claude subscription; no `ANTHROPIC_API_KEY` needed.
//! - **`anthropic-api`** — calls the Anthropic Messages API directly via HTTP.
//!   Requires `ANTHROPIC_API_KEY`. `ANTHROPIC_MODEL` is optional and must be a
//!   full model id; the default is `claude-sonnet-5`.
//!
//! ICM's `icm_memory_consolidate` MCP tool exists but requires both `topic`
//! and `summary` parameters and simply merges a topic's memories into one
//! record — it does **not** perform LLM summarization, so we handle that here.
//!
//! The pipeline:
//! 1. Recall recent memories of the current project from ICM (no type filter).
//! 2. Precondition: ≥3 records, otherwise skip with a diagnostic.
//! 3. Build ExpeL-inspired prompt from memory summaries.
//! 4. Call the configured LLM backend (120s timeout).
//! 5. Parse bullet-point rules from the response.
//! 6. Store each rule as `type: semantic`, `importance: high`, in the project's
//!    rule topic, unless a matching rule is already stored there (#2387).
//!
//! All failures are fail-soft: `tracing::error!`, return `Ok(summary)`. The
//! detached child's log filter is ERROR-only by default, so a `warn!` here
//! would leave no trace (#2355).

use std::process::Stdio;
use std::time::Duration;

use crate::hook_run::checkpoint;
use crate::hook_run::idempotency;
use crate::hook_run::mcp_client::McpHttpClient;

mod dedup;

/// Hard timeout for the LLM backend call.
const LLM_TIMEOUT: Duration = Duration::from_secs(120);
/// Minimum episodic records needed to trigger consolidation.
const MIN_RECORDS: usize = 3;
/// Maximum character length for a single rule bullet.
const MAX_RULE_LENGTH: usize = 500;
/// Default model for the `anthropic-api` backend.
const DEFAULT_MODEL: &str = "claude-sonnet-5";

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
    topic: Option<String>,
}

/// Parse the non-compact `icm_memory_recall` output into structured records.
/// Extracts the `summary` field from each record.
fn parse_recall_output(text: &str) -> Vec<MemoryRecord> {
    let mut records = Vec::new();
    // A record counts only when it carries a `summary:` line.
    let mut summary: Option<String> = None;
    let mut topic: Option<String> = None;
    let mut in_record = false;

    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("--- ") && trimmed.ends_with(" ---") {
            records.extend(summary.take().map(|summary| MemoryRecord {
                summary,
                topic: topic.take(),
            }));
            topic = None;
            in_record = true;
        } else if in_record && let Some(rest) = trimmed.strip_prefix("summary:") {
            summary = Some(rest.trim().to_string());
        } else if in_record && let Some(rest) = trimmed.strip_prefix("topic:") {
            topic = Some(rest.trim().to_string());
        }
    }

    records.extend(summary.map(|summary| MemoryRecord { summary, topic }));
    records
}

/// Build the prompt body for the Anthropic API call.
fn build_prompt(max_rules: u32, summaries: &[String]) -> String {
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
    let mut child = spawn_with_kill_on_drop(claude_command())?;

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

/// Env var set on the `claude -p` child. `hook-run` exits at once when it
/// sees it, so the child can never start consolidation again (#2355).
pub(crate) const CHILD_GUARD_ENV: &str = "LLMENV_CONSOLIDATION_CHILD";

/// The isolated `claude -p` call. The child inherits `CLAUDE_CONFIG_DIR`, so
/// without these flags it runs every llmenv hook and MCP server, and its own
/// `SessionEnd` hook starts consolidation again (#2355).
///
/// The user's settings still load, because they can hold the auth (`env`,
/// `apiKeyHelper`, a Bedrock or Vertex provider). `--bare` and
/// `--setting-sources ""` both drop that auth, and `--bare` also drops the
/// OAuth login this backend exists for. `disableAllHooks` stops the hooks,
/// and the default output style keeps the reply in the bullet format that
/// [`parse_bullets`] reads.
fn claude_command() -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new("claude");
    cmd.args([
        "-p",
        "--settings",
        r#"{"disableAllHooks":true,"outputStyle":"default"}"#,
        // No --mcp-config is given, so no MCP server starts.
        "--strict-mcp-config",
        "--tools",
        "",
        "--disable-slash-commands",
        "--no-session-persistence",
    ])
    .env(CHILD_GUARD_ENV, "1");
    cmd
}

/// The consolidation settings of the active memory entry: the first entry in
/// `memory` (top-level plus bundle-contributed) whose `when` has an active
/// tag ([`crate::mcp::resolve::memory_is_tag_active`], the selection rule),
/// when its consolidation is enabled. `resolve_mcps` rejects two active
/// entries, so the first one is the only one.
#[must_use]
pub(crate) fn active_consolidation<'a>(
    memory: &'a [crate::config::Memory],
    active_tags: &std::collections::BTreeSet<String>,
) -> Option<&'a crate::config::ConsolidationConfig> {
    memory
        .iter()
        .find(|m| crate::mcp::resolve::memory_is_tag_active(m, active_tags))
        .and_then(|m| m.consolidation.as_ref())
        .filter(|c| c.enabled)
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

/// Pick the model for the `anthropic-api` backend from `ANTHROPIC_MODEL`.
///
/// Claude Code also reads `ANTHROPIC_MODEL` and accepts aliases such as `opus`. The Messages
/// API rejects an alias, so only a value that starts with `claude-` is used. Returns the model
/// and, when the value was rejected, a warning for the caller to log.
fn resolve_api_model(env_value: Option<&str>) -> (String, Option<String>) {
    match env_value {
        None => (DEFAULT_MODEL.to_string(), None),
        Some(value) if value.starts_with("claude-") => (value.to_string(), None),
        Some(value) => (
            DEFAULT_MODEL.to_string(),
            Some(format!(
                "consolidation: ignoring ANTHROPIC_MODEL=\"{value}\": the Messages API needs a \
                 full model ID such as claude-sonnet-5, not a Claude Code alias. \
                 Using {DEFAULT_MODEL}."
            )),
        ),
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
    let (model, warning) = resolve_api_model(std::env::var("ANTHROPIC_MODEL").ok().as_deref());
    if let Some(warning) = warning {
        tracing::error!("{warning}");
    }

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
                |e| tracing::error!(error = %e, url = "https://api.anthropic.com/v1/messages", "failed to read consolidation error response body"),
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

/// What names one consolidation run: the project, and the id of the run that started it.
struct RunScope<'a> {
    project: &'a str,
    run_id: &'a str,
}

/// The id of this run: the nonce its checkpoint was written with, or empty without a checkpoint.
fn run_id(checkpoint: Option<&std::path::Path>) -> String {
    checkpoint
        .and_then(|p| checkpoint::load(p).ok())
        .and_then(|c| c.inputs["nonce"].as_str().map(str::to_string))
        .unwrap_or_default()
}

/// Store a single consolidation rule via `icm_memory_store`, unless this exact rule was already
/// stored for `project` (#2397). The seen-set catches a resumed run that repeats a stored rule;
/// the similarity check in [`store_new_rules`] stays as the second line of defense.
async fn store_rule(
    client: &McpHttpClient,
    state_dir: Option<&std::path::Path>,
    run: &RunScope<'_>,
    rule: &str,
) -> anyhow::Result<()> {
    let project = run.project;
    // The run id keeps a rule that the user later forgot from being skipped as already stored; it
    // is the same for a resumed run, which is the repeat the seen-set exists to catch.
    let id = idempotency::request_id(&[project, "consolidation", run.run_id, rule]);
    let guard = idempotency::Guard::new(state_dir, project, &id);
    if guard.as_ref().is_some_and(idempotency::Guard::already_done) {
        return Ok(());
    }
    let args = serde_json::json!({
        "content": rule,
        "topic": dedup::rule_topic(project),
        "type": "semantic",
        "importance": "high",
        "keywords": [format!("request:{id}")],
    });
    client.call_tool("icm_memory_store", args).await?;
    if let Some(guard) = &guard {
        guard.done();
    }
    Ok(())
}

/// Store each rule that no stored rule or earlier rule in `rules` matches.
/// Returns the number stored and the number skipped as duplicates.
async fn store_new_rules(
    client: &McpHttpClient,
    state_dir: Option<&std::path::Path>,
    run: &RunScope<'_>,
    rules: &[&str],
) -> (usize, usize) {
    let project = run.project;
    let mut stored = 0usize;
    let mut duplicates = 0usize;
    let mut batch: Vec<&str> = Vec::new();
    for rule in rules.iter().copied() {
        match dedup::is_stored(client, project, rule).await {
            Ok(false) if !dedup::matches_any(rule, batch.iter().copied()) => {}
            Ok(_) => {
                duplicates += 1;
                continue;
            }
            Err(e) => {
                tracing::error!(
                    project,
                    "consolidation: duplicate check failed, rule skipped: {e:#}"
                );
                continue;
            }
        }
        batch.push(rule);
        match store_rule(client, state_dir, run, rule).await {
            Ok(()) => stored += 1,
            Err(e) => {
                tracing::error!("consolidation: failed to store rule (fail-soft): {e:#}");
            }
        }
    }
    (stored, duplicates)
}

/// How far the first half of a consolidation run got.
enum Distilled {
    /// The model returned a summary; `records` is the number of memories it read, 0 on a resume.
    Summary { text: String, records: usize },
    /// The run ends here. `settled` is true when nothing is left to retry (too few records, or no
    /// rules), and false for a failure that a later run may fix.
    Stop { msg: String, settled: bool },
}

/// Steps 1-4: recall recent memories, check the precondition, and ask the model for rules. On a
/// model success the summary goes into the checkpoint, so a resume after a failed store never
/// pays for the model again (#2396).
async fn distill(
    cc: &crate::config::ConsolidationConfig,
    client: &McpHttpClient,
    project: &str,
    checkpoint: Option<&std::path::Path>,
) -> Distilled {
    let recall_result = tracing::debug_span!("consolidation_recall")
        .in_scope(|| async {
            client
                .call_tool(
                    "icm_memory_recall",
                    serde_json::json!({
                        "query": "",
                        "project": project,
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
            tracing::error!("{msg}");
            return Distilled::Stop {
                msg,
                settled: false,
            };
        }
    };

    let records = parse_recall_output(&output);
    if records.len() < MIN_RECORDS {
        let msg = format!(
            "consolidation: skipping — only {} record(s) found, need at least {MIN_RECORDS}",
            records.len(),
        );
        tracing::debug!("{msg}");
        return Distilled::Stop { msg, settled: true };
    }
    tracing::info!(
        count = records.len(),
        "consolidation: recalling {} memory records",
        records.len(),
    );

    let summaries: Vec<String> = records.iter().map(|r| r.summary.clone()).collect();
    let prompt = build_prompt(cc.max_rules_per_session, &summaries);
    let llm_result = tracing::debug_span!("consolidation_llm_call")
        .in_scope(|| async {
            use crate::config::ConsolidationBackend;
            match cc.backend {
                ConsolidationBackend::ClaudeCli => call_claude(&prompt).await,
                ConsolidationBackend::AnthropicApi => call_anthropic_api(&prompt).await,
            }
        })
        .await;

    match llm_result {
        Ok(text) => {
            note_phase(checkpoint, "summarized", Some(&text));
            Distilled::Summary {
                text,
                records: records.len(),
            }
        }
        Err(e) => {
            let msg = format!("consolidation: LLM call failed (fail-soft): {e}");
            tracing::error!("{msg}");
            Distilled::Stop {
                msg,
                settled: false,
            }
        }
    }
}

/// Record the phase (and the model summary, once there is one) in the checkpoint. Fail-soft.
fn note_phase(checkpoint: Option<&std::path::Path>, phase: &str, summary: Option<&str>) {
    let Some(path) = checkpoint else {
        return;
    };
    let result = checkpoint::update(path, |c| {
        c.phase = phase.to_string();
        if let Some(summary) = summary {
            c.inputs["summary"] = serde_json::json!(summary);
        }
    });
    if let Err(e) = result {
        tracing::error!("consolidation: cannot update the checkpoint: {e:#}");
    }
}

/// The model summary a previous run saved, when this run resumes after the model step.
fn saved_summary(checkpoint: Option<&std::path::Path>) -> Option<String> {
    let loaded = checkpoint::load(checkpoint?)
        .inspect_err(|e| {
            tracing::error!(
                "consolidation: the model summary is lost, so the run asks again: {e:#}"
            );
        })
        .ok()?;
    if loaded.phase != "summarized" {
        return None;
    }
    loaded.inputs["summary"].as_str().map(str::to_string)
}

/// Delete the checkpoint of a run with nothing left to retry. Fail-soft.
fn settle(checkpoint: Option<&std::path::Path>) {
    if let Some(path) = checkpoint
        && let Err(e) = checkpoint::complete(path)
    {
        tracing::error!("consolidation: {e:#}");
    }
}

/// Run post-session consolidation with the active memory entry's settings
/// (see [`active_consolidation`]).
///
/// Recalls recent memories from the ICM backend, preconditions ≥3 records,
/// calls the Anthropic Messages API for distillation, and stores the
/// resulting rules as semantic/high memories. With a `checkpoint`, the run
/// records its phase, keeps the model summary, and deletes the file only
/// when no work is left to retry (#2396). `state_dir` holds the seen-set that keeps a resumed run
/// from storing a rule twice (#2397).
///
/// # Errors
/// All errors are caught and logged via `tracing::error!` — this function
/// always returns `Ok(summary)` to match the fail-soft contract.
pub(crate) async fn run(
    cc: &crate::config::ConsolidationConfig,
    client: &McpHttpClient,
    project: &str,
    state_dir: Option<&std::path::Path>,
    checkpoint: Option<&std::path::Path>,
) -> anyhow::Result<String> {
    tracing::info!(
        max_rules = cc.max_rules_per_session,
        backend = ?cc.backend,
        "running post-session consolidation"
    );

    let (llm_output, records) = match saved_summary(checkpoint) {
        Some(text) => (text, 0),
        None => match distill(cc, client, project, checkpoint).await {
            Distilled::Summary { text, records } => (text, records),
            Distilled::Stop { msg, settled } => {
                if settled {
                    settle(checkpoint);
                }
                return Ok(msg);
            }
        },
    };

    // Step 5: Parse bullet points
    let rules = parse_bullets(&llm_output);

    if rules.is_empty() {
        let msg = format!("consolidation: LLM returned no rules (parsed {records} records)");
        tracing::debug!("{msg}");
        settle(checkpoint);
        return Ok(msg);
    }

    // Enforce max_rules client-side (spec R5)
    let max_rules = cc.max_rules_per_session as usize;
    let rules: Vec<&str> = rules.iter().map(|s| s.as_str()).take(max_rules).collect();

    // Step 6: Store each rule
    let run_id = run_id(checkpoint);
    let scope = RunScope {
        project,
        run_id: &run_id,
    };
    let (stored, duplicates) = store_new_rules(client, state_dir, &scope, &rules).await;

    let msg = format!(
        "consolidation: distilled {records} memory records into {} semantic rule(s) \
         (backend: {:?}, rules stored: {stored}, duplicates skipped: {duplicates})",
        rules.len(),
        cc.backend,
    );
    if stored + duplicates < rules.len() {
        tracing::error!("{msg}");
    } else {
        tracing::info!("{msg}");
        settle(checkpoint);
    }
    Ok(msg)
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "test code")]
mod tests {
    use proptest::prelude::*;

    use super::*;

    #[test]
    fn default_model_is_a_published_id() {
        assert_eq!(
            DEFAULT_MODEL, "claude-sonnet-5",
            "DEFAULT_MODEL must be a published Messages API id (docs/design/issue-2143-model-ids.md)"
        );
    }

    #[test]
    fn resolve_api_model_uses_default_when_unset() {
        assert_eq!(resolve_api_model(None), (DEFAULT_MODEL.to_string(), None));
    }

    #[test]
    fn resolve_api_model_accepts_full_id() {
        assert_eq!(
            resolve_api_model(Some("claude-opus-5-5")),
            ("claude-opus-5-5".to_string(), None)
        );
    }

    #[test]
    fn resolve_api_model_rejects_aliases_and_empty() {
        for value in ["opus", "sonnet[1m]", ""] {
            let (model, warning) = resolve_api_model(Some(value));
            assert_eq!(model, DEFAULT_MODEL, "value {value:?}");
            let warning = warning.expect("alias must warn");
            assert!(
                warning.contains(&format!("ANTHROPIC_MODEL=\"{value}\"")),
                "{warning}"
            );
            assert!(warning.contains(DEFAULT_MODEL), "{warning}");
        }
    }

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
    fn parse_recall_output_keeps_each_record_topic() {
        let text =
            "--- a ---\n  topic: t1\n  summary: one\n--- b ---\n  summary: two\n  topic: t2\n";
        let got: Vec<_> = parse_recall_output(text)
            .into_iter()
            .map(|r| (r.summary, r.topic))
            .collect();
        assert_eq!(
            got,
            [
                ("one".to_string(), Some("t1".to_string())),
                ("two".to_string(), Some("t2".to_string()))
            ]
        );
    }

    #[test]
    fn parse_recall_output_empty_summary_creates_record() {
        let text = "--- id-1 ---\n  summary:\n  weight: 0.5\n";
        let records = parse_recall_output(text);
        assert_eq!(records.len(), 1);
        assert!(records[0].summary.is_empty());
    }

    // -- isolated claude -p child (#2355) --

    #[test]
    fn claude_command_isolates_the_child_and_sets_the_guard() {
        let cmd = claude_command();
        let std_cmd = cmd.as_std();
        let args: Vec<&std::ffi::OsStr> = std_cmd.get_args().collect();
        let has_pair = |flag: &str, value: &str| {
            args.windows(2)
                .any(|w| w[0] == std::ffi::OsStr::new(flag) && w[1] == std::ffi::OsStr::new(value))
        };
        assert!(
            has_pair(
                "--settings",
                r#"{"disableAllHooks":true,"outputStyle":"default"}"#
            ),
            "{args:?}"
        );
        assert!(has_pair("--tools", ""), "{args:?}");
        // Dropping the setting sources would drop auth held in settings.
        assert!(!args.contains(&std::ffi::OsStr::new("--setting-sources")));
        assert!(args.contains(&std::ffi::OsStr::new("--strict-mcp-config")));
        assert!(args.contains(&std::ffi::OsStr::new("-p")));
        // `--bare` would drop OAuth auth, which this backend exists for.
        assert!(!args.contains(&std::ffi::OsStr::new("--bare")));
        let guard = std_cmd
            .get_envs()
            .find(|(k, _)| *k == std::ffi::OsStr::new(CHILD_GUARD_ENV))
            .and_then(|(_, v)| v);
        assert_eq!(guard, Some(std::ffi::OsStr::new("1")));
    }

    fn memory_entry(when: &str, enabled: Option<bool>) -> crate::config::Memory {
        let consolidation = enabled.map_or(String::new(), |e| {
            format!("consolidation:\n  enabled: {e}\n  max_rules_per_session: 4\n")
        });
        serde_yaml::from_str(&format!(
            "server_host: h\nport: 1\nwhen: [{when}]\n{consolidation}"
        ))
        .expect("valid Memory fixture YAML")
    }

    proptest! {
        // Oracle check: the first tag-active entry decides, and only when its
        // consolidation is enabled; a later enabled entry never wins.
        #[test]
        fn active_consolidation_matches_oracle(
            entries in proptest::collection::vec(
                (0u8..3, proptest::option::of(any::<bool>())),
                0..5,
            ),
            active in proptest::collection::btree_set(0u8..3, 0..3),
        ) {
            let memory: Vec<crate::config::Memory> = entries
                .iter()
                .map(|(tag, enabled)| memory_entry(&format!("t{tag}"), *enabled))
                .collect();
            let tags: std::collections::BTreeSet<String> =
                active.iter().map(|t| format!("t{t}")).collect();
            let expected = entries
                .iter()
                .find(|(tag, _)| active.contains(tag))
                .and_then(|(_, enabled)| *enabled)
                .unwrap_or(false);
            prop_assert_eq!(active_consolidation(&memory, &tags).is_some(), expected);
        }
    }

    #[test]
    fn active_consolidation_uses_the_tag_active_entry() {
        let tags = std::collections::BTreeSet::from(["on".to_string()]);
        let memory = vec![
            memory_entry("off", Some(true)),
            memory_entry("on", Some(true)),
        ];
        let cc = active_consolidation(&memory, &tags).expect("active entry consolidates");
        assert_eq!(cc.max_rules_per_session, 4);
    }

    #[test]
    fn active_consolidation_none_when_disabled_absent_or_inactive() {
        let tags = std::collections::BTreeSet::from(["on".to_string()]);
        assert!(active_consolidation(&[memory_entry("on", Some(false))], &tags).is_none());
        assert!(active_consolidation(&[memory_entry("on", None)], &tags).is_none());
        assert!(active_consolidation(&[memory_entry("off", Some(true))], &tags).is_none());
        assert!(active_consolidation(&[], &tags).is_none());
    }

    // -- request ids (#2397) --

    fn ok_body(text: &str) -> serde_json::Value {
        serde_json::json!({"jsonrpc":"2.0","id":1,
            "result":{"content":[{"type":"text","text":text}]}})
    }

    async fn store_calls(server: &wiremock::MockServer) -> Vec<serde_json::Value> {
        server
            .received_requests()
            .await
            .expect("recorded")
            .iter()
            .filter_map(|r| serde_json::from_slice::<serde_json::Value>(&r.body).ok())
            .filter(|b| b["params"]["name"] == "icm_memory_store")
            .collect()
    }

    #[tokio::test]
    async fn a_resumed_batch_stores_each_rule_once_with_a_request_keyword() {
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(ok_body("")))
            .mount(&server)
            .await;
        let client = McpHttpClient::test_new(server.uri(), Duration::from_secs(2)).expect("client");
        let dir = tempfile::tempdir().expect("tempdir");
        let rules = [
            "Run the formatter before every commit",
            "Pin action versions to a SHA",
        ];
        let run = RunScope {
            project: "proj",
            run_id: "run-1",
        };
        let first = store_new_rules(&client, Some(dir.path()), &run, &rules).await;
        let resumed = store_new_rules(&client, Some(dir.path()), &run, &rules).await;
        assert_eq!(first, (2, 0));
        assert_eq!(
            resumed,
            (2, 0),
            "the resumed run still reports its rules as handled"
        );
        let stored = store_calls(&server).await;
        assert_eq!(stored.len(), 2, "the resumed run must store nothing new");
        let later = RunScope {
            project: "proj",
            run_id: "run-2",
        };
        let relearned = store_new_rules(&client, Some(dir.path()), &later, &rules).await;
        assert_eq!(
            relearned,
            (2, 0),
            "a later run may store a rule that was forgotten from ICM"
        );
        assert_eq!(store_calls(&server).await.len(), 4);

        let keyword = &stored[0]["params"]["arguments"]["keywords"][0];
        assert!(
            keyword.as_str().expect("keyword").starts_with("request:"),
            "{keyword}"
        );
    }

    // -- checkpoint phases (#2396) --

    fn cc() -> crate::config::ConsolidationConfig {
        crate::config::ConsolidationConfig {
            enabled: true,
            backend: crate::config::ConsolidationBackend::default(),
            max_rules_per_session: 5,
        }
    }

    fn summarized_checkpoint(dir: &std::path::Path) -> std::path::PathBuf {
        use crate::hook_run::checkpoint::{Checkpoint, JobKind, write};
        let mut cp = Checkpoint::new(
            JobKind::Consolidation,
            serde_json::json!({ "cwd": "/p" }),
            None,
        );
        cp.phase = "summarized".into();
        cp.inputs["summary"] = serde_json::json!("- Run the formatter before every commit");
        write(dir, &cp).expect("write").expect("path")
    }

    async fn mount_icm(server: &wiremock::MockServer, store_ok: bool) {
        use wiremock::matchers::{body_string_contains, method};
        use wiremock::{Mock, ResponseTemplate};
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(ok_body("")))
            .mount(server)
            .await;
        if !store_ok {
            Mock::given(method("POST"))
                .and(body_string_contains("icm_memory_store"))
                .respond_with(ResponseTemplate::new(500))
                .with_priority(1)
                .mount(server)
                .await;
        }
    }

    async fn recalls_of_fifty(server: &wiremock::MockServer) -> usize {
        server
            .received_requests()
            .await
            .expect("recorded")
            .iter()
            .filter_map(|r| serde_json::from_slice::<serde_json::Value>(&r.body).ok())
            .filter(|b| {
                b["params"]["name"] == "icm_memory_recall"
                    && b["params"]["arguments"]["limit"] == 50
            })
            .count()
    }

    #[tokio::test]
    async fn a_resume_after_the_model_step_skips_recall_and_the_model_and_settles() {
        let server = wiremock::MockServer::start().await;
        mount_icm(&server, true).await;
        let client = McpHttpClient::test_new(server.uri(), Duration::from_secs(2)).expect("client");
        let dir = tempfile::tempdir().expect("tempdir");
        let file = summarized_checkpoint(dir.path());
        let msg = run(&cc(), &client, "proj", Some(dir.path()), Some(&file))
            .await
            .expect("run");
        assert!(msg.contains("1 semantic rule"), "{msg}");
        assert_eq!(
            recalls_of_fifty(&server).await,
            0,
            "a resume must not recall again"
        );
        assert_eq!(store_calls(&server).await.len(), 1);
        assert!(
            !file.exists(),
            "every rule is handled, so the checkpoint goes"
        );
    }

    #[tokio::test]
    async fn a_failed_store_keeps_the_checkpoint_at_summarized_with_its_summary() {
        let server = wiremock::MockServer::start().await;
        mount_icm(&server, false).await;
        let client = McpHttpClient::test_new(server.uri(), Duration::from_secs(2)).expect("client");
        let dir = tempfile::tempdir().expect("tempdir");
        let file = summarized_checkpoint(dir.path());
        run(&cc(), &client, "proj", Some(dir.path()), Some(&file))
            .await
            .expect("run");
        let kept = crate::hook_run::checkpoint::load(&file).expect("kept");
        assert_eq!(kept.phase, "summarized");
        assert!(
            kept.inputs["summary"]
                .as_str()
                .expect("summary")
                .contains("formatter")
        );
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
            let _ = build_prompt(max_rules_per_session, &summaries);
        }

        /// Every placeholder the prompt template declares (`{max_rules}`,
        /// `{summaries}`) must be fully consumed by the `.replace()` chain —
        /// none should survive into the built prompt.
        #[test]
        fn build_prompt_consumes_all_declared_placeholders(
            max_rules_per_session in 0u32..1000,
            junk in "[^{}]{0,10}",
        ) {
            let out = build_prompt(max_rules_per_session, &[junk]);
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
            let out = build_prompt(max_rules_per_session, &summaries);
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
