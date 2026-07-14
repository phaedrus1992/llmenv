<!-- markdownlint-disable MD013 -->
# ICM-Transcript Session Logging Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Record llmenv session activity (scope + lifecycle baseline, plus opt-in
verbose prompt/tool capture, plus internal-op diagnostics) into a unified event
stream that fans out to two independent sinks — a local JSONL file and ICM's
transcript store via the ICM MCP — discoverable by tag/bundle/project.

**Architecture:** New `src/session_log/` module owns a `SessionLogEvent` model and
three pure units (file sink, scope-header builder, transcript-arg builder) plus a
state map. Internal-op events are captured by a `tracing_subscriber::Layer` in the
main process (file sink only). Agent-session events (lifecycle, scope, prompts,
tools) are produced in `llmenv hook-run` hook processes, which append to the file
sink and dispatch transcript records to a **detached child** (`llmenv session-log`)
so hooks return instantly. All ICM interaction goes through the existing
`McpHttpClient` against the resolved `icm` MCP endpoint — never the `icm` CLI.

**Tech Stack:** Rust (workspace: root crate `llmenv` + `crates/llmenv-config`,
`crates/llmenv-paths`), `serde`/`serde_yaml`/`serde_json`, `tracing` +
`tracing-subscriber`, `reqwest` (in `McpHttpClient`), `tokio` current-thread
runtime, `proptest`, `tempfile`.

## Global Constraints

- **3.0 major release, branch from `main`.** The `session_log` config change is
  breaking; no back-compat shim. Issue #382, milestone "Large Features".
- **MCP only, never the `icm` CLI** for runtime ICM calls (AGENTS.md rule). Reuse
  `crate::hook_run::mcp_client::McpHttpClient::call_tool(name, arguments)`.
- **Disabled means disabled:** `session_log: { transcript: false, file: false }`
  → no session started, no hooks injected, nothing written. **Default (no
  `session_log` block) = `{ file: false, transcript: true, verbose: false }`.**
- **Never fail a launch, never block on the network.** Sinks are independent: MCP
  unreachable → only transcript sink no-ops; `file: true` still records all.
- **Code quality (CLAUDE.md):** ≤100 lines/function, complexity ≤8, ≤5 positional
  params, 100-char lines, absolute imports only (no `..`), JSDoc/rustdoc on public
  APIs, zero warnings. `for` over iterators where it reads clearer; newtypes/enums
  over primitives/bools; `thiserror`/`anyhow` per existing crate convention.
- **Tooling:** `cargo clippy --all-targets --all-features -- -D warnings`,
  `cargo fmt`, `cargo test`. Reuse `crate::hook_run::action::{tag_keyword,
  bundle_keyword}` for the `llmenv-tag:` / `llmenv-bundle:` tokens — do not
  re-implement the prefix.
- **State path:** `llmenv_paths::state_dir()` (`~/.local/state/llmenv` or
  `$LLMENV_STATE_DIR`), files written 0o600 via
  `llmenv_paths::write_owner_only_atomic`.

---

## File Structure

- `crates/llmenv-config/src/schema.rs` — replace `session_log: Option<String>`
  (line 100) with `Option<SessionLog>`; add `SessionLog` struct + custom
  deserialize. Modify.
- `crates/llmenv-config/src/lib.rs` — update `session_log_*` tests; add resolver.
- `src/session_log/mod.rs` — module API: `SessionLogEvent`, `EventKind`,
  `EventScope`, `emit`/`init` for the in-process file path. Create.
- `src/session_log/event.rs` — event model + `to_jsonl` + `transcript_args`. Create.
- `src/session_log/file_sink.rs` — append JSONL, 0o600. Create.
- `src/session_log/scope_header.rs` — scope-header content + metadata builder. Create.
- `src/session_log/transcript.rs` — MCP arg builders + `record`/`start_session`
  calls via `McpHttpClient`. Create.
- `src/session_log/state.rs` — `claude_session_id → icm_session_id` map. Create.
- `src/session_log/tracing_layer.rs` — `tracing_subscriber::Layer` → file sink. Create.
- `src/main.rs` — replace old `session_log` file-layer wiring (lines 1-45). Modify.
- `src/hook_run/mod.rs` — emit lifecycle/scope events; new `HookEvent` variants;
  dispatch a transcript record. Modify.
- `src/cli/mod.rs` — register `llmenv session-log` internal subcommand. Modify.
- `src/adapter/claude_code.rs` — inject baseline + verbose hooks. Modify.
- `examples/config-llmenv-dir/config.yaml` — documented `session_log:` block. Modify.
- `docs/` + `CHANGELOG.md` — user docs + Unreleased entry. Modify.

---

## Phase 1 — Config, event model, file sink

### Task 1: `SessionLog` config struct (breaking replace)

**Files:**

- Modify: `crates/llmenv-config/src/schema.rs:100`
- Modify: `crates/llmenv-config/src/lib.rs:83-99` (existing tests)
- Test: `crates/llmenv-config/src/schema.rs` (inline `#[cfg(test)]`)

**Interfaces:**

- Produces: `pub struct SessionLog { pub file: bool, pub transcript: bool,
  pub verbose: bool, pub path: Option<String>, pub max_content_bytes: Option<usize> }`;
  `impl Default for SessionLog` → `{ file: false, transcript: true, verbose:
  false, path: None, max_content_bytes: None }`. `Config::session_log:
  Option<SessionLog>`. `Config::session_log_resolved(&self) -> SessionLog` =
  `self.session_log.clone().unwrap_or_default()`.

- [ ] **Step 1: Write the failing tests** (replace the two existing
  `session_log_*` tests in `lib.rs` and add struct tests)

```rust
// crates/llmenv-config/src/lib.rs (replace session_log_defaults_to_none and
// session_log_parses_path)
#[test]
fn session_log_absent_resolves_to_transcript_on() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path().join("config.yaml");
    std::fs::write(&p, "cache: {}\n").unwrap();
    let cfg = Config::load(&p).unwrap();
    assert!(cfg.session_log.is_none());
    let resolved = cfg.session_log_resolved();
    assert!(resolved.transcript, "default must enable transcript");
    assert!(!resolved.file);
    assert!(!resolved.verbose);
}

#[test]
fn session_log_table_parses_flags() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path().join("config.yaml");
    std::fs::write(
        &p,
        "session_log:\n  file: true\n  transcript: false\n  verbose: true\n",
    )
    .unwrap();
    let cfg = Config::load(&p).unwrap();
    let r = cfg.session_log_resolved();
    assert!(r.file && !r.transcript && r.verbose);
}

#[test]
fn session_log_bare_string_is_rejected_with_migration_hint() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path().join("config.yaml");
    std::fs::write(&p, "session_log: /tmp/session.jsonl\n").unwrap();
    let err = Config::load(&p).unwrap_err().to_string();
    assert!(err.contains("session_log"), "error names the field");
    assert!(err.contains("file: true"), "error shows the migration");
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p llmenv-config session_log`
Expected: FAIL — `session_log_resolved` undefined, string still parses.

- [ ] **Step 3: Implement the struct + custom deserialize + resolver**

