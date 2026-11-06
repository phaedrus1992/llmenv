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
//! All failures are fail-soft: an agent notice, then `Ok(summary)`. The
//! detached child's log filter is ERROR-only by default, so a `warn!` here
//! would leave no trace (#2355).

use std::process::Stdio;
use std::time::Duration;

use crate::hook_run::checkpoint;
use crate::hook_run::idempotency;
use llmenv_mcp::mcp_client::McpHttpClient;

mod bullets;
mod phase;
use phase::{note_phase, notice, saved_summary, settle};
mod dedup;

/// Hard timeout for the LLM backend call.
const LLM_TIMEOUT: Duration = Duration::from_secs(120);
/// Minimum episodic records needed to trigger consolidation.
const MIN_RECORDS: usize = 3;
/// Default model for the `anthropic-api` backend.
const DEFAULT_MODEL: &str = "claude-sonnet-5";

/// Output cap sent with each Messages API request. A longer answer is cut off, and
/// `answer_text` refuses the cut answer.
const ANSWER_MAX_TOKENS: u64 = 4096;

/// Messages API endpoint for the `anthropic-api` backend.
const ANTHROPIC_MESSAGES_URL: &str = "https://api.anthropic.com/v1/messages";

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
    let started = std::time::Instant::now();
    let waited = run_claude(prompt).await;
    crate::trace::emit_spawn_trace(
        "consolidation:claude",
        started.elapsed(),
        match &waited {
            Ok(output) => output.status.code().map_or(
                crate::trace::SpawnExit::Signal,
                crate::trace::SpawnExit::Code,
            ),
            Err(e) if e.downcast_ref::<WaitTimedOut>().is_some() => {
                crate::trace::SpawnExit::Timeout
            }
            Err(_) => crate::trace::SpawnExit::Error,
        },
    );
    let output = waited?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("claude -p exited with {}: {stderr}", output.status);
    }

    let stdout = String::from_utf8(output.stdout)?;
    Ok(stdout.trim().to_string())
}

/// Start `claude -p`, send it `prompt`, and wait for its output.
async fn run_claude(prompt: &str) -> anyhow::Result<std::process::Output> {
    let program = crate::path_lookup::resolve_program("claude")?;
    let mut child = spawn_with_kill_on_drop(claude_command(&program))?;

    // Write prompt to stdin and close it
    if let Some(mut stdin) = child.stdin.take() {
        use tokio::io::AsyncWriteExt;
        stdin.write_all(prompt.as_bytes()).await?;
        // Drop stdin so the process can read EOF
        drop(stdin);
    }

    wait_with_timeout_or_kill_group(child, LLM_TIMEOUT).await
}

/// Env var set on the `claude -p` child. `hook-run` exits at once when it
/// sees it, so the child can never start consolidation again (#2355).
pub(crate) const CHILD_GUARD_ENV: &str = "LLMENV_CONSOLIDATION_CHILD";

mod claude_cli;
use claude_cli::claude_command;

/// The consolidation settings of the active memory entry: the one entry in `memory` (top-level
/// plus bundle-contributed) whose `when` has an active tag, when its consolidation is enabled.
/// With two active entries the memory server does not resolve, so neither entry applies.
#[must_use]
pub(crate) fn active_consolidation<'a>(
    memory: &'a [crate::config::Memory],
    active_tags: &std::collections::BTreeSet<String>,
) -> Option<&'a crate::config::ConsolidationConfig> {
    crate::memory::auto_prune::single_active(memory, active_tags)
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
/// The child did not exit within the timeout, and its process group was killed.
#[derive(Debug)]
struct WaitTimedOut {
    pid: Option<u32>,
    timeout: Duration,
}

impl std::fmt::Display for WaitTimedOut {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "process (pid {:?}) timed out after {:?}",
            self.pid, self.timeout
        )
    }
}

impl std::error::Error for WaitTimedOut {}

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
            Err(WaitTimedOut { pid, timeout }.into())
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
        // ESRCH means the group already exited, which is the normal outcome. Any other error
        // (EPERM, for one) leaves a child running, so it is logged (#2666).
        match rustix::process::kill_process_group(pid, rustix::process::Signal::KILL) {
            Ok(()) | Err(rustix::io::Errno::SRCH) => {}
            Err(e) => {
                notice(&format!(
                    "consolidation: cannot kill the process group {pid_i32}, so it can still run: {e}"
                ));
                tracing::error!(
                    pid = pid_i32,
                    error = %e,
                    "cannot kill the consolidation process group"
                )
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
    }
}

