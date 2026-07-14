<!-- markdownlint-disable MD013 -->
# Lifecycle Memory Hooks Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add `llmenv hook-run <event>` — an engine-neutral lifecycle-hook dispatcher that injects ICM memory context into an AI coding agent automatically by calling the configured ICM memory MCP over HTTP, auto-wired into the Claude Code adapter when a scope's active tags select the `memory:` backend.

**Architecture:** A new `src/hook_run/` module holds (1) a minimal async HTTP JSON-RPC MCP client that speaks `initialize` + `tools/call`, (2) an `Action` enum mapping each neutral event to one ICM tool call, and (3) a `dispatch`/`run` entry point the CLI invokes. The dispatcher returns plain text; the active adapter formats it via a new `emit_hook_context` trait method (Claude Code emits `hookSpecificOutput.additionalContext`). The Claude Code adapter auto-emits three lifecycle hooks into `settings.json` when the memory backend is active for the scope, exactly like the existing auto-emitted `check-stale` SessionStart hook. All network calls are bounded by a tight timeout; any failure degrades to a one-line stderr warning and exit 0.

**Tech Stack:** Rust, tokio (already a dep), serde_json (already a dep), anyhow (already a dep), clap (already a dep). New deps: `reqwest` (HTTP client, rustls + json) and `wiremock` (dev-dependency, async mock HTTP server). Design spec: `docs/superpowers/specs/2026-05-28-lifecycle-memory-hooks-design.md`.

---

## Reference: existing code this plan builds on

- **Adapter trait:** `src/adapter/mod.rs:12-34` — `AgentAdapter` with `name`, `env_vars`, `materialize`. All methods use `anyhow::Result`. This plan adds `emit_hook_context`.
- **Claude Code adapter:** `src/adapter/claude_code.rs`. Settings/hook emission in `generate_settings_json` (`src/adapter/claude_code.rs:322`); the auto-emitted SessionStart `check-stale` hook is at `src/adapter/claude_code.rs:356-361` using the `STALE_CHECK_COMMAND` constant.
- **MCP resolution:** `src/mcp/resolve.rs`. `resolve_mcps(config, &active.tags)` returns `Vec<ResolvedMcp>`; the memory backend resolves to `ResolvedKind::Remote { url, transport }` named `MEMORY_MCP_NAME` (`"icm"`, `src/mcp/resolve.rs:43`). `resolve_memory` is private — do NOT make it public; instead scan `resolve_mcps` output for the entry named `MEMORY_MCP_NAME`.
- **Config + scope resolution (the canonical pattern, from `run_export`):** `src/cli/mod.rs:559-564`:

  ```rust
  let config_path = paths::config_path()?;
  let config = Config::load(&config_path)?;
  let env = crate::scope::matcher::Env::detect();
  let active = crate::scope::evaluate(&config, &env);
  // active.tags : BTreeSet<String>
  ```

- **ICM context chunk:** `src/icm.rs:36` `generate_context_chunk(active: &ActiveScopes, bundles: &[String]) -> String` — reuse for the Store action's payload.
- **CLI command enum + dispatch:** `src/cli/mod.rs:94-168` (`enum Command`) and `src/cli/mod.rs:178-` (the `match cli.command` block). Add a `HookRun` variant and its arm.

## File Structure

- **Create `src/hook_run/mod.rs`** — public `run(event: HookEvent) -> anyhow::Result<()>` entry called by the CLI; `HookEvent` enum; `dispatch(event) -> Vec<Action>`; the fail-soft wrapper (warn + exit 0).
- **Create `src/hook_run/action.rs`** — `Action` enum (`WakeUp`, `Recall`, `Store`); `Action::run(&self, client, ctx) -> anyhow::Result<String>` performing the tool call and returning result text.
- **Create `src/hook_run/mcp_client.rs`** — `McpHttpClient` with `new(url, timeout)`, `initialize()`, `call_tool(name, args) -> anyhow::Result<String>`. Minimal JSON-RPC over HTTP.
- **Modify `src/lib.rs`** — add `pub mod hook_run;`.
- **Modify `src/adapter/mod.rs:12-34`** — add `emit_hook_context` to the `AgentAdapter` trait.
- **Modify `src/adapter/claude_code.rs`** — implement `emit_hook_context`; add a constant for the three lifecycle hook commands; auto-emit them in `generate_settings_json` when the memory backend is active.
- **Modify `src/cli/mod.rs`** — add `HookRun { event: String }` command variant + match arm calling `hook_run::run`.
- **Modify `Cargo.toml`** — add `reqwest` dependency and `wiremock` dev-dependency.