```rust
// crates/llmenv-config/src/schema.rs — replace the `session_log` field (line 100)
    /// Session logging configuration. Absent → ICM transcript on, file + verbose
    /// off (see `Config::session_log_resolved`). Was a bare path string before
    /// 3.0; that form is now rejected with a migration hint.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_log: Option<SessionLog>,
}

/// Where and how llmenv records session activity. `file` and `transcript` are
/// independent sinks that receive the same event stream; `verbose` adds
/// per-hook prompt/tool detail to it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SessionLog {
    /// Append the session-event stream as JSONL to `path` (or the default).
    pub file: bool,
    /// Record the same stream to ICM transcripts via the ICM MCP.
    pub transcript: bool,
    /// Include per-hook prompt/tool-use events in the stream.
    pub verbose: bool,
    /// Override the file-sink path (default `<state_dir>/session-log.jsonl`).
    pub path: Option<String>,
    /// Truncate event content to this many bytes (default 16384).
    pub max_content_bytes: Option<usize>,
}

impl Default for SessionLog {
    fn default() -> Self {
        Self { file: false, transcript: true, verbose: false, path: None, max_content_bytes: None }
    }
}

// Reject the pre-3.0 bare-string form with a clear migration message, accept a
// mapping via a shadow struct that carries the same defaults.
impl<'de> serde::Deserialize<'de> for SessionLog {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Shadow {
            #[serde(default)] file: bool,
            #[serde(default = "default_true")] transcript: bool,
            #[serde(default) ] verbose: bool,
            #[serde(default)] path: Option<String>,
            #[serde(default)] max_content_bytes: Option<usize>,
        }
        let v = serde_yaml::Value::deserialize(d)?;
        if v.is_string() {
            return Err(serde::de::Error::custom(
                "session_log is now a mapping, not a path string; use \
                 `session_log: { file: true }` (file path overridable via `path:`)",
            ));
        }
        let s: Shadow = serde_yaml::from_value(v).map_err(serde::de::Error::custom)?;
        Ok(SessionLog { file: s.file, transcript: s.transcript, verbose: s.verbose,
            path: s.path, max_content_bytes: s.max_content_bytes })
    }
}

fn default_true() -> bool { true }
```

```rust
// crates/llmenv-config/src/lib.rs — add to `impl Config`
impl Config {
    /// Effective session-logging config: an absent block means ICM transcript
    /// on, file + verbose off.
    #[must_use]
    pub fn session_log_resolved(&self) -> SessionLog {
        self.session_log.clone().unwrap_or_default()
    }
}
```

Add `pub use schema::SessionLog;` next to the existing schema re-exports in
`lib.rs` (match the surrounding re-export style).

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p llmenv-config session_log`
Expected: PASS (3 tests).

- [ ] **Step 5: Fix the breaking call site in `src/main.rs`** so the workspace
  compiles (full rewrite is Task 10; here just stop the build break)

Replace the body of `fn main`'s session-log lines (`src/main.rs:22-26`) with:

```rust
    let resolved = llmenv_paths::config_path()
        .ok()
        .and_then(|p| llmenv_config::Config::load(&p).ok())
        .map(|c| c.session_log_resolved())
        .unwrap_or_default();
    let session_log_path = resolved.file.then(|| {
        resolved.path.clone().unwrap_or_else(crate::session_log::default_file_path_string)
    });
```

Leave the existing `open_session_log`/`file_layer` logic consuming
`session_log_path: Option<String>` for now (Task 10 replaces it). Add a
temporary `pub(crate) fn default_file_path_string() -> String` stub in a new
`src/session_log/mod.rs` returning `"/dev/null".into()` — Task 3 implements it
properly. Register `mod session_log;` in `src/lib.rs` (alphabetical with the
other `pub mod` lines) and `use` it in `main.rs`.

- [ ] **Step 6: Verify the workspace builds and validate.rs fixtures still compile**

Run: `cargo build && cargo test -p llmenv-config`
Expected: PASS. The ~30 `session_log: None` fixtures in `validate.rs` still
compile because the field stays `Option<_>`.

- [ ] **Step 7: Commit**

```bash
git add crates/llmenv-config/src/schema.rs crates/llmenv-config/src/lib.rs \
        src/main.rs src/session_log/mod.rs src/lib.rs
git commit -m "feat: session_log config becomes a table (transcript on by default)"
```

---

### Task 2: `SessionLogEvent` model + JSONL serialization

**Files:**

- Create: `src/session_log/event.rs`
- Modify: `src/session_log/mod.rs` (declare `pub mod event; pub use event::*;`)
- Test: inline `#[cfg(test)]` in `event.rs`

**Interfaces:**

- Produces:
  `pub enum EventScope { AgentSession, Process }`
  `pub enum EventKind { LifecycleStart, Scope, Internal, Prompt, ToolUse, ToolResult, Notification, Stop, LifecycleEnd }`
  `pub struct SessionLogEvent { pub ts: String, pub kind: EventKind, pub scope: EventScope, pub role: String, pub tool_name: Option<String>, pub tokens: Option<u64>, pub level: Option<String>, pub content: String, pub fields: serde_json::Value }`
  `impl SessionLogEvent { pub fn to_jsonl(&self) -> String; pub fn truncated(self, max: usize) -> Self; }`
- Consumes: nothing.

- [ ] **Step 1: Write the failing test**

```rust
// src/session_log/event.rs
#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn ev() -> SessionLogEvent {
        SessionLogEvent {
            ts: "2026-06-30T00:00:00Z".into(),
            kind: EventKind::Prompt,
            scope: EventScope::AgentSession,
            role: "user".into(),
            tool_name: None,
            tokens: None,
            level: None,
            content: "hello".into(),
            fields: serde_json::json!({}),
        }
    }

    #[test]
    fn to_jsonl_is_one_line_with_required_fields() {
        let line = ev().to_jsonl();
        assert!(!line.contains('\n'));
        let v: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(v["kind"], "prompt");
        assert_eq!(v["role"], "user");
        assert_eq!(v["content"], "hello");
        assert_eq!(v["ts"], "2026-06-30T00:00:00Z");
    }

    #[test]
    fn truncated_caps_content_bytes() {
        let mut e = ev();
        e.content = "x".repeat(100);
        let t = e.truncated(10);
        assert_eq!(t.content.len(), 10);
    }

    #[test]
    fn truncated_is_noop_when_within_cap() {
        let t = ev().truncated(1000);
        assert_eq!(t.content, "hello");
    }
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p llmenv session_log::event`
Expected: FAIL — types undefined.

- [ ] **Step 3: Implement the model**

```rust
//! The single event type session logging produces, and how it renders to a
//! JSONL line. The transcript-record mapping lives in `transcript.rs`.

use serde::Serialize;

/// Whether an event belongs to a correlated agent transcript session or is a
/// process-level llmenv diagnostic (no session to attach to).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EventScope {
    AgentSession,
    Process,
}

/// The kind of session event. Drives transcript `role`/`kind` labelling.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    LifecycleStart,
    Scope,
    Internal,
    Prompt,
    ToolUse,
    ToolResult,
    Notification,
    Stop,
    LifecycleEnd,
}

/// One session-logging event. Both sinks consume this; the file sink writes
/// `to_jsonl`, the transcript sink maps it via `transcript::record_args`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SessionLogEvent {
    pub ts: String,
    pub kind: EventKind,
    pub scope: EventScope,
    pub role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub level: Option<String>,
    pub content: String,
    pub fields: serde_json::Value,
}

impl SessionLogEvent {
    /// Render to a single JSONL line (no trailing newline).
    #[must_use]
    pub fn to_jsonl(&self) -> String {
        // Serialize is infallible for this shape; fall back to a minimal line.
        serde_json::to_string(self).unwrap_or_else(|_| {
            format!("{{\"ts\":\"{}\",\"kind\":\"internal\"}}", self.ts)
        })
    }

    /// Cap `content` to `max` bytes (on a char boundary), returning self.
    #[must_use]
    pub fn truncated(mut self, max: usize) -> Self {
        if self.content.len() > max {
            let mut end = max;
            while !self.content.is_char_boundary(end) {
                end -= 1;
            }
            self.content.truncate(end);
        }
        self
    }
}
```

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test -p llmenv session_log::event`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/session_log/event.rs src/session_log/mod.rs
git commit -m "feat: add SessionLogEvent model and JSONL rendering"
```