/// Pick the model for the `anthropic-api` backend from `ANTHROPIC_MODEL`.
///
/// Claude Code also reads `ANTHROPIC_MODEL` and accepts aliases such as `opus`. The Messages
/// API rejects an alias, so only a value that starts with `claude-` is used. Any other value
/// fails. A silent fallback would run consolidation on a model the user did not choose.
fn resolve_api_model(env_value: Option<&str>) -> anyhow::Result<String> {
    match env_value {
        None => Ok(DEFAULT_MODEL.to_string()),
        Some(value) if value.starts_with("claude-") => Ok(value.to_string()),
        Some("") => anyhow::bail!(
            "ANTHROPIC_MODEL is set but empty. Set it to a full model ID such as {DEFAULT_MODEL}, \
             or unset it to use {DEFAULT_MODEL}."
        ),
        Some(value) => anyhow::bail!(
            "ANTHROPIC_MODEL=\"{value}\" is not a full model ID. The Messages API needs a full \
             model ID such as {DEFAULT_MODEL}, not a Claude Code alias. Set ANTHROPIC_MODEL to a \
             full model ID, or unset it to use {DEFAULT_MODEL}."
        ),
    }
}

/// Pull the answer text out of a Messages API response.
///
/// Only a normal stop is accepted. A `max_tokens` stop means the answer was cut off, and the last
/// bullet can then be a partial rule. Any other stop, such as `refusal`, does not carry rules.
/// The text blocks are joined with a newline, so two blocks never merge into one bullet.
fn answer_text(json: &serde_json::Value) -> anyhow::Result<String> {
    match json["stop_reason"].as_str() {
        Some("end_turn" | "stop_sequence") => {}
        Some("max_tokens") => anyhow::bail!(
            "Anthropic API stopped at max_tokens ({ANSWER_MAX_TOKENS}), so the answer may be cut \
             short. No rules were stored."
        ),
        other => anyhow::bail!(
            "Anthropic API stopped with {other:?}, not a normal end. No rules were stored."
        ),
    }
    let blocks = json["content"]
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("unexpected Anthropic API response shape"))?;
    let texts: Vec<&str> = blocks.iter().filter_map(|b| b["text"].as_str()).collect();
    Ok(texts.join("\n"))
}

/// Make a non-streaming call to the Anthropic Messages API.
///
/// Requires `ANTHROPIC_API_KEY` and (optionally) `ANTHROPIC_MODEL` env vars.
///
/// # Errors
/// Returns `anyhow::Error` on HTTP failure, timeout, or malformed response.
async fn call_anthropic_api(prompt: &str) -> anyhow::Result<String> {
    let api_key = api_key_from_env(std::env::var("ANTHROPIC_API_KEY"))?;
    let model_value = model_value_from_env(std::env::var("ANTHROPIC_MODEL"))?;
    let model = resolve_api_model(model_value.as_deref())?;
    post_messages(ANTHROPIC_MESSAGES_URL, &api_key, &model, prompt).await
}

/// Turn the `ANTHROPIC_API_KEY` lookup into a key. An error names the variable and the fix.
/// An empty key fails here, because the API would reject it with an unexplained HTTP 401.
fn api_key_from_env(value: Result<String, std::env::VarError>) -> anyhow::Result<String> {
    match value {
        Ok(key) if key.trim().is_empty() => anyhow::bail!(
            "ANTHROPIC_API_KEY is set but empty. Export your Anthropic API key, or set the \
             consolidation backend to claude-cli, which needs no key."
        ),
        Ok(key) => Ok(key),
        Err(std::env::VarError::NotPresent) => anyhow::bail!(
            "ANTHROPIC_API_KEY is not set. Export your Anthropic API key, or set the \
             consolidation backend to claude-cli, which needs no key."
        ),
        Err(std::env::VarError::NotUnicode(_)) => anyhow::bail!(
            "ANTHROPIC_API_KEY is not valid UTF-8. Export the key again as plain text."
        ),
    }
}