---

## Task 1: Add HTTP client dependencies

**Files:**

- Modify: `Cargo.toml`

- [ ] **Step 1: Look up current stable versions**

Run: `cargo search reqwest --limit 1 && cargo search wiremock --limit 1`
Expected: prints the latest published versions (e.g. `reqwest = "0.12.x"`, `wiremock = "0.6.x"`). Use the exact versions printed — pin them (no `^`/`~`).

- [ ] **Step 2: Add `reqwest` to `[dependencies]`**

Add to the `[dependencies]` table in `Cargo.toml` (use the version from Step 1):

```toml
reqwest = { version = "0.12", default-features = false, features = ["rustls-tls", "json"] }
```

Rationale: `default-features = false` + `rustls-tls` avoids a system OpenSSL dependency (matches a single-binary distribution goal); `json` enables `.json()` request/response bodies.

- [ ] **Step 3: Add `wiremock` to `[dev-dependencies]`**

Add to (or create) the `[dev-dependencies]` table in `Cargo.toml`:

```toml
wiremock = "0.6"
```

- [ ] **Step 4: Verify it builds**

Run: `cargo build`
Expected: compiles clean (no code uses the crates yet; this just resolves them).

- [ ] **Step 5: Run crate-skills sync if the repo uses it**

Run: `ls .claude/ 2>/dev/null && echo "check for /sync-crate-skills"` — if the project has a crate-skills workflow, run it. Otherwise skip.
Expected: either a sync runs, or nothing to do.

- [ ] **Step 6: Commit**

```bash
git add Cargo.toml Cargo.lock
git commit -m "build: add reqwest and wiremock for lifecycle memory hooks"
```

---

## Task 2: Minimal HTTP MCP client

**Files:**

- Create: `src/hook_run/mcp_client.rs`
- Create: `src/hook_run/mod.rs` (module declaration only, fleshed out in Task 4)
- Modify: `src/lib.rs`

- [ ] **Step 1: Declare the module**

In `src/lib.rs`, add alongside the other `pub mod` lines:

```rust
pub mod hook_run;
```

In a new file `src/hook_run/mod.rs`, declare the submodule:

```rust
//! Engine-neutral agent lifecycle hooks that inject ICM memory context over MCP.

mod action;
mod mcp_client;
```

- [ ] **Step 2: Write the failing test for tool-call success**

Create `src/hook_run/mcp_client.rs` with this test module at the bottom (implementation comes next):

```rust
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use std::time::Duration;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn call_tool_returns_text_content() {
        let server = MockServer::start().await;
        // MCP tools/call response: result.content[0].text
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": {
                "content": [{ "type": "text", "text": "wake-up pack" }]
            }
        });
        Mock::given(method("POST"))
            .and(path("/"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(&server)
            .await;

        let client = McpHttpClient::new(server.uri(), Duration::from_secs(2));
        let text = client
            .call_tool("icm_wake_up", serde_json::json!({}))
            .await
            .expect("call_tool ok");
        assert_eq!(text, "wake-up pack");
    }

    #[tokio::test]
    async fn call_tool_errors_on_unreachable() {
        // Port 0 is never listening; connection fails fast.
        let client = McpHttpClient::new("http://127.0.0.1:0".to_string(), Duration::from_millis(200));
        let result = client.call_tool("icm_wake_up", serde_json::json!({})).await;
        assert!(result.is_err());
    }
}
```

- [ ] **Step 3: Run the test to verify it fails**

Run: `cargo test -p llmenv hook_run::mcp_client 2>&1 | tail -20` (adjust `-p` to the crate name if different; check `Cargo.toml [package] name`).
Expected: FAIL — `McpHttpClient` is not defined.

- [ ] **Step 4: Implement the client**

At the TOP of `src/hook_run/mcp_client.rs` (above the test module), add:

```rust
//! Minimal HTTP JSON-RPC MCP client — only the `tools/call` path this feature
//! needs. Not a general MCP library.

use std::time::Duration;

use anyhow::{Context, anyhow};
use serde_json::{Value, json};

/// A minimal MCP-over-HTTP client bound to one server URL with a fixed timeout.
#[derive(Debug, Clone)]
pub struct McpHttpClient {
    url: String,
    client: reqwest::Client,
}

impl McpHttpClient {
    /// Build a client for `url` whose every request is bounded by `timeout`.
    pub fn new(url: String, timeout: Duration) -> Self {
        // `build()` only fails on TLS backend init; default rustls is infallible
        // here, so fall back to a default client rather than panicking.
        let client = reqwest::Client::builder()
            .timeout(timeout)
            .build()
            .unwrap_or_default();
        Self { url, client }
    }

    /// Call one MCP tool and return the concatenated text content.
    ///
    /// # Errors
    /// Network failure, timeout, non-2xx status, a JSON-RPC `error` field, or a
    /// response missing `result.content[].text`.
    pub async fn call_tool(&self, name: &str, arguments: Value) -> anyhow::Result<String> {
        let req = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": { "name": name, "arguments": arguments }
        });
        let resp = self
            .client
            .post(&self.url)
            .json(&req)
            .send()
            .await
            .with_context(|| format!("POST {} for tool {name}", self.url))?
            .error_for_status()
            .with_context(|| format!("tool {name} returned an error status"))?;
        let body: Value = resp
            .json()
            .await
            .with_context(|| format!("decoding JSON response for tool {name}"))?;

        if let Some(err) = body.get("error") {
            return Err(anyhow!("tool {name} JSON-RPC error: {err}"));
        }
        extract_text(&body)
            .ok_or_else(|| anyhow!("tool {name} response missing result.content[].text"))
    }
}

/// Pull and concatenate every `text` entry from `result.content[]`.
fn extract_text(body: &Value) -> Option<String> {
    let content = body.get("result")?.get("content")?.as_array()?;
    let mut out = String::new();
    for item in content {
        if let Some(t) = item.get("text").and_then(Value::as_str) {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(t);
        }
    }
    Some(out)
}
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p llmenv hook_run::mcp_client 2>&1 | tail -20`
Expected: PASS (both `call_tool_returns_text_content` and `call_tool_errors_on_unreachable`).

- [ ] **Step 6: Add a test for the JSON-RPC error path**

Add to the test module in `src/hook_run/mcp_client.rs`:

```rust
    #[tokio::test]
    async fn call_tool_errors_on_jsonrpc_error() {
        let server = MockServer::start().await;
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "error": { "code": -32000, "message": "boom" }
        });
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(&server)
            .await;
        let client = McpHttpClient::new(server.uri(), Duration::from_secs(2));
        let result = client.call_tool("icm_wake_up", serde_json::json!({})).await;
        assert!(result.is_err());
    }
```

- [ ] **Step 7: Run it to verify pass**

Run: `cargo test -p llmenv hook_run::mcp_client 2>&1 | tail -20`
Expected: PASS (three tests).

- [ ] **Step 8: Commit**

```bash
git add src/lib.rs src/hook_run/mod.rs src/hook_run/mcp_client.rs
git commit -m "feat: add minimal HTTP MCP client for lifecycle hooks"
```

---

## Task 3: Action enum (event → ICM tool call)

**Files:**

- Create: `src/hook_run/action.rs`
- Modify: `src/hook_run/mcp_client.rs` (make `extract_text` reachable if needed — it stays private; no change expected)

- [ ] **Step 1: Write the failing test**

Create `src/hook_run/action.rs` with the test module first:

```rust
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn action_tool_name_mapping() {
        assert_eq!(Action::WakeUp.tool_name(), "icm_wake_up");
        assert_eq!(Action::Recall.tool_name(), "icm_memory_recall");
        assert_eq!(Action::Store.tool_name(), "icm_memory_store");
    }

    #[test]
    fn wakeup_arguments_are_empty_object() {
        let args = Action::WakeUp.arguments("query text", "chunk text");
        assert_eq!(args, serde_json::json!({}));
    }

    #[test]
    fn recall_arguments_carry_query() {
        let args = Action::Recall.arguments("rust, work", "chunk");
        assert_eq!(args["query"], serde_json::json!("rust, work"));
    }

    #[test]
    fn store_arguments_carry_content() {
        let args = Action::Store.arguments("query", "## llmenv context\n...");
        assert_eq!(args["content"], serde_json::json!("## llmenv context\n..."));
    }
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p llmenv hook_run::action 2>&1 | tail -20`
Expected: FAIL — `Action` not defined.

- [ ] **Step 3: Implement the Action enum**

At the TOP of `src/hook_run/action.rs`:

```rust
//! The memory action each lifecycle event performs, and how it maps to an ICM
//! MCP tool call.

use serde_json::{Value, json};

use crate::hook_run::mcp_client::McpHttpClient;

/// One memory action against the ICM MCP backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Inject the session wake-up pack (`icm_wake_up`).
    WakeUp,
    /// Inject recalled context for the active tags/project (`icm_memory_recall`).
    Recall,
    /// Best-effort store of the active scope context (`icm_memory_store`).
    Store,
}

impl Action {
    /// The ICM MCP tool this action calls.
    pub fn tool_name(self) -> &'static str {
        match self {
            Action::WakeUp => "icm_wake_up",
            Action::Recall => "icm_memory_recall",
            Action::Store => "icm_memory_store",
        }
    }

    /// Build the `arguments` object for this action's tool call. `query` is the
    /// recall query (active tags/project), `chunk` is the llmenv context chunk
    /// used as store content. Unused fields are ignored per action.
    pub fn arguments(self, query: &str, chunk: &str) -> Value {
        match self {
            Action::WakeUp => json!({}),
            Action::Recall => json!({ "query": query }),
            Action::Store => json!({ "content": chunk }),
        }
    }

    /// Execute the action: call the tool and return its text result.
    ///
    /// # Errors
    /// Propagates any client/network error from `call_tool`.
    pub async fn run(self, client: &McpHttpClient, query: &str, chunk: &str) -> anyhow::Result<String> {
        client.call_tool(self.tool_name(), self.arguments(query, chunk)).await
    }
}
```

- [ ] **Step 4: Declare the module**

In `src/hook_run/mod.rs`, the `mod action;` line from Task 2 already declares it. Confirm `mod action;` is present.

- [ ] **Step 5: Run the tests to verify pass**

Run: `cargo test -p llmenv hook_run::action 2>&1 | tail -20`
Expected: PASS (four tests).

- [ ] **Step 6: Commit**

```bash
git add src/hook_run/action.rs src/hook_run/mod.rs
git commit -m "feat: add lifecycle hook action-to-ICM-tool mapping"
```

---

## Task 4: Event dispatcher + fail-soft run entry

**Files:**

- Modify: `src/hook_run/mod.rs`

- [ ] **Step 1: Write the failing test for event parsing + dispatch**

Add a test module at the bottom of `src/hook_run/mod.rs`:

```rust
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn parses_neutral_event_names() {
        assert_eq!("session_start".parse::<HookEvent>().unwrap(), HookEvent::SessionStart);
        assert_eq!("turn_start".parse::<HookEvent>().unwrap(), HookEvent::TurnStart);
        assert_eq!("session_end".parse::<HookEvent>().unwrap(), HookEvent::SessionEnd);
    }

    #[test]
    fn rejects_unknown_event() {
        assert!("nope".parse::<HookEvent>().is_err());
    }

    #[test]
    fn dispatch_maps_events_to_actions() {
        assert_eq!(dispatch(HookEvent::SessionStart), vec![Action::WakeUp]);
        assert_eq!(dispatch(HookEvent::TurnStart), vec![Action::Recall]);
        assert_eq!(dispatch(HookEvent::SessionEnd), vec![Action::Store]);
    }
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p llmenv hook_run:: 2>&1 | tail -20`
Expected: FAIL — `HookEvent` / `dispatch` not defined.

- [ ] **Step 3: Implement `HookEvent`, `dispatch`, and `run`**

Replace the contents of `src/hook_run/mod.rs` (keeping the `mod action;` / `mod mcp_client;` lines) with:

```rust
//! Engine-neutral agent lifecycle hooks that inject ICM memory context over MCP.
//!
//! `run(event)` is the CLI entry. It resolves the active config, finds the
//! memory backend's HTTP URL, runs the actions configured for `event`, and
//! prints the adapter-formatted context to stdout. Every failure degrades to a
//! one-line stderr warning and exit 0 — lifecycle hooks run on the agent's hot
//! path and must never block it.

mod action;
mod mcp_client;

use std::str::FromStr;
use std::time::Duration;

use action::Action;
use mcp_client::McpHttpClient;

use crate::adapter::AgentAdapter;
use crate::adapter::claude_code::ClaudeCodeAdapter;
use crate::mcp::resolve::{MEMORY_MCP_NAME, ResolvedKind, resolve_mcps};

/// Per-call network timeout. Lifecycle hooks run on startup and every prompt, so
/// a slow/dead remote ICM must not stall the agent. 2s balances real round-trips
/// against not hanging the prompt.
const HOOK_TIMEOUT: Duration = Duration::from_secs(2);

/// An engine-neutral lifecycle event. Adapters translate these to native hook
/// names when wiring them into agent config.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookEvent {
    /// Session begins (Claude Code: `SessionStart`).
    SessionStart,
    /// A user prompt/turn begins (Claude Code: `UserPromptSubmit`).
    TurnStart,
    /// Session ends (Claude Code: `SessionEnd`).
    SessionEnd,
}

impl FromStr for HookEvent {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "session_start" => Ok(HookEvent::SessionStart),
            "turn_start" => Ok(HookEvent::TurnStart),
            "session_end" => Ok(HookEvent::SessionEnd),
            other => Err(anyhow::anyhow!(
                "unknown hook event '{other}' (expected session_start|turn_start|session_end)"
            )),
        }
    }
}

/// The ordered actions to run for an event. One per event today; the Vec leaves
/// room for an event to gain more actions later.
fn dispatch(event: HookEvent) -> Vec<Action> {
    match event {
        HookEvent::SessionStart => vec![Action::WakeUp],
        HookEvent::TurnStart => vec![Action::Recall],
        HookEvent::SessionEnd => vec![Action::Store],
    }
}

/// CLI entry. Fail-soft: a warning + empty stdout + exit 0 on any error. Returns
/// `Ok(())` even when the backend is unreachable.
pub fn run(event: &str) -> anyhow::Result<()> {
    let parsed = match HookEvent::from_str(event) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("llmenv: {e}");
            return Ok(());
        }
    };
    match run_inner(parsed) {
        Ok(text) => {
            let out = ClaudeCodeAdapter.emit_hook_context(&text);
            if !out.is_empty() {
                println!("{out}");
            }
        }
        Err(e) => {
            eprintln!("llmenv: memory {event} skipped: {e}");
        }
    }
    Ok(())
}

/// Resolve config, find the memory URL, run the event's actions, and return the
/// concatenated result text. Errors here are caught and warned by `run`.
fn run_inner(event: HookEvent) -> anyhow::Result<String> {
    let config_path = crate::paths::config_path()?;
    let config = crate::config::Config::load(&config_path)?;
    let env = crate::scope::matcher::Env::detect();
    let active = crate::scope::evaluate(&config, &env);

    let url = memory_url(&config, &active)
        .ok_or_else(|| anyhow::anyhow!("no memory backend active for this scope"))?;

    // Recall query: the sorted active tags. Store content: the llmenv context
    // chunk (tags/bundles/project). Bundles aren't needed for the query.
    let query = active.tags.iter().cloned().collect::<Vec<_>>().join(", ");
    let chunk = crate::icm::generate_context_chunk(&active, &[]);

    let client = McpHttpClient::new(url, HOOK_TIMEOUT);
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async {
        let mut out = String::new();
        for action in dispatch(event) {
            let text = action.run(&client, &query, &chunk).await?;
            if !text.is_empty() {
                if !out.is_empty() {
                    out.push_str("\n\n");
                }
                out.push_str(&text);
            }
        }
        Ok::<String, anyhow::Error>(out)
    })
}

/// Find the resolved memory backend's HTTP URL for the active tags, if any.
fn memory_url(
    config: &crate::config::Config,
    active: &crate::scope::ActiveScopes,
) -> Option<String> {
    let resolved = resolve_mcps(config, &active.tags).ok()?;
    resolved.into_iter().find_map(|m| match m.kind {
        ResolvedKind::Remote { url, .. } if m.name == MEMORY_MCP_NAME => Some(url),
        _ => None,
    })
}
```

- [ ] **Step 4: Run the dispatcher tests to verify pass**

Run: `cargo test -p llmenv hook_run:: 2>&1 | tail -20`
Expected: PASS (event parse + dispatch tests, plus Task 2/3 tests still green). `emit_hook_context` won't exist yet — if the crate fails to compile because of that call, proceed to Task 5 first, then return and run this. (To keep TDD order strict, Task 5 may be implemented before this Step 4 passes; that's acceptable since they're interdependent — implement Task 5, then both compile.)

- [ ] **Step 5: Commit (after Task 5 compiles)**

```bash
git add src/hook_run/mod.rs
git commit -m "feat: add lifecycle hook event dispatcher with fail-soft run"
```

---

## Task 5: Adapter `emit_hook_context`

**Files:**

- Modify: `src/adapter/mod.rs:12-34`
- Modify: `src/adapter/claude_code.rs`

- [ ] **Step 1: Write the failing test**

Add to the test module in `src/adapter/claude_code.rs` (find the existing `#[cfg(test)] mod tests` block; if none, add one):

```rust
    #[test]
    fn emit_hook_context_wraps_text() {
        let out = ClaudeCodeAdapter.emit_hook_context("hello ctx");
        let v: serde_json::Value = serde_json::from_str(&out).expect("valid JSON");
        assert_eq!(
            v["hookSpecificOutput"]["additionalContext"],
            serde_json::json!("hello ctx")
        );
    }

    #[test]
    fn emit_hook_context_empty_is_empty_string() {
        assert_eq!(ClaudeCodeAdapter.emit_hook_context(""), "");
    }
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p llmenv emit_hook_context 2>&1 | tail -20`
Expected: FAIL — method not on `AgentAdapter` / `ClaudeCodeAdapter`.