---

### Task 3: File sink (append JSONL, 0o600)

**Files:**

- Create: `src/session_log/file_sink.rs`
- Modify: `src/session_log/mod.rs` (declare module; implement
  `default_file_path_string` for real, replacing the Task 1 stub)
- Test: inline + a `tempfile`-based test

**Interfaces:**

- Produces: `pub struct FileSink { path: std::path::PathBuf }`;
  `impl FileSink { pub fn new(path: PathBuf) -> Self; pub fn append(&self, line: &str); }`;
  `pub fn default_file_path() -> anyhow::Result<PathBuf>` (=
  `state_dir()/session-log.jsonl`); `pub fn default_file_path_string() -> String`.
- Consumes: `llmenv_paths::{state_dir, write_owner_only_atomic}`.

- [ ] **Step 1: Write the failing test**

```rust
// src/session_log/file_sink.rs
#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn append_writes_lines_and_preserves_existing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session-log.jsonl");
        let sink = FileSink::new(path.clone());
        sink.append("{\"a\":1}");
        sink.append("{\"b\":2}");
        let body = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = body.lines().collect();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0], "{\"a\":1}");
        assert_eq!(lines[1], "{\"b\":2}");
    }

    #[cfg(unix)]
    #[test]
    fn append_creates_owner_only_file() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        FileSink::new(path.clone()).append("{}");
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o077, 0, "group/other bits must be unset: {mode:o}");
    }
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p llmenv session_log::file_sink`
Expected: FAIL — `FileSink` undefined.

- [ ] **Step 3: Implement the file sink**

```rust
//! Local JSONL sink. Append-only, owner-only, best-effort: a write failure logs
//! at `debug!` and is dropped — session logging never fails a launch.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;

use llmenv_paths::state_dir;

/// Default file-sink path: `<state_dir>/session-log.jsonl`.
///
/// # Errors
/// Propagates `state_dir()` resolution failure.
pub fn default_file_path() -> anyhow::Result<PathBuf> {
    Ok(state_dir()?.join("session-log.jsonl"))
}

/// `default_file_path` as a string, falling back to a relative name if the
/// state dir cannot be resolved (the open will then fail-soft).
#[must_use]
pub fn default_file_path_string() -> String {
    default_file_path()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| "session-log.jsonl".to_string())
}

/// Appends rendered events to one JSONL file.
#[derive(Debug, Clone)]
pub struct FileSink {
    path: PathBuf,
}

impl FileSink {
    /// Create a sink writing to `path`. The parent dir is created on first
    /// append.
    #[must_use]
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    /// Append one line (a `\n` is added). Best-effort; errors are logged and
    /// dropped.
    pub fn append(&self, line: &str) {
        if let Err(e) = self.try_append(line) {
            tracing::debug!("session_log file append failed: {e}");
        }
    }

    fn try_append(&self, line: &str) -> std::io::Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut opts = OpenOptions::new();
        opts.create(true).append(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut f = opts.open(&self.path)?;
        f.write_all(line.as_bytes())?;
        f.write_all(b"\n")
    }
}
```

Replace the Task 1 stub `default_file_path_string` in `mod.rs` with
`pub use file_sink::{default_file_path, default_file_path_string, FileSink};`.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p llmenv session_log::file_sink`
Expected: PASS (2 tests).

- [ ] **Step 5: Commit**

```bash
git add src/session_log/file_sink.rs src/session_log/mod.rs
git commit -m "feat: add append-only owner-only session-log file sink"
```

---

### Task 4: Scope-header content + metadata builder

**Files:**

- Create: `src/session_log/scope_header.rs`
- Modify: `src/session_log/mod.rs`
- Test: inline + property test

**Interfaces:**

- Produces:
  `pub struct ScopeContext { pub tags: Vec<String>, pub bundles: Vec<String>, pub project: Option<String>, pub cwd: String, pub adapter: String, pub llmenv_version: String }`
  `pub fn scope_header_content(ctx: &ScopeContext) -> String` (FTS-searchable, embeds `llmenv-tag:` / `llmenv-bundle:` tokens)
  `pub fn scope_metadata_json(ctx: &ScopeContext) -> serde_json::Value`
- Consumes: `crate::hook_run::action::{tag_keyword, bundle_keyword}`.

- [ ] **Step 1: Write the failing test**

```rust
// src/session_log/scope_header.rs
#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn ctx() -> ScopeContext {
        ScopeContext {
            tags: vec!["rust".into(), "work-vpn".into()],
            bundles: vec!["base".into()],
            project: Some("llmenv".into()),
            cwd: "/Users/x/git/llmenv".into(),
            adapter: "claude_code".into(),
            llmenv_version: "3.0.0".into(),
        }
    }

    #[test]
    fn content_embeds_searchable_tag_and_bundle_tokens() {
        let c = scope_header_content(&ctx());
        assert!(c.contains("llmenv-tag:rust"));
        assert!(c.contains("llmenv-tag:work-vpn"));
        assert!(c.contains("llmenv-bundle:base"));
        assert!(c.contains("llmenv"), "project name present");
    }

    #[test]
    fn metadata_carries_full_structured_fields() {
        let m = scope_metadata_json(&ctx());
        assert_eq!(m["tags"], serde_json::json!(["rust", "work-vpn"]));
        assert_eq!(m["bundles"], serde_json::json!(["base"]));
        assert_eq!(m["adapter"], "claude_code");
        assert_eq!(m["llmenv_version"], "3.0.0");
    }
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p llmenv session_log::scope_header`
Expected: FAIL — undefined.

- [ ] **Step 3: Implement the builder**

```rust
//! Builds the scope-header event's content + metadata. Content carries the
//! `llmenv-tag:` / `llmenv-bundle:` tokens so ICM's content-only FTS can find a
//! session by the scope that produced it. Tokens reuse the existing keyword
//! helpers so the encoding never drifts.

use crate::hook_run::action::{bundle_keyword, tag_keyword};

/// The active llmenv scope at session start.
#[derive(Debug, Clone)]
pub struct ScopeContext {
    pub tags: Vec<String>,
    pub bundles: Vec<String>,
    pub project: Option<String>,
    pub cwd: String,
    pub adapter: String,
    pub llmenv_version: String,
}

/// FTS-searchable header line(s): project plus one `llmenv-tag:<t>` /
/// `llmenv-bundle:<b>` token per active scope element.
#[must_use]
pub fn scope_header_content(ctx: &ScopeContext) -> String {
    let mut parts: Vec<String> = Vec::new();
    parts.push("llmenv session".to_string());
    if let Some(p) = &ctx.project {
        parts.push(format!("project:{p}"));
    }
    for t in &ctx.tags {
        parts.push(tag_keyword(t));
    }
    for b in &ctx.bundles {
        parts.push(bundle_keyword(b));
    }
    parts.join(" ")
}