/// Read `ANTHROPIC_MODEL`. A set value that is not Unicode fails, because the user set it to
/// choose a model. Falling back to the default would run a different model without a visible error.
fn model_value_from_env(
    value: Result<String, std::env::VarError>,
) -> anyhow::Result<Option<String>> {
    match value {
        Ok(model) => Ok(Some(model)),
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(std::env::VarError::NotUnicode(_)) => anyhow::bail!(
            "ANTHROPIC_MODEL is not valid UTF-8. Export it again as a full model ID such as \
             claude-sonnet-5, or unset it to use {DEFAULT_MODEL}."
        ),
    }
}

/// The problem that an `ANTHROPIC_MODEL` value causes for the `anthropic-api` backend, or `None`
/// when the backend accepts it. `llmenv doctor` shows this, because the detached run that hits
/// the problem writes only to its own log.
pub(crate) fn model_problem(value: Result<String, std::env::VarError>) -> Option<String> {
    model_value_from_env(value)
        .and_then(|model| resolve_api_model(model.as_deref()))
        .err()
        .map(|e| format!("{e:#}"))
}

/// Post one Messages API request to `url`. `url` is a parameter so tests can use a mock server.
async fn post_messages(
    url: &str,
    api_key: &str,
    model: &str,
    prompt: &str,
) -> anyhow::Result<String> {
    // The API key travels in `x-api-key`, which reqwest does not strip on a cross-host redirect.
    // The endpoint is fixed, so a redirect is never expected and is refused.
    let client = reqwest::Client::builder()
        .timeout(LLM_TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        .build()?;

    let body = serde_json::json!({
        "model": model,
        "max_tokens": ANSWER_MAX_TOKENS,
        "messages": [{
            "role": "user",
            "content": prompt
        }]
    });

    let resp = client
        .post(url)
        .header("x-api-key", api_key)
        .header("anthropic-version", "2023-06-01")
        .header("content-type", "application/json")
        .json(&body)
        .send()
        .await?;

    let status = resp.status();
    if status.is_redirection() {
        // Redirects are off, so a 3xx arrives as a response. Name the target so the cause is
        // visible; the key was never sent to it.
        let target = resp
            .headers()
            .get(reqwest::header::LOCATION)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("(no Location header)");
        anyhow::bail!(
            "Anthropic API redirected the request to {target}; the redirect was refused so the API \
             key stays with api.anthropic.com. Check for a proxy or a changed endpoint."
        );
    }
    if !status.is_success() {
        let text = match resp.text().await {
            Ok(text) => text,
            Err(e) => format!("(body unreadable: {e})"),
        };
        anyhow::bail!("Anthropic API returned {status}: {text}");
    }

    let json: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| anyhow::anyhow!("failed to decode the Anthropic API response: {e}"))?;
    answer_text(&json)
}

/// What names one consolidation run: the project, and the id of the run that started it.
struct RunScope<'a> {
    project: &'a str,
    run_id: &'a str,
}