- [ ] **Step 3: Add the trait method**

In `src/adapter/mod.rs`, inside `trait AgentAdapter`, add after `materialize`:

```rust
    /// Format injected hook context in the engine's native hook-output shape so
    /// the agent runtime adds it to the model's context. Empty input returns an
    /// empty string, which suppresses any output.
    fn emit_hook_context(&self, text: &str) -> String;
```

- [ ] **Step 4: Implement it on the Claude Code adapter**

In `src/adapter/claude_code.rs`, inside `impl AgentAdapter for ClaudeCodeAdapter`, add:

```rust
    fn emit_hook_context(&self, text: &str) -> String {
        if text.is_empty() {
            return String::new();
        }
        serde_json::json!({
            "hookSpecificOutput": { "additionalContext": text }
        })
        .to_string()
    }
```

- [ ] **Step 5: Run the tests to verify pass**

Run: `cargo test -p llmenv emit_hook_context 2>&1 | tail -20`
Expected: PASS (both tests).

- [ ] **Step 6: Now compile the whole crate (Task 4 + Task 5 together)**

Run: `cargo test -p llmenv hook_run:: 2>&1 | tail -20`
Expected: PASS — Task 4's `run` now compiles because `emit_hook_context` exists.

- [ ] **Step 7: Commit**

```bash
git add src/adapter/mod.rs src/adapter/claude_code.rs
git commit -m "feat: add emit_hook_context adapter method"
```