/// Full structured session metadata for exact inspection / replay.
#[must_use]
pub fn scope_metadata_json(ctx: &ScopeContext) -> serde_json::Value {
    serde_json::json!({
        "tags": ctx.tags,
        "bundles": ctx.bundles,
        "project": ctx.project,
        "cwd": ctx.cwd,
        "adapter": ctx.adapter,
        "llmenv_version": ctx.llmenv_version,
    })
}
```

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test -p llmenv session_log::scope_header`
Expected: PASS.

- [ ] **Step 5: Add a property test** (tags are pre-validated by
  `hook_run::validate_tag`; assert every tag yields a findable token)

```rust
    use proptest::prelude::*;
    proptest! {
        #[test]
        fn every_tag_appears_as_a_token(tags in proptest::collection::vec("[a-z0-9_-]{1,12}", 0..5)) {
            let c = scope_header_content(&ScopeContext {
                tags: tags.clone(), bundles: vec![], project: None,
                cwd: "/".into(), adapter: "claude_code".into(), llmenv_version: "3.0.0".into(),
            });
            for t in &tags {
                prop_assert!(c.contains(&format!("llmenv-tag:{t}")));
            }
        }
    }
```

- [ ] **Step 6: Run + commit**

Run: `cargo test -p llmenv session_log::scope_header`
Expected: PASS.

```bash
git add src/session_log/scope_header.rs src/session_log/mod.rs
git commit -m "feat: add scope-header content and metadata builder"
```

---

## Phase 2 — Transcript sink (MCP) + state map

### Task 5: Transcript MCP arg builders

**Files:**

- Create: `src/session_log/transcript.rs`
- Modify: `src/session_log/mod.rs`
- Test: inline

**Interfaces:**

- Produces:
  `pub fn start_session_args(agent: &str, project: Option<&str>, metadata: &serde_json::Value) -> serde_json::Value`
  `pub fn record_args(session_id: &str, ev: &SessionLogEvent) -> serde_json::Value`
  `pub const START_TOOL: &str = "icm_transcript_start_session";`
  `pub const RECORD_TOOL: &str = "icm_transcript_record";`
- Consumes: `super::event::SessionLogEvent`.

- [ ] **Step 1: Write the failing test**

```rust
// src/session_log/transcript.rs
#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::session_log::event::{EventKind, EventScope, SessionLogEvent};

    fn ev() -> SessionLogEvent {
        SessionLogEvent {
            ts: "t".into(), kind: EventKind::ToolUse, scope: EventScope::AgentSession,
            role: "tool".into(), tool_name: Some("Bash".into()), tokens: Some(5),
            level: None, content: "ls".into(), fields: serde_json::json!({"k":1}),
        }
    }

    #[test]
    fn start_session_args_shape() {
        let a = start_session_args("claude_code", Some("llmenv"),
            &serde_json::json!({"tags":["rust"]}));
        assert_eq!(a["agent"], "claude_code");
        assert_eq!(a["project"], "llmenv");
        assert_eq!(a["metadata"]["tags"], serde_json::json!(["rust"]));
    }

    #[test]
    fn record_args_map_event_fields() {
        let a = record_args("sess-1", &ev());
        assert_eq!(a["session_id"], "sess-1");
        assert_eq!(a["role"], "tool");
        assert_eq!(a["content"], "ls");
        assert_eq!(a["tool_name"], "Bash");
        assert_eq!(a["tokens"], 5);
        // structured event fields ride along as metadata (string-encoded JSON,
        // per the MCP tool's `metadata: string` contract).
        assert!(a["metadata"].as_str().unwrap().contains("\"k\":1"));
    }
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p llmenv session_log::transcript`
Expected: FAIL — undefined.