/// The id of this run: the run tag its checkpoint was written with, or empty without a checkpoint.
fn run_id(checkpoint: Option<&std::path::Path>) -> String {
    checkpoint
        .and_then(|p| {
            // An empty id on a resumed run makes the rule request ids differ from the first
            // run, so the seen-set misses a repeat.
            checkpoint::load(p)
                .inspect_err(|e| {
                    phase::notice(&format!(
                        "consolidation: the run tag is lost, so a repeated rule may be stored \
                         again: {e:#}"
                    ));
                })
                .ok()
        })
        .and_then(|c| c.inputs["run_tag"].as_str().map(str::to_string))
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

/// The line that a consolidation run logs. A rule whose duplicate check or store failed is
/// counted, so the log says that the next run tries it again.
fn summary(
    records: usize,
    rules: usize,
    backend: impl std::fmt::Debug,
    (stored, duplicates, failed): (usize, usize, usize),
) -> String {
    let mut msg = format!(
        "consolidation: distilled {records} memory records into {rules} semantic rule(s) \
         (backend: {backend:?}, rules stored: {stored}, duplicates skipped: {duplicates}"
    );
    if failed > 0 {
        msg.push_str(&format!(
            ", failed: {failed}, tried again on the next run; the log above names each cause"
        ));
    }
    msg.push(')');
    msg
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
                        // Without a budget icm clamps the limit to 20 (#3419).
                        "max_tokens": crate::memory::recall_json::MAX_TOKENS,
                        "format": crate::memory::recall_json::FORMAT,
                    }),
                )
                .await
        })
        .await;

    let output = match recall_result {
        Ok(out) => out,
        Err(e) => {
            let msg = format!("consolidation: recall failed (fail-soft): {e}");
            notice(&msg);
            return Distilled::Stop {
                msg,
                settled: false,
            };
        }
    };

    let records = match crate::memory::recall_json::parse(&output) {
        Ok(records) => records,
        Err(e) => {
            let msg = format!("consolidation: recall reply unreadable (fail-soft): {e}");
            notice(&msg);
            return Distilled::Stop {
                msg,
                settled: false,
            };
        }
    };
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
            notice(&msg);
            Distilled::Stop {
                msg,
                settled: false,
            }
        }
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
/// All errors are caught and recorded as agent notices — this function
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
    let rules = bullets::parse_bullets(&llm_output);

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

    let failed = rules.len().saturating_sub(stored + duplicates);
    let msg = summary(
        records,
        rules.len(),
        &cc.backend,
        (stored, duplicates, failed),
    );
    if failed > 0 {
        notice(&msg);
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
        assert_eq!(
            resolve_api_model(None).expect("unset must pass"),
            DEFAULT_MODEL
        );
    }

    #[test]
    fn resolve_api_model_accepts_full_id() {
        assert_eq!(
            resolve_api_model(Some("claude-opus-5-5")).expect("full id must pass"),
            "claude-opus-5-5"
        );
    }

    #[test]
    fn resolve_api_model_refuses_aliases_and_names_the_variable() {
        for value in ["opus", "sonnet[1m]"] {
            let msg = resolve_api_model(Some(value))
                .expect_err("alias must fail, not fall back")
                .to_string();
            assert!(
                msg.contains(&format!("ANTHROPIC_MODEL=\"{value}\"")),
                "{msg}"
            );
            assert!(msg.contains(DEFAULT_MODEL), "{msg}");
        }
    }

    #[test]
    fn resolve_api_model_refuses_empty_and_names_the_variable() {
        let msg = resolve_api_model(Some(""))
            .expect_err("empty must fail, not fall back")
            .to_string();
        assert!(msg.contains("ANTHROPIC_MODEL is set but empty"), "{msg}");
    }

    proptest! {
        #[test]
        fn resolve_api_model_accepts_exactly_the_claude_prefix(value in ".{0,40}") {
            match resolve_api_model(Some(&value)) {
                Ok(model) => {
                    prop_assert!(value.starts_with("claude-"));
                    prop_assert_eq!(model, value);
                }
                Err(e) => {
                    prop_assert!(!value.starts_with("claude-"));
                    prop_assert!(e.to_string().contains("ANTHROPIC_MODEL"));
                }
            }
        }
    }

    #[test]
    fn answer_text_refuses_a_refusal_stop() {
        let json = serde_json::json!({
            "stop_reason": "refusal",
            "content": [{ "type": "text", "text": "- not rules" }]
        });
        let msg = answer_text(&json)
            .expect_err("a refusal must fail")
            .to_string();
        assert!(msg.contains("refusal"), "{msg}");
    }

    #[test]
    fn answer_text_joins_text_blocks_with_a_newline() {
        let json = serde_json::json!({
            "stop_reason": "end_turn",
            "content": [
                { "type": "text", "text": "- a" },
                { "type": "text", "text": "- b" }
            ]
        });
        assert_eq!(
            answer_text(&json).expect("two blocks must pass"),
            "- a\n- b"
        );
    }

    proptest! {
        #[test]
        fn answer_text_refuses_every_max_tokens_stop(text in ".{0,60}", extra in 0usize..3) {
            let content: Vec<serde_json::Value> = (0..=extra)
                .map(|_| serde_json::json!({ "type": "text", "text": text }))
                .collect();
            let json = serde_json::json!({ "stop_reason": "max_tokens", "content": content });
            prop_assert!(answer_text(&json).is_err());
        }
    }

    #[test]
    fn answer_text_refuses_a_max_tokens_stop() {
        let json = serde_json::json!({
            "stop_reason": "max_tokens",
            "content": [{ "type": "text", "text": "- first rule\n- cut" }]
        });
        let msg = answer_text(&json)
            .expect_err("a cut answer must fail")
            .to_string();
        assert!(msg.contains("max_tokens"), "{msg}");
        assert!(msg.contains("No rules were stored"), "{msg}");
    }

    #[test]
    fn answer_text_returns_text_on_a_normal_stop() {
        let json = serde_json::json!({
            "stop_reason": "end_turn",
            "content": [{ "type": "text", "text": "- a rule" }]
        });
        assert_eq!(answer_text(&json).expect("end_turn must pass"), "- a rule");
    }

    #[test]
    fn answer_text_refuses_a_missing_content_block() {
        let json = serde_json::json!({ "stop_reason": "end_turn" });
        let msg = answer_text(&json)
            .expect_err("no content must fail")
            .to_string();
        assert!(
            msg.contains("unexpected Anthropic API response shape"),
            "{msg}"
        );
    }

    #[test]
    fn api_key_missing_names_the_variable_and_the_fix() {
        let msg = api_key_from_env(Err(std::env::VarError::NotPresent))
            .expect_err("a missing key must fail")
            .to_string();
        assert!(msg.contains("ANTHROPIC_API_KEY is not set"), "{msg}");
        assert!(msg.contains("claude-cli"), "{msg}");
    }

    #[test]
    fn api_key_present_is_returned() {
        let key = api_key_from_env(Ok("sk-test".into())).expect("a present key must pass");
        assert_eq!(key, "sk-test");
    }

    #[cfg(unix)]
    #[test]
    fn api_key_not_unicode_is_an_error() {
        use std::os::unix::ffi::OsStringExt;
        let raw = std::ffi::OsString::from_vec(vec![0xff]);
        let msg = api_key_from_env(Err(std::env::VarError::NotUnicode(raw)))
            .expect_err("a non-Unicode key must fail")
            .to_string();
        assert!(
            msg.contains("ANTHROPIC_API_KEY is not valid UTF-8"),
            "{msg}"
        );
    }

    #[test]
    fn model_problem_is_none_when_the_model_is_unset_or_a_full_id() {
        assert_eq!(model_problem(Err(std::env::VarError::NotPresent)), None);
        assert_eq!(model_problem(Ok("claude-sonnet-5".to_string())), None);
    }

    #[test]
    fn model_problem_names_an_alias_and_the_fix() {
        let problem = model_problem(Ok("opus".to_string())).expect("an alias is a problem");
        assert!(
            problem.contains("\"opus\" is not a full model ID"),
            "{problem}"
        );
        assert!(problem.contains("claude-sonnet-5"), "{problem}");
    }

    #[test]
    fn model_problem_names_an_empty_value() {
        let problem = model_problem(Ok(String::new())).expect("an empty value is a problem");
        assert!(
            problem.contains("ANTHROPIC_MODEL is set but empty"),
            "{problem}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn model_not_unicode_is_an_error_not_a_silent_default() {
        use std::os::unix::ffi::OsStringExt;
        let raw = std::ffi::OsString::from_vec(vec![0xff]);
        let msg = model_value_from_env(Err(std::env::VarError::NotUnicode(raw)))
            .expect_err("a non-Unicode model must fail")
            .to_string();
        assert!(msg.contains("ANTHROPIC_MODEL is not valid UTF-8"), "{msg}");
    }

    #[test]
    fn model_present_is_kept() {
        let model = model_value_from_env(Ok("claude-opus-5-5".into())).expect("present model");
        assert_eq!(model.as_deref(), Some("claude-opus-5-5"));
    }

    #[test]
    fn model_absent_is_none() {
        let model = model_value_from_env(Err(std::env::VarError::NotPresent)).expect("absent");
        assert_eq!(model, None);
    }

    #[test]
    fn api_key_empty_is_an_error_naming_the_variable() {
        let msg = api_key_from_env(Ok("   ".into()))
            .expect_err("an empty key must fail")
            .to_string();
        assert!(msg.contains("ANTHROPIC_API_KEY is set but empty"), "{msg}");
    }

    #[tokio::test]
    async fn post_messages_refuses_a_redirect_and_names_the_target() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .respond_with(
                wiremock::ResponseTemplate::new(302)
                    .insert_header("location", "https://evil.example/relay"),
            )
            .mount(&server)
            .await;
        let url = format!("{}/v1/messages", server.uri());
        let msg = post_messages(&url, "sk-test", DEFAULT_MODEL, "hi")
            .await
            .expect_err("a redirect must be refused")
            .to_string();
        assert!(msg.contains("https://evil.example/relay"), "{msg}");
        assert!(msg.contains("redirect was refused"), "{msg}");
    }

    #[tokio::test]
    async fn post_messages_error_status_includes_the_body() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .respond_with(wiremock::ResponseTemplate::new(500).set_body_string("overloaded"))
            .mount(&server)
            .await;
        let url = format!("{}/v1/messages", server.uri());
        let msg = post_messages(&url, "sk-test", DEFAULT_MODEL, "hi")
            .await
            .expect_err("a 500 must fail")
            .to_string();
        assert!(msg.contains("500"), "{msg}");
        assert!(msg.contains("overloaded"), "{msg}");
    }

    // -- isolated claude -p child (#2355) --

    #[test]
    fn claude_command_isolates_the_child_and_sets_the_guard() {
        let cmd = claude_command(std::path::Path::new("claude"));
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
        let wait = std_cmd
            .get_envs()
            .find(|(k, _)| *k == std::ffi::OsStr::new("CLAUDE_CODE_MCP_STARTUP_WAIT_MS"))
            .and_then(|(_, v)| v);
        assert_eq!(wait, Some(std::ffi::OsStr::new("0")));
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
        // Oracle check: exactly one tag-active entry decides, and only when its
        // consolidation is enabled; with two active entries none applies.
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
            let hits: Vec<_> = entries.iter().filter(|(tag, _)| active.contains(tag)).collect();
            let expected = hits.len() == 1 && hits[0].1.unwrap_or(false);
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
            .respond_with(ResponseTemplate::new(200).set_body_json(ok_body("[]")))
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
            .respond_with(ResponseTemplate::new(200).set_body_json(ok_body("[]")))
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

    #[test]
    fn the_summary_counts_failed_rules_only_when_some_failed() {
        let ok = summary(9, 2, "Claude", (1, 1, 0));
        assert_eq!(
            ok,
            "consolidation: distilled 9 memory records into 2 semantic rule(s) (backend: \
             \"Claude\", rules stored: 1, duplicates skipped: 1)"
        );
        let failed = summary(9, 3, "Claude", (1, 1, 1));
        assert!(
            failed.ends_with("duplicates skipped: 1, failed: 1, tried again on the next run; the log above names each cause)"),
            "{failed}"
        );
    }

    #[tokio::test]
    async fn distill_asks_for_json_and_stops_fail_soft_on_a_text_reply() {
        use wiremock::matchers::method;
        use wiremock::{Mock, ResponseTemplate};
        let server = wiremock::MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(ok_body("--- a ---\n")))
            .mount(&server)
            .await;
        let client = McpHttpClient::test_new(server.uri(), Duration::from_secs(2)).expect("client");
        let Distilled::Stop { msg, settled } = distill(&cc(), &client, "proj", None).await else {
            unreachable!("a text reply must stop the run");
        };
        assert!(!settled, "an unreadable reply is retried on a later run");
        assert!(msg.contains("recall reply unreadable"), "{msg}");
        assert!(msg.contains("Fix: upgrade icm to 0.11.5 or later"), "{msg}");
        let notices = crate::agent_notice::current().unwrap_or_default();
        assert!(
            notices.contains("recall reply unreadable"),
            "the agent is told: {notices}"
        );
        let asked = server.received_requests().await.expect("recorded");
        assert!(
            asked
                .iter()
                .any(|r| String::from_utf8_lossy(&r.body).contains(r#""format":"json""#)),
            "the recall must ask for JSON"
        );
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
                    && b["params"]["arguments"]["max_tokens"] == 32_000
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
        let notices = crate::agent_notice::current().unwrap_or_default();
        assert!(
            notices.contains("llmenv: consolidation"),
            "the agent is told: {notices}"
        );
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
        let err = result.expect_err("sh should not exit within 50ms");
        assert!(
            err.downcast_ref::<WaitTimedOut>().is_some(),
            "a timeout must be the typed error, got {err:#}"
        );

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
}