(If Task 4's `src/hook_run/mod.rs` is still uncommitted from Task 4 Step 5, include it here or commit it separately now.)

---

## Task 6: Wire the CLI `hook-run` command

**Files:**

- Modify: `src/cli/mod.rs:94-168` (enum) and the `match cli.command` block (`src/cli/mod.rs:178-`)

- [ ] **Step 1: Add the command variant**

In `enum Command` (`src/cli/mod.rs`), add after the `Hook` variant (around `src/cli/mod.rs:115`):

```rust
    /// Run an agent lifecycle hook (injects ICM memory context over MCP).
    ///
    /// Invoked by the agent runtime, not by users directly. `event` is an
    /// engine-neutral name: session_start | turn_start | session_end.
    HookRun {
        /// Lifecycle event: session_start, turn_start, or session_end
        event: String,
    },
```

- [ ] **Step 2: Add the match arm**

In the `match cli.command` block, add after the `Hook` arm (around `src/cli/mod.rs:185-187`):

```rust
        Some(Command::HookRun { event }) => {
            crate::hook_run::run(&event)?;
        }
```

- [ ] **Step 3: Verify it builds and the command is wired**

Run: `cargo build && cargo run -p llmenv -- hook-run --help 2>&1 | tail -10`
Expected: build succeeds; `--help` shows the `hook-run` usage with the `event` arg.

- [ ] **Step 4: Manual fail-soft smoke check (no backend)**

Run: `cargo run -p llmenv -- hook-run session_start; echo "exit=$?"`
Expected: exit=0, and (with no memory backend active) a single stderr line like `llmenv: memory session_start skipped: no memory backend active for this scope`, empty stdout.

- [ ] **Step 5: Commit**

```bash
git add src/cli/mod.rs
git commit -m "feat: wire hook-run CLI command"
```

---

## Task 7: Auto-emit lifecycle hooks in settings.json when memory is active

**Files:**

- Modify: `src/adapter/claude_code.rs` (`generate_settings_json`, around `src/adapter/claude_code.rs:322-361`)

Background: `generate_settings_json` already auto-emits a SessionStart `check-stale` hook (`src/adapter/claude_code.rs:356-361`). This task adds three more lifecycle hooks — but only when the memory backend is active. The function currently takes `(out, manifest)`. Whether memory is active is derivable from the manifest's MCP entries: the resolved memory backend lands as an MCP named `MEMORY_MCP_NAME`. Confirm during implementation how resolved MCPs are stored on `MergedManifest` (search the struct); if the manifest does not carry resolved MCP names, thread an `memory_active: bool` parameter from the caller (`materialize`) which already has access to the resolved MCP list.

- [ ] **Step 1: Determine the memory-active signal**

Run: `grep -n "resolved_mcps\|mcps\|ResolvedMcp\|MergedManifest" src/merge/mod.rs src/materialize/mod.rs src/adapter/claude_code.rs | head -30`
Expected: shows whether resolved MCPs are on the manifest or passed separately. Decide: read from manifest if present, else add a `memory_active: bool` param to `generate_settings_json` and pass it from the call site.

- [ ] **Step 2: Write the failing test — hooks present when memory active**

Add to the test module in `src/adapter/claude_code.rs`. Build a `MergedManifest` (use an existing test helper if one exists — `grep -n "fn .*manifest\|MergedManifest {" src/adapter/claude_code.rs src/merge/mod.rs`) with the memory backend active, call `generate_settings_json` into a tempdir, read `settings.json`, and assert:

```rust
    #[test]
    fn settings_includes_lifecycle_hooks_when_memory_active() {
        let dir = tempfile::TempDir::new().unwrap();
        let manifest = manifest_with_memory_active(); // helper: memory backend selected
        generate_settings_json(dir.path(), &manifest).unwrap();
        let s = std::fs::read_to_string(dir.path().join("settings.json")).unwrap();
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        let cmds = collect_hook_commands(&v); // helper: flatten all hooks[].command
        assert!(cmds.iter().any(|c| c == "llmenv hook-run session_start"));
        assert!(cmds.iter().any(|c| c == "llmenv hook-run turn_start"));
        assert!(cmds.iter().any(|c| c == "llmenv hook-run session_end"));
    }

    #[test]
    fn settings_omits_lifecycle_hooks_when_memory_inactive() {
        let dir = tempfile::TempDir::new().unwrap();
        let manifest = manifest_without_memory(); // helper: no memory backend
        generate_settings_json(dir.path(), &manifest).unwrap();
        let s = std::fs::read_to_string(dir.path().join("settings.json")).unwrap();
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        let cmds = collect_hook_commands(&v);
        assert!(!cmds.iter().any(|c| c.starts_with("llmenv hook-run")));
        // check-stale is unconditional and must still be present:
        assert!(cmds.iter().any(|c| c == STALE_CHECK_COMMAND));
    }
```

Add the two test helpers (`manifest_with_memory_active`, `manifest_without_memory`, `collect_hook_commands`) in the same test module. For `collect_hook_commands`, walk `v["hooks"]` → each event array → each entry's `hooks` array → each handler's `command` string. Mirror the manifest-construction approach already used by existing `generate_settings_json` tests in this file.

- [ ] **Step 3: Run the tests to verify they fail**

Run: `cargo test -p llmenv settings_includes_lifecycle_hooks settings_omits_lifecycle_hooks 2>&1 | tail -30`
Expected: FAIL — the lifecycle hooks aren't emitted yet (first test fails; second may pass trivially).

- [ ] **Step 4: Add the command constants**

Near `STALE_CHECK_COMMAND` in `src/adapter/claude_code.rs`, add:

```rust
/// Lifecycle memory hooks, keyed by Claude Code's native event name. Emitted
/// into settings.json only when the memory backend is active for the scope. The
/// neutral event name is the `hook-run` argument; the map key is the native
/// Claude Code event the handler is registered under.
const LIFECYCLE_HOOKS: [(&str, &str); 3] = [
    ("SessionStart", "llmenv hook-run session_start"),
    ("UserPromptSubmit", "llmenv hook-run turn_start"),
    ("SessionEnd", "llmenv hook-run session_end"),
];
```

- [ ] **Step 5: Emit them conditionally**

In `generate_settings_json`, after the unconditional `check-stale` block (`src/adapter/claude_code.rs:356-361`) and before building `hooks_obj`, add:

```rust
    // Auto-wire lifecycle memory hooks when the memory backend is active for the
    // scope (mirrors the unconditional check-stale hook above, but gated). The
    // adapter owns the neutral→native event-name translation.
    if memory_active {
        for (native_event, command) in LIFECYCLE_HOOKS {
            hooks_by_event
                .entry(native_event.to_string())
                .or_default()
                .push(json!({
                    "hooks": [{ "type": "command", "command": command }],
                }));
        }
    }
```

Where `memory_active` is the boolean determined in Step 1 (read from the manifest, or a new parameter). If you added a parameter, update the signature and the single call site in `materialize`.

- [ ] **Step 6: Run the tests to verify pass**

Run: `cargo test -p llmenv settings_includes_lifecycle_hooks settings_omits_lifecycle_hooks 2>&1 | tail -30`
Expected: PASS (both).

- [ ] **Step 7: Run the full adapter test module to catch regressions**

Run: `cargo test -p llmenv adapter::claude_code 2>&1 | tail -20`
Expected: PASS — existing settings.json tests (including the check-stale assertion) still green.

- [ ] **Step 8: Commit**

```bash
git add src/adapter/claude_code.rs src/materialize/mod.rs
git commit -m "feat: auto-emit lifecycle memory hooks when memory backend active"
```

(Include `src/materialize/mod.rs` only if you threaded a `memory_active` parameter through the call site.)

---

## Task 8: Documentation

**Files:**

- Modify: `docs/commands.md`
- Modify: `docs/mcp.md`
- Modify: `CHANGELOG.md`

- [ ] **Step 1: Document `hook-run` in commands.md**

Add a `hook-run` entry to `docs/commands.md` describing: it's invoked by the agent runtime (not users), the three neutral events and what each does (`session_start`→wake-up, `turn_start`→recall, `session_end`→best-effort store), that it talks to the configured ICM memory MCP over HTTP, and that it fails soft (warning + exit 0). Note it is distinct from `hook zsh|bash` (shell integration).

- [ ] **Step 2: Document auto-wiring in mcp.md**

In `docs/mcp.md`, in the memory section, add a paragraph: when a scope's active tags select the `memory:` backend, llmenv auto-wires SessionStart/UserPromptSubmit/SessionEnd hooks (via `llmenv hook-run <event>`) that inject wake-up/recalled context and best-effort-store the active scope context. Mention the fail-soft behavior and that extraction-style memory creation remains the agent's job (nudged by the MCP server instructions).

- [ ] **Step 3: Add a CHANGELOG entry**

Under `## [Unreleased]` in `CHANGELOG.md`, add to (or create) an `### Added` subsection:

```markdown
- `llmenv hook-run <event>`: engine-neutral lifecycle hooks (session_start /
  turn_start / session_end) that inject ICM memory context over MCP. Auto-wired
  into the Claude Code adapter when a scope's active tags select the memory
  backend. Fail-soft: a dead/remote backend warns and never blocks the agent.
```

- [ ] **Step 4: Verify links resolve**

Run: `grep -n "hook-run" docs/commands.md docs/mcp.md CHANGELOG.md`
Expected: the new references appear in all three.

- [ ] **Step 5: Commit**

```bash
git add docs/commands.md docs/mcp.md CHANGELOG.md
git commit -m "docs: document hook-run lifecycle memory hooks"
```

---

## Task 9: Full quality gate

**Files:** none (verification only)

- [ ] **Step 1: Format check**

Run: `cargo fmt --check`
Expected: no diff. If it complains, run `cargo fmt` and commit the formatting separately.

- [ ] **Step 2: Clippy with warnings denied**

Run: `cargo clippy --all-targets --all-features -- -D warnings`
Expected: clean. Common fixes: the `Runtime::new()?` in `run_inner` is fine; if clippy flags `unwrap_or_default` on the reqwest builder, replace with an explicit `?` + map_err, or keep with an `#[expect(...)]` carrying a reason.

- [ ] **Step 3: Full test suite**

Run: `cargo test --all-features 2>&1 | tail -20`
Expected: all tests pass, including the existing 403+ and the new hook_run/adapter tests.

- [ ] **Step 4: Final manual smoke (fail-soft, real binary)**

Run: `cargo build --release && ./target/release/llmenv hook-run turn_start; echo "exit=$?"`
Expected: exit=0; with no active memory backend, one stderr warning and empty stdout.

- [ ] **Step 5: Commit any fixes**

```bash
git add -A
git commit -m "chore: clippy/fmt fixes for lifecycle memory hooks"
```

(Skip if Steps 1–3 were already clean.)

---

## Self-review notes (author)

- **Spec coverage:** session_start/turn_start/session_end → Tasks 3–4; HTTP MCP client → Task 2; adapter-routed emit → Task 5; auto-wiring gated by memory backend → Task 7; fail-soft + tight timeout + stderr warning → Task 4 (`run`) + Task 6 Step 4; non-goals (no transcript extraction, no `pre`, no `check-stale` fold-in, no Codex adapter) respected — none of those tasks exist. Testing strategy items all have tasks.
- **Open spec items resolved here:** timeout = 2s (`HOOK_TIMEOUT`); recall left uncapped (query is the tag list; the MCP returns a bounded pack).
- **Type consistency:** `McpHttpClient::new(String, Duration)` / `call_tool(&str, Value) -> Result<String>`; `Action::{tool_name, arguments, run}`; `HookEvent::{SessionStart, TurnStart, SessionEnd}`; `dispatch(HookEvent) -> Vec<Action>`; `run(&str)`; `emit_hook_context(&self, &str) -> String`. Names used identically across tasks.
- **Known follow-up (out of scope):** folding `check-stale` into `hook-run session_start`; a `doctor` line listing active lifecycle hooks; a Codex adapter mapping `session_end`.