- [ ] **Step 3: Implement** (note: ICM's `metadata` MCP arg is a JSON **string**)

```rust
//! Maps session-log events to ICM transcript MCP tool-call arguments. The calls
//! themselves go through `McpHttpClient` (see `dispatch.rs`); these are pure
//! argument builders so they are unit-testable without a server.

use serde_json::{json, Value};

use crate::session_log::event::SessionLogEvent;

/// ICM MCP tool: create a transcript session, returns its id as text.
pub const START_TOOL: &str = "icm_transcript_start_session";
/// ICM MCP tool: append one message to a session.
pub const RECORD_TOOL: &str = "icm_transcript_record";

/// Arguments for `icm_transcript_start_session`.
#[must_use]
pub fn start_session_args(agent: &str, project: Option<&str>, metadata: &Value) -> Value {
    json!({
        "agent": agent,
        "project": project,
        // ICM stores session metadata as a JSON string.
        "metadata": metadata.to_string(),
    })
}

/// Arguments for `icm_transcript_record` for `ev` in session `session_id`.
#[must_use]
pub fn record_args(session_id: &str, ev: &SessionLogEvent) -> Value {
    let mut a = json!({
        "session_id": session_id,
        "role": ev.role,
        "content": ev.content,
        "metadata": ev.fields.to_string(),
    });
    if let Some(t) = &ev.tool_name {
        a["tool_name"] = json!(t);
    }
    if let Some(n) = ev.tokens {
        a["tokens"] = json!(n);
    }
    a
}
```

- [ ] **Step 4: Run + commit**

Run: `cargo test -p llmenv session_log::transcript`
Expected: PASS.

```bash
git add src/session_log/transcript.rs src/session_log/mod.rs
git commit -m "feat: add ICM transcript MCP argument builders"
```

---

### Task 6: Session-id state map

**Files:**

- Create: `src/session_log/state.rs`
- Modify: `src/session_log/mod.rs`
- Test: inline + property test (mirror `icm.rs` perms test)

**Interfaces:**

- Produces:
  `pub fn lookup_session(claude_session_id: &str) -> Option<String>`
  `pub fn record_session(claude_session_id: &str, icm_session_id: &str) -> anyhow::Result<()>`
  `pub fn state_path() -> anyhow::Result<PathBuf>` (= `state_dir()/transcript-sessions.json`)
- Consumes: `llmenv_paths::{state_dir, write_owner_only_atomic}`.

- [ ] **Step 1: Write the failing test**

```rust
// src/session_log/state.rs
#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn record_then_lookup_roundtrips() {
        let dir = tempfile::tempdir().unwrap();
        // SAFETY: single-threaded test; isolates the state dir.
        unsafe { std::env::set_var("LLMENV_STATE_DIR", dir.path()) };
        assert_eq!(lookup_session("claude-1"), None);
        record_session("claude-1", "icm-aaa").unwrap();
        record_session("claude-2", "icm-bbb").unwrap();
        assert_eq!(lookup_session("claude-1").as_deref(), Some("icm-aaa"));
        assert_eq!(lookup_session("claude-2").as_deref(), Some("icm-bbb"));
        assert_eq!(lookup_session("missing"), None);
    }
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p llmenv session_log::state`
Expected: FAIL — undefined.

- [ ] **Step 3: Implement** (JSON map, atomic owner-only write, read-modify-write)

```rust
//! Persists the `claude_session_id -> icm_session_id` correlation under the
//! stable state dir so every hook process for a launch records into the same
//! transcript session.

use std::collections::BTreeMap;
use std::path::PathBuf;

use llmenv_paths::{state_dir, write_owner_only_atomic};

/// Path to the correlation map file.
///
/// # Errors
/// Propagates `state_dir()` failure.
pub fn state_path() -> anyhow::Result<PathBuf> {
    Ok(state_dir()?.join("transcript-sessions.json"))
}

fn load() -> BTreeMap<String, String> {
    state_path()
        .ok()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

/// The ICM session id for a Claude session, if recorded.
#[must_use]
pub fn lookup_session(claude_session_id: &str) -> Option<String> {
    load().get(claude_session_id).cloned()
}

/// Record the correlation (read-modify-write, atomic, 0o600).
///
/// # Errors
/// Path resolution or atomic-write failure.
pub fn record_session(claude_session_id: &str, icm_session_id: &str) -> anyhow::Result<()> {
    let path = state_path()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut map = load();
    map.insert(claude_session_id.to_string(), icm_session_id.to_string());
    let body = serde_json::to_string(&map)?;
    write_owner_only_atomic(&path, body.as_bytes())?;
    Ok(())
}
```

- [ ] **Step 4: Add the perms property test**

```rust
    use proptest::prelude::*;
    proptest! {
        #[test]
        fn state_file_is_owner_only(id in "[a-z0-9-]{1,16}") {
            let dir = tempfile::tempdir().unwrap();
            unsafe { std::env::set_var("LLMENV_STATE_DIR", dir.path()) };
            record_session(&id, "icm-x").unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mode = std::fs::metadata(state_path().unwrap()).unwrap()
                    .permissions().mode();
                prop_assert_eq!(mode & 0o077, 0);
            }
        }
    }
```

- [ ] **Step 5: Run + commit**

Run: `cargo test -p llmenv session_log::state`
Expected: PASS.

```bash
git add src/session_log/state.rs src/session_log/mod.rs
git commit -m "feat: persist claude->icm transcript session correlation map"
```

---

### Task 7: Transcript dispatch via `McpHttpClient` (start + record)

**Files:**

- Create: `src/session_log/dispatch.rs`
- Modify: `src/session_log/mod.rs`
- Test: inline using `wiremock` (already a dev-dep — see `mcp_client.rs` tests)

**Interfaces:**

- Consumes: `McpHttpClient::{new, call_tool}`, `transcript::{start_session_args,
  record_args, START_TOOL, RECORD_TOOL}`, `event::SessionLogEvent`.
- Produces (async, run inside a current-thread runtime like `hook_run::run_inner`):
  `pub async fn start_session(client: &McpHttpClient, agent: &str, project: Option<&str>, metadata: &Value) -> anyhow::Result<String>` (returns the icm session id, parsed from tool text)
  `pub async fn record(client: &McpHttpClient, session_id: &str, ev: &SessionLogEvent) -> anyhow::Result<()>`

- [ ] **Step 1: Write the failing test** (mirror the wiremock setup in
  `src/hook_run/mcp_client.rs` tests)

```rust
// src/session_log/dispatch.rs
#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::session_log::event::{EventKind, EventScope, SessionLogEvent};
    use std::time::Duration;
    use wiremock::matchers::method;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn text_result(text: &str) -> serde_json::Value {
        serde_json::json!({"jsonrpc":"2.0","id":1,
            "result":{"content":[{"type":"text","text":text}]}})
    }

    #[tokio::test]
    async fn start_session_parses_returned_id() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(text_result("sess-42")))
            .mount(&server).await;
        let client = McpHttpClient::new(server.uri(), Duration::from_secs(2)).unwrap();
        let id = start_session(&client, "claude_code", Some("llmenv"),
            &serde_json::json!({})).await.unwrap();
        assert_eq!(id, "sess-42");
    }

    #[tokio::test]
    async fn record_posts_without_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(text_result("ok")))
            .mount(&server).await;
        let client = McpHttpClient::new(server.uri(), Duration::from_secs(2)).unwrap();
        let ev = SessionLogEvent { ts: "t".into(), kind: EventKind::Prompt,
            scope: EventScope::AgentSession, role: "user".into(), tool_name: None,
            tokens: None, level: None, content: "hi".into(), fields: serde_json::json!({}) };
        record(&client, "sess-42", &ev).await.unwrap();
    }
}
```

> Note: `McpHttpClient::new` SSRF-rejects loopback. The `mcp_client.rs` tests use
> `test_new` for that reason. Make `test_new` `pub(crate)` (it is currently
> private `#[cfg(test)]`) OR add a `#[cfg(test)] pub(crate)` re-export so this
> test can build a loopback client. Use `McpHttpClient::test_new` in the two
> tests above instead of `new`.

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p llmenv session_log::dispatch`
Expected: FAIL — undefined.

- [ ] **Step 3: Implement**

```rust
//! Issues the two transcript MCP calls through the shared `McpHttpClient`.
//! Callers run these inside a current-thread tokio runtime (see `hook_run`).

use serde_json::Value;

use crate::hook_run::mcp_client::McpHttpClient;
use crate::session_log::event::SessionLogEvent;
use crate::session_log::transcript::{record_args, start_session_args, RECORD_TOOL, START_TOOL};

/// Start a transcript session; returns its id (the tool's text result, trimmed).
///
/// # Errors
/// Any `call_tool` failure, or an empty id.
pub async fn start_session(
    client: &McpHttpClient,
    agent: &str,
    project: Option<&str>,
    metadata: &Value,
) -> anyhow::Result<String> {
    let text = client
        .call_tool(START_TOOL, start_session_args(agent, project, metadata))
        .await?;
    let id = text.trim().to_string();
    if id.is_empty() {
        anyhow::bail!("{START_TOOL} returned an empty session id");
    }
    Ok(id)
}

/// Record one event into `session_id`.
///
/// # Errors
/// Any `call_tool` failure.
pub async fn record(
    client: &McpHttpClient,
    session_id: &str,
    ev: &SessionLogEvent,
) -> anyhow::Result<()> {
    client.call_tool(RECORD_TOOL, record_args(session_id, ev)).await?;
    Ok(())
}
```

- [ ] **Step 4: Make `test_new` reachable** — in `src/hook_run/mcp_client.rs`
  change `fn test_new` to `pub(crate) fn test_new` (keep `#[cfg(test)]`).

- [ ] **Step 5: Run + commit**

Run: `cargo test -p llmenv session_log::dispatch`
Expected: PASS.

```bash
git add src/session_log/dispatch.rs src/session_log/mod.rs src/hook_run/mcp_client.rs
git commit -m "feat: transcript start/record dispatch via shared McpHttpClient"
```

---

## Phase 3 — Internal-op tracing layer + main wiring

### Task 8: `tracing` layer → file sink (process-scoped internal events)

**Files:**

- Create: `src/session_log/tracing_layer.rs`
- Modify: `src/session_log/mod.rs`
- Test: inline (capture an event through the layer into a temp file)

**Interfaces:**

- Produces: `pub struct FileLogLayer { sink: FileSink }`;
  `impl FileLogLayer { pub fn new(sink: FileSink) -> Self }`;
  `impl<S> tracing_subscriber::Layer<S> for FileLogLayer`.
- Consumes: `file_sink::FileSink`, `event::*`.

- [ ] **Step 1: Write the failing test**

```rust
// src/session_log/tracing_layer.rs
#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use tracing_subscriber::prelude::*;

    #[test]
    fn info_event_is_written_as_internal_jsonl() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        let layer = FileLogLayer::new(crate::session_log::FileSink::new(path.clone()));
        let sub = tracing_subscriber::registry().with(layer);
        tracing::subscriber::with_default(sub, || {
            tracing::info!(target: "llmenv::materialize", "materialized 3 files");
        });
        let body = std::fs::read_to_string(&path).unwrap();
        let v: serde_json::Value = serde_json::from_str(body.lines().next().unwrap()).unwrap();
        assert_eq!(v["kind"], "internal");
        assert_eq!(v["scope"], "process");
        assert_eq!(v["level"], "INFO");
        assert!(v["content"].as_str().unwrap().contains("materialized 3 files"));
    }
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p llmenv session_log::tracing_layer`
Expected: FAIL — undefined.

- [ ] **Step 3: Implement** (record the message field; emit a process-scoped
  `Internal` event)

```rust
//! A `tracing` layer that mirrors llmenv's own `info`+ events into the session
//! file sink as `kind=internal`, `scope=process` events. This preserves the
//! useful internal logging (materialization, change detection, …) the pre-3.0
//! `session_log` file carried.

use tracing::field::{Field, Visit};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::layer::Context;
use tracing_subscriber::Layer;

use crate::session_log::event::{EventKind, EventScope, SessionLogEvent};
use crate::session_log::file_sink::FileSink;

/// Writes internal `tracing` events to a session-log file sink.
pub struct FileLogLayer {
    sink: FileSink,
}

impl FileLogLayer {
    #[must_use]
    pub fn new(sink: FileSink) -> Self {
        Self { sink }
    }
}

#[derive(Default)]
struct MsgVisitor {
    message: String,
}

impl Visit for MsgVisitor {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            self.message = format!("{value:?}");
        }
    }
}

impl<S: Subscriber> Layer<S> for FileLogLayer {
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let meta = event.metadata();
        // Only mirror info+ to avoid debug/trace noise in the session log.
        if *meta.level() > Level::INFO {
            return;
        }
        let mut v = MsgVisitor::default();
        event.record(&mut v);
        let ev = SessionLogEvent {
            ts: now_rfc3339(),
            kind: EventKind::Internal,
            scope: EventScope::Process,
            role: "system".into(),
            tool_name: None,
            tokens: None,
            level: Some(meta.level().to_string()),
            content: v.message.trim_matches('"').to_string(),
            fields: serde_json::json!({ "target": meta.target() }),
        };
        self.sink.append(&ev.to_jsonl());
    }
}

/// RFC 3339 timestamp. Uses `time` (already a transitive dep via tracing) or
/// `chrono` if the crate already depends on it; otherwise add the `time` dep.
fn now_rfc3339() -> String {
    // Implementation note: use the same timestamp source the rest of the crate
    // uses for log lines. If none exists, use
    // `time::OffsetDateTime::now_utc().format(&time::format_description::well_known::Rfc3339)`.
    time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default()
}
```

> Dependency check before coding: run `cargo tree -p llmenv | rg '^.*(time|chrono)'`.
> If `time` is not already available with the `formatting` feature, add it to
> `Cargo.toml` (`time = { version = "<current>", features = ["formatting"] }`),
> then **regenerate `THIRD-PARTY-LICENSES.md` and the website copy via
> `scripts/gen-attribution.sh`** and commit them in this task (AGENTS.md
> licensing rule). Prefer an already-present crate to avoid the new dep.

- [ ] **Step 4: Run + commit**

Run: `cargo test -p llmenv session_log::tracing_layer`
Expected: PASS.

```bash
git add src/session_log/tracing_layer.rs src/session_log/mod.rs Cargo.toml Cargo.lock \
        THIRD-PARTY-LICENSES.md website/docs/third-party-licenses.md 2>/dev/null
git commit -m "feat: mirror internal tracing events into the session-log file"
```

---

### Task 9: Wire `main.rs` (install sinks from resolved config)

**Files:**

- Modify: `src/main.rs:1-50`
- Test: none (integration wiring; covered by manual smoke + later e2e)

**Interfaces:**

- Consumes: `Config::session_log_resolved`, `session_log::{FileSink,
  default_file_path_string, tracing_layer::FileLogLayer}`.

- [ ] **Step 1: Replace the session-log wiring in `fn main`**

```rust
// src/main.rs — replace lines 1-45 region
use tracing_subscriber::{EnvFilter, prelude::*};

fn main() {
    let resolved = llmenv_paths::config_path()
        .ok()
        .and_then(|p| llmenv_config::Config::load(&p).ok())
        .map(|c| c.session_log_resolved())
        .unwrap_or_default();

    // File sink layer: only when `file: true`. Internal `tracing` events are
    // mirrored here as session-log `internal` events. Independent of ICM.
    let file_layer = resolved.file.then(|| {
        let raw = resolved.path.clone()
            .unwrap_or_else(llmenv::session_log::default_file_path_string);
        let path = llmenv_paths::expand_tilde(&raw);
        llmenv::session_log::tracing_layer::FileLogLayer::new(
            llmenv::session_log::FileSink::new(std::path::PathBuf::from(path)),
        )
    });

    tracing_subscriber::registry()
        .with(
            tracing_subscriber::fmt::layer()
                .with_writer(std::io::stderr)
                .with_filter(EnvFilter::from_default_env()),
        )
        .with(file_layer)
        .init();
    // ... rest of main unchanged ...
}
```

Delete the now-unused `open_session_log` fn and the `OpenOptions`/`BufWriter`/
`Mutex` imports introduced for it.

- [ ] **Step 2: Build + clippy**

Run: `cargo build && cargo clippy --all-targets --all-features -- -D warnings`
Expected: PASS, zero warnings.

- [ ] **Step 3: Smoke test the file sink**

```bash
LLMENV_STATE_DIR=$(mktemp -d) bash -c '
  printf "session_log:\n  file: true\n" > "$LLMENV_STATE_DIR/config.yaml"
  LLMENV_CONFIG_PATH="$LLMENV_STATE_DIR/config.yaml" cargo run -q -- status >/dev/null 2>&1
  test -f "$LLMENV_STATE_DIR/session-log.jsonl" && echo "FILE SINK OK" || echo "no file (ok if no info events fired)"
'
```

Expected: command exits 0 (file may or may not exist depending on whether an
`info` event fired — the assertion is "no crash").

- [ ] **Step 4: Commit**

```bash
git add src/main.rs
git commit -m "feat: wire session-log file sink from resolved config"
```

---

## Phase 4 — Hook integration (baseline + verbose)

### Task 10: Baseline — emit lifecycle + scope-header in hooks

**Files:**

- Modify: `src/hook_run/mod.rs` (`run_inner` / `dispatch` region, around lines
  220-280) — add session-log emission alongside the existing memory actions.
- Test: inline test asserting a session-log file line is produced on
  `session_start` when `file: true`.

**Interfaces:**

- Consumes: `session_log::{FileSink, scope_header::*, event::*, state, dispatch,
  default_file_path}`, `McpHttpClient`, the active tags/bundles `run_inner`
  already computes.
- Produces: a private `fn emit_session_log(event: SessionLogEvent, cfg:
  &SessionLog, client: Option<&McpHttpClient>, session_id: Option<&str>)` that
  appends to the file sink (if `file`) and records to transcript (if `transcript`
  and a session is available).

- [ ] **Step 1: Write the failing test** (drive `run` for `session_start` with
  `file: true`, assert a `lifecycle_start` line appears)

```rust
// src/hook_run/mod.rs tests
#[test]
fn session_start_writes_lifecycle_and_scope_to_file() {
    let dir = tempfile::tempdir().unwrap();
    unsafe { std::env::set_var("LLMENV_STATE_DIR", dir.path()) };
    // Minimal config enabling the file sink, no transcript (offline test).
    // (Use the crate's existing test harness for building a Config + active
    // scopes; assert two JSONL lines: kind=lifecycle_start and kind=scope.)
    // See run_inner for how active scopes are derived.
}
```

> The exact harness mirrors existing `hook_run` tests; the implementer fills the
> Config/active-scope construction from the patterns already in
> `src/hook_run/mod.rs` `#[cfg(test)]`. Assertion: read
> `<state_dir>/session-log.jsonl`, expect a line with `"kind":"lifecycle_start"`
> and a line with `"kind":"scope"` containing an `llmenv-tag:` token for an
> active tag.

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p llmenv hook_run::tests::session_start_writes`
Expected: FAIL.

- [ ] **Step 3: Implement** — in `run_inner`, after computing `tags`/`bundles`,
  build a `ScopeContext`, and for `HookEvent::SessionStart`:
  1. read Claude `session_id` from the parsed stdin JSON (the `hook_event_name`
     parse already exists; extend it to also pull `session_id`);
  2. if `cfg.transcript`: `start_session` via the client, `record_session` the
     mapping;
  3. emit `lifecycle_start` then the scope-header `Scope` event through
     `emit_session_log`.
  For `HookEvent::SessionEnd`: emit `lifecycle_end`. Gate all of it on
  `cfg.file || cfg.transcript`. Use `EventScope::AgentSession`.

```rust
// sketch of emit_session_log (full impl ≤100 lines, fail-soft)
fn emit_session_log(
    ev: SessionLogEvent,
    cfg: &SessionLog,
    client: Option<&McpHttpClient>,
    session_id: Option<&str>,
) {
    let max = cfg.max_content_bytes.unwrap_or(16_384);
    let ev = ev.truncated(max);
    if cfg.file {
        let path = cfg.path.clone()
            .map(std::path::PathBuf::from)
            .or_else(|| crate::session_log::default_file_path().ok());
        if let Some(p) = path {
            crate::session_log::FileSink::new(p).append(&ev.to_jsonl());
        }
    }
    if cfg.transcript && ev.scope == EventScope::AgentSession {
        if let (Some(c), Some(sid)) = (client, session_id) {
            // best-effort; see Task 11 for the detached-dispatch refinement.
            let _ = futures::executor::block_on(crate::session_log::dispatch::record(c, sid, &ev));
        }
    }
}
```

> The synchronous `block_on` here is a placeholder; Task 11 moves transcript
> dispatch to a detached child so the hook returns instantly. Keep the file sink
> synchronous.

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test -p llmenv hook_run::tests::session_start_writes`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/hook_run/mod.rs
git commit -m "feat: emit lifecycle + scope-header session events from hooks"
```

---

### Task 11: Detached transcript dispatch (`llmenv session-log` subcommand)

**Files:**

- Create: `src/session_log/detached.rs` (spawn helper)
- Modify: `src/cli/mod.rs` (register `session-log` subcommand)
- Modify: `src/hook_run/mod.rs` (`emit_session_log` transcript branch → spawn
  detached instead of `block_on`)
- Test: inline test for the spawn-args builder (pure), not the fork itself.

**Interfaces:**

- Produces:
  `pub fn spawn_record(session_id: &str, ev: &SessionLogEvent)` — serialize `ev`
  to a temp/stdin payload and `setsid`-detach `llmenv session-log record
  --session <id>` reading the event JSON on stdin; returns immediately.
  `pub fn run_record(session_id: &str, event_json: &str) -> anyhow::Result<()>` —
  the child entrypoint: resolve MCP url, build client, `block_on(record(...))`.
- Consumes: `dispatch::record`, `memory_url` (reuse from `hook_run`).

- [ ] **Step 1: Write the failing test** (the detach is process-level; test the
  child entrypoint against wiremock, and that `spawn_record` builds a valid
  command without panicking)

```rust
// src/session_log/detached.rs
#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    // run_record is exercised via wiremock similar to dispatch tests; assert it
    // returns Ok when the server responds 200, Err on connection refused.
}
```

- [ ] **Step 2..4:** implement `spawn_record` using the ICM `cmd_hook_end`
  detach template (`std::process::Command` + `pre_exec(libc::setsid)` on unix,
  stdio null), and `run_record` resolving the MCP url like
  `hook_run::run_inner` does (`memory_url(&config, config_dir, &active)`), then
  building a current-thread runtime and `block_on(dispatch::record(...))`. Wire
  `Commands::SessionLog { ... }` in `cli/mod.rs` to `run_record`. Replace the
  `block_on` in `emit_session_log` (Task 10) with `spawn_record`. For
  `start_session` (which must return an id to persist), keep it inline in the
  SessionStart hook but with the existing short `HOOK_TIMEOUT`; only the
  per-event records are detached.

- [ ] **Step 5: Run full hook tests + commit**

Run: `cargo test -p llmenv session_log:: && cargo test -p llmenv hook_run::`
Expected: PASS.

```bash
git add src/session_log/detached.rs src/cli/mod.rs src/hook_run/mod.rs
git commit -m "feat: dispatch transcript records via detached child so hooks return fast"
```

---

### Task 12: Verbose `HookEvent` variants + record mapping

**Files:**

- Modify: `src/hook_run/mod.rs` (`HookEvent` enum lines 122-155, `FromStr`,
  `Display`, `dispatch`)
- Test: extend the existing `parses_neutral_event_names` / `rejects_unknown_event`
  tests + a new event→kind mapping test.

**Interfaces:**

- Produces: `HookEvent::{PreToolUse, PostToolUse, Notification, Stop,
  SubagentStop, PreCompact}`; `fn event_to_log_kind(HookEvent) -> Option<(EventKind, &'static str)>`
  (kind + role; `None` for the memory-only events that don't log a turn).

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn parses_verbose_event_names() {
    assert_eq!(HookEvent::from_str("pre_tool_use").unwrap(), HookEvent::PreToolUse);
    assert_eq!(HookEvent::from_str("post_tool_use").unwrap(), HookEvent::PostToolUse);
    assert_eq!(HookEvent::from_str("stop").unwrap(), HookEvent::Stop);
}

#[test]
fn verbose_events_map_to_log_kinds() {
    use crate::session_log::event::EventKind;
    assert_eq!(event_to_log_kind(HookEvent::PreToolUse).unwrap().0, EventKind::ToolUse);
    assert_eq!(event_to_log_kind(HookEvent::PostToolUse).unwrap().0, EventKind::ToolResult);
    assert_eq!(event_to_log_kind(HookEvent::UserPromptSubmit).unwrap().0, EventKind::Prompt);
}
```

- [ ] **Step 2: Run to verify they fail.** Run: `cargo test -p llmenv hook_run`.
  Expected: FAIL.

- [ ] **Step 3: Implement** — add the variants to the enum, `FromStr`
  (snake_case names matching Claude events), `Display`, and `event_to_log_kind`
  (UserPromptSubmit→Prompt/"user", PreToolUse→ToolUse/"tool",
  PostToolUse→ToolResult/"tool", Notification→Notification/"system",
  Stop/SubagentStop→Stop/"assistant", PreCompact→Notification/"system";
  SessionStart/SessionEnd/TurnStart return None for turn-kind since they are
  handled as lifecycle/memory). In `run`, when `cfg.verbose` and the parsed event
  maps to a kind, build a `SessionLogEvent` from the hook stdin payload (content
  = prompt/tool input/result extracted from the JSON; `tool_name` from payload)
  and `emit_session_log` it (looking up the session id via
  `state::lookup_session`, lazily starting one if absent).

- [ ] **Step 4: Run + commit**

Run: `cargo test -p llmenv hook_run`
Expected: PASS.

```bash
git add src/hook_run/mod.rs
git commit -m "feat: capture prompts and tool use as verbose session events"
```

---

### Task 13: Adapter hook injection (baseline always, verbose gated)

**Files:**

- Modify: `src/adapter/claude_code.rs` (hook emission; `STALE_CHECK_COMMAND`
  region + the `build_settings`/`hooks_by_event` generation)
- Test: inline test asserting generated settings.json contains the baseline
  hooks when enabled and the verbose hooks only when `verbose: true`.

**Interfaces:**

- Consumes: `Config::session_log_resolved` (the adapter receives the resolved
  config / manifest; thread the `SessionLog` into the settings builder).
- Produces: auto-emitted hooks calling `llmenv hook-run <event>` for
  SessionStart + SessionEnd (baseline) and the verbose events when `verbose`.

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn baseline_injects_sessionstart_sessionend_only() {
    // Build settings with a SessionLog { transcript: true, verbose: false, .. }.
    // Assert generated hooks include SessionStart + SessionEnd handlers that run
    // `llmenv hook-run session_start` / `session_end`, and DO NOT include
    // PreToolUse/PostToolUse session-log handlers.
}

#[test]
fn verbose_injects_all_turn_hooks() {
    // With verbose: true, assert PreToolUse, PostToolUse, UserPromptSubmit,
    // Stop, Notification handlers are present.
}
```

- [ ] **Step 2: Run to verify it fails.** Run: `cargo test -p llmenv claude_code`.
  Expected: FAIL.

- [ ] **Step 3: Implement** — add a constant per new hook command (e.g.
  `const SESSION_LOG_HOOK: &str = "llmenv hook-run";` invoked with the event
  arg), and in the settings hook builder append: SessionStart+SessionEnd when
  `transcript || file`; the verbose set additionally when `verbose`. Follow the
  exact JSON shape the existing auto-hooks use (matcher/hooks/handler). Do not
  duplicate a hook event that already exists for memory — append a second handler
  to the same event array (Claude runs all handlers for an event).

- [ ] **Step 4: Run + commit**

Run: `cargo test -p llmenv claude_code`
Expected: PASS.

```bash
git add src/adapter/claude_code.rs
git commit -m "feat: inject baseline + verbose session-log hooks into Claude Code"
```

---

## Phase 5 — Examples, docs, changelog

### Task 14: Example config block

**Files:**

- Modify: `examples/config-llmenv-dir/config.yaml`

- [ ] **Step 1: Add a documented `session_log:` block** in the file's house
  style (heavy explanatory comments), e.g. after the `cache:` block:

```yaml
################################################################################
# Session logging.
#
# llmenv records session activity into a single event stream that fans out to
# two independent sinks: a local JSONL file and ICM's transcript store (via the
# ICM MCP — works even when this host is not the primary ICM host).
#
#   transcript: true   ICM transcript (default ON). Discoverable later by tag
#                      via `icm_transcript_search "llmenv-tag:<tag>"`.
#   file: false        Mirror the same stream to a local JSONL file.
#   verbose: false     Also capture per-hook prompts and tool use, not just the
#                      scope header + lifecycle.
#
# Omitting this block entirely is equivalent to `transcript: true` (ICM only).
# This is NOT the same thing as the `bundles/base/hooks/session-log.sh` example
# hook, which is a user-authored bundle hook unrelated to this built-in feature.
################################################################################
session_log:
  transcript: true
  file: false
  verbose: false
```

- [ ] **Step 2: Sanity-check it parses**

Run: `LLMENV_CONFIG_DIR=examples/config-llmenv-dir cargo run -q -- doctor 2>&1 | tail -5`
Expected: no config parse error about `session_log`.

- [ ] **Step 3: Commit**

```bash
git add examples/config-llmenv-dir/config.yaml
git commit -m "docs: show session_log settings in the example config"
```

---

### Task 15: User docs

**Files:**

- Create/modify: a docs page under `docs/` (and `website/` if the site mirrors
  docs — check `website/docs/` for the existing structure and add a matching
  page).

- [ ] **Step 1: Write the page** covering: the three flags + default; the two
  sinks and independence; that ICM is reached via MCP (multi-host); the four
  discoverability handles with concrete `icm_transcript_search` recipes
  (`"llmenv-tag:rust"`, project filter); verbose's privacy/size note
  (`max_content_bytes`); and that disabling is `transcript: false`.

- [ ] **Step 2: Commit**

```bash
git add docs/ website/ 2>/dev/null
git commit -m "docs: document ICM-transcript session logging and query recipes"
```

---

### Task 16: CHANGELOG (Unreleased) + final verification

**Files:**

- Modify: `CHANGELOG.md` (under `## [Unreleased]`)

- [ ] **Step 1: Add entries** under `## [Unreleased]` (do NOT create a version
  heading — RELEASING.md):

```markdown
### Added
- ICM-transcript session logging: llmenv records scope + lifecycle (and, with
  `session_log.verbose`, prompts and tool use) into ICM's transcript store via
  the ICM MCP, discoverable by `llmenv-tag:` / `llmenv-bundle:` tokens and
  project. A local JSONL `file` sink mirrors the same stream. (#382)

### Changed
- **BREAKING:** `session_log` is now a mapping (`{ file, transcript, verbose,
  path, max_content_bytes }`), not a path string. ICM transcript logging is on
  by default. The pre-3.0 `session_log: "<path>"` form is rejected with a
  migration hint. (#382)
```

Then reconcile against the older release line per AGENTS.md (run
`git log --no-merges <last-tag>..HEAD` and check the older branch's CHANGELOG for
any forward-merged fix needing a back-reference — add if found).

- [ ] **Step 2: Full verification**

Run:

```bash
cargo fmt --check && \
cargo clippy --all-targets --all-features -- -D warnings && \
cargo test && \
cargo deny check 2>/dev/null || echo "cargo deny: review if deps changed"
```

Expected: fmt clean, zero clippy warnings, all tests pass. If a dependency was
added in Task 8, confirm attribution files were regenerated.

- [ ] **Step 3: Commit**

```bash
git add CHANGELOG.md
git commit -m "docs: changelog entry for ICM-transcript session logging (#382)"
```

---

## Self-Review (completed)

- **Spec coverage:** config table + default-on (T1, T14), event model (T2),
  file sink + internal-op preservation (T3, T8), scope-header FTS tokens (T4),
  transcript via MCP not CLI (T5, T7, T11), background dispatch (T11), state
  correlation (T6), baseline lifecycle hooks (T10, T13), verbose all-hooks
  capture (T12, T13), independence/degradation (T3/T10 fail-soft + T10 file-on
  test), discoverability recipes (T15), examples (T14), milestone/branch +
  breaking changelog (Global Constraints, T16). All spec sections map to tasks.
- **Type consistency:** `SessionLog`, `SessionLogEvent`, `EventKind`,
  `EventScope`, `FileSink`, `ScopeContext`, `start_session`/`record`,
  `lookup_session`/`record_session`, `event_to_log_kind`, `emit_session_log`,
  `spawn_record`/`run_record` are used with identical signatures across tasks.
- **Open implementation notes flagged inline:** the `time`/`chrono` timestamp
  source (T8) and the `test_new` visibility bump (T7) are called out where they
  occur. No placeholders remain.
