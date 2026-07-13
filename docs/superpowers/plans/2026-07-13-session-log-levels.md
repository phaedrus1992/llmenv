# Session Log Levels Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace `session_log.verbose: bool` with per-sink `level` (info/debug/trace), so lifecycle events and user prompts always record while tool details and hook internals are opt-in per sink.

**Architecture:** A new `LogLevel` enum drives event filtering at each sink. The config schema gains `FileSinkConfig`/`TranscriptSinkConfig` structs with `enabled` + `level` fields. A backward-compatible deserializer translates `verbose: true` → both sinks at debug, `file: true` → file enabled at info. `SessionLogEvent` gains a `log_level()` method and `trace_fields` for hook diagnostics. Hook registration switches from `verbose` gating to "any sink enabled" gating; per-event level filtering happens in `run_session_log` and `emit_session_log`.

**Tech Stack:** Rust, serde, serde_yaml, tracing (unchanged)

## Global Constraints

- Backward compatible: old `{file: bool, transcript: bool, verbose: bool}` shape deserializes correctly
- Default unchanged: transcript on at info, file off
- `tracing` subscriber and `RUST_LOG`/`EnvFilter` are not changed
- No error/warn levels for session-log events
- File sink wires in `main.rs` using resolved config — same path as today
- Detached record child must pass `trace_fields` through stdin JSON

## File Map

| File | Role |
|------|------|
| `crates/llmenv-config/src/schema.rs` | `SessionLog`, `FileSinkConfig`, `TranscriptSinkConfig`, `LogLevel`, custom deserializer |
| `crates/llmenv-config/src/lib.rs` | Re-exports, `session_log_resolved()`, config tests |
| `crates/llmenv-config/src/validate.rs` | `arb_session_log()` proptest strategy |
| `src/session_log/event.rs` | `SessionLogEvent::log_level()`, `trace_fields` field |
| `src/hook_run/mod.rs` | Level filtering in `run_session_log` / `emit_session_log`, remove verbose gating, update `event_to_log_kind` |
| `src/adapter/claude_code.rs` | Hook registration: always register turn hooks when any sink enabled |
| `src/main.rs` | File sink wiring reads new config shape |
| `src/merge/mod.rs` | `MergedManifest.session_log` (unchanged type, pins through) |

---

### Task 1: Config types — LogLevel, FileSinkConfig, TranscriptSinkConfig, new SessionLog

**Files:**
- Modify: `crates/llmenv-config/src/schema.rs:163-240`
- Modify: `crates/llmenv-config/src/lib.rs:1-10` (re-exports)
- Modify: `crates/llmenv-config/src/validate.rs:735-753`

**Interfaces:**
- Produces: `LogLevel` enum (Trace, Debug, Info), `FileSinkConfig` struct, `TranscriptSinkConfig` struct, updated `SessionLog` struct
- Produces: backward-compatible `Deserialize` for `SessionLog` — old `verbose: bool` → per-sink levels

**Design notes:**
- `LogLevel` derives `Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize`
- Serialize: lowercase strings (`"trace"`, `"debug"`, `"info"`)
- `FileSinkConfig { enabled: bool, level: LogLevel, path: Option<String> }`
- `TranscriptSinkConfig { enabled: bool, level: LogLevel }`
- `SessionLog { file: Option<FileSinkConfig>, transcript: Option<TranscriptSinkConfig>, max_content_bytes: Option<usize> }`
- Custom deserializer: detect old shape by checking for `verbose`/`file`/`transcript` as bools in the mapping; if found, translate; otherwise parse new shape
- `Default` for `SessionLog`: `file: None, transcript: Some(TranscriptSinkConfig { enabled: true, level: Info }), max_content_bytes: None`
- `Default` for `FileSinkConfig`: `enabled: false, level: Info, path: None`
- `Default` for `TranscriptSinkConfig`: `enabled: true, level: Info`

- [ ] **Step 1: Add LogLevel enum at top of schema.rs (before SessionLog)**

```rust
/// Per-sink log level for session-log events. Each level includes all events
/// from the levels above it: Trace ⊃ Debug ⊃ Info.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum LogLevel {
    /// Lifecycle events, user prompts, stop, notifications, precompact.
    Info,
    /// Info + tool uses, tool results.
    Debug,
    /// Debug + hook stdout/stderr, hook exit codes.
    Trace,
}

impl Default for LogLevel {
    fn default() -> Self {
        Self::Info
    }
}

impl<'de> Deserialize<'de> for LogLevel {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        match s.as_str() {
            "info" => Ok(Self::Info),
            "debug" => Ok(Self::Debug),
            "trace" => Ok(Self::Trace),
            other => Err(serde::de::Error::custom(format!(
                "unknown log level {other:?}, expected info | debug | trace"
            ))),
        }
    }
}
```

- [ ] **Step 2: Add FileSinkConfig and TranscriptSinkConfig structs below LogLevel**

```rust
/// Configuration for the file sink within `session_log`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileSinkConfig {
    /// Enable the JSONL file sink.
    #[serde(default)]
    pub enabled: bool,
    /// Minimum event level recorded to this file.
    #[serde(default)]
    pub level: LogLevel,
    /// Override the default path (`<state_dir>/session-log.jsonl`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
}

impl Default for FileSinkConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            level: LogLevel::Info,
            path: None,
        }
    }
}

/// Configuration for the ICM transcript sink within `session_log`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TranscriptSinkConfig {
    /// Enable the ICM transcript sink.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Minimum event level recorded to the transcript.
    #[serde(default)]
    pub level: LogLevel,
}

impl Default for TranscriptSinkConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            level: LogLevel::Info,
        }
    }
}
```

- [ ] **Step 3: Replace SessionLog struct and associated impls**

Replace the existing `SessionLog` struct (lines 163-178), `Default` impl (lines 180-189), and `Deserialize` impl (lines 194-236) with:

```rust
/// Where and how llmenv records session activity. `file` and `transcript` are
/// independent sinks that share the same event stream; each filters by its own
/// `level`. `max_content_bytes` applies uniformly to both.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SessionLog {
    /// JSONL file sink config. Absent → disabled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<FileSinkConfig>,
    /// ICM transcript sink config. Absent → default (enabled at info).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transcript: Option<TranscriptSinkConfig>,
    /// Truncate event content to this many bytes (default 16384).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_content_bytes: Option<usize>,
}

impl Default for SessionLog {
    fn default() -> Self {
        Self {
            file: None,
            transcript: Some(TranscriptSinkConfig::default()),
            max_content_bytes: None,
        }
    }
}

impl SessionLog {
    /// Whether any sink is enabled at any level (cheap gate for hook early-exit).
    #[must_use]
    pub fn any_sink_enabled(&self) -> bool {
        self.file.as_ref().is_some_and(|f| f.enabled)
            || self.transcript.as_ref().is_some_and(|t| t.enabled)
    }

    /// Whether any sink is enabled at or above `level`.
    #[must_use]
    pub fn any_sink_wants(&self, level: LogLevel) -> bool {
        self.file.as_ref().is_some_and(|f| f.enabled && f.level <= level)
            || self.transcript.as_ref().is_some_and(|t| t.enabled && t.level <= level)
    }

    /// Whether the file sink is enabled and accepts events at `level`.
    #[must_use]
    pub fn file_wants(&self, level: LogLevel) -> bool {
        self.file.as_ref().is_some_and(|f| f.enabled && f.level <= level)
    }

    /// Whether the transcript sink is enabled and accepts events at `level`.
    #[must_use]
    pub fn transcript_wants(&self, level: LogLevel) -> bool {
        self.transcript.as_ref().is_some_and(|t| t.enabled && t.level <= level)
    }

    /// File sink path override, if any.
    #[must_use]
    pub fn file_path(&self) -> Option<&str> {
        self.file.as_ref().and_then(|f| f.path.as_deref())
    }
}

/// Deserialize: accept the new shape (file/transcript sections) or translate the
/// old boolean-only shape (`{file, transcript, verbose}` as bools). Also rejects
/// the pre-3.0 bare-string form with a migration message.
impl<'de> serde::Deserialize<'de> for SessionLog {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let v = serde_yaml::Value::deserialize(d)?;
        if !v.is_mapping() {
            let got = match &v {
                serde_yaml::Value::String(_) => {
                    "a string (the pre-3.0 bare path-string form is no longer supported)"
                        .to_string()
                }
                serde_yaml::Value::Bool(_) => "a boolean".to_string(),
                serde_yaml::Value::Number(_) => "a number".to_string(),
                serde_yaml::Value::Sequence(_) => "a sequence".to_string(),
                other => format!("{other:?}"),
            };
            return Err(serde::de::Error::custom(format!(
                "session_log must be a mapping, not {got}"
            )));
        }
        let m = v.as_mapping().unwrap();

        // Detect old boolean shape: any of `file`, `transcript`, or `verbose` is
        // a bool → translate the legacy format.
        let is_old_shape = m.iter().any(|(k, v)| {
            let key = k.as_str().unwrap_or("");
            (key == "file" || key == "transcript" || key == "verbose") && v.is_bool()
        });

        if is_old_shape {
            #[derive(Deserialize)]
            struct OldShape {
                #[serde(default)]
                file: bool,
                #[serde(default = "default_true")]
                transcript: bool,
                #[serde(default)]
                verbose: bool,
                #[serde(default)]
                path: Option<String>,
                #[serde(default)]
                max_content_bytes: Option<usize>,
            }
            let old: OldShape = serde_yaml::from_value(v)
                .map_err(serde::de::Error::custom)?;
            let level = if old.verbose { LogLevel::Debug } else { LogLevel::Info };
            return Ok(SessionLog {
                file: old.file.then(|| FileSinkConfig {
                    enabled: true,
                    level,
                    path: old.path,
                }),
                transcript: if old.transcript {
                    Some(TranscriptSinkConfig { enabled: true, level })
                } else {
                    Some(TranscriptSinkConfig { enabled: false, level })
                },
                max_content_bytes: old.max_content_bytes,
            });
        }

        // New shape.
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct NewShape {
            #[serde(default)]
            file: Option<FileSinkConfig>,
            #[serde(default)]
            transcript: Option<TranscriptSinkConfig>,
            #[serde(default)]
            max_content_bytes: Option<usize>,
        }
        let n: NewShape = serde_yaml::from_value(v)
            .map_err(serde::de::Error::custom)?;
        Ok(SessionLog {
            file: n.file,
            transcript: n.transcript,
            max_content_bytes: n.max_content_bytes,
        })
    }
}
```

- [ ] **Step 4: Build — verify new types compile**

Run: `cargo build -p llmenv-config 2>&1`
Expected: compiles clean (tests may fail — old `SessionLog` field refs)

- [ ] **Step 5: Update re-exports in lib.rs**

In `crates/llmenv-config/src/lib.rs`, add to the existing `pub use` block:

```rust
// Add LogLevel, FileSinkConfig, TranscriptSinkConfig alongside the existing
// SessionLog re-export (line 33).
```

Update line 33 from:
```rust
RESERVED_OFFICIAL_MARKETPLACES, ReadOnce, ReadOnceMode, Scopes, SessionLog, S...
```
to include `LogLevel, FileSinkConfig, TranscriptSinkConfig`.

- [ ] **Step 6: Update config-level tests in lib.rs**

Read the existing tests at `crates/llmenv-config/src/lib.rs:115-155`. Update:

`session_log_absent_resolves_to_transcript_on` (line 115):
```rust
#[test]
fn session_log_absent_resolves_to_transcript_on() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path().join("config.yaml");
    std::fs::write(&p, "cache: {}\n").unwrap();
    let cfg = Config::load(&p).unwrap();
    assert!(cfg.session_log.is_none());
    let resolved = cfg.session_log_resolved();
    assert!(!resolved.any_sink_wants(LogLevel::Debug));
    let t = resolved.transcript.as_ref().unwrap();
    assert!(t.enabled);
    assert_eq!(t.level, LogLevel::Info);
}
```

`session_log_table_parses_flags` (line 128):
```rust
#[test]
fn session_log_old_shape_translates_to_new() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path().join("config.yaml");
    std::fs::write(
        &p,
        "session_log:\n  file: true\n  transcript: false\n  verbose: true\n",
    )
    .unwrap();
    let r = Config::load(&p).unwrap().session_log_resolved();
    let f = r.file.as_ref().unwrap();
    assert!(f.enabled);
    assert_eq!(f.level, LogLevel::Debug);
    let t = r.transcript.as_ref().unwrap();
    assert!(!t.enabled);
    assert_eq!(t.level, LogLevel::Debug);
}

#[test]
fn session_log_new_shape_parses() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path().join("config.yaml");
    std::fs::write(
        &p,
        "session_log:\n  file:\n    enabled: true\n    level: trace\n  transcript:\n    enabled: true\n    level: info\n",
    )
    .unwrap();
    let r = Config::load(&p).unwrap().session_log_resolved();
    let f = r.file.as_ref().unwrap();
    assert!(f.enabled);
    assert_eq!(f.level, LogLevel::Trace);
    let t = r.transcript.as_ref().unwrap();
    assert!(t.enabled);
    assert_eq!(t.level, LogLevel::Info);
}
```

Keep `session_log_bare_string_is_rejected_with_migration_hint` (line 142) unchanged.

- [ ] **Step 7: Run config tests**

Run: `cargo test -p llmenv-config 2>&1`
Expected: PASS (all config tests)

- [ ] **Step 8: Update arb_session_log in validate.rs**

Replace the arb_session_log function at lines 735-753:

```rust
fn arb_session_log() -> impl Strategy<Value = crate::SessionLog> {
    use crate::LogLevel;
    fn arb_level() -> impl Strategy<Value = LogLevel> {
        prop_oneof![
            Just(LogLevel::Info),
            Just(LogLevel::Debug),
            Just(LogLevel::Trace),
        ]
    }
    (
        prop::option::of((any::<bool>(), arb_level(), arb_opt_string())),
        prop::option::of((any::<bool>(), arb_level())),
        prop::option::of(0usize..65_536),
    )
        .prop_map(|(file, transcript, max_content_bytes)| {
            crate::SessionLog {
                file: file.map(|(enabled, level, path)| crate::FileSinkConfig {
                    enabled,
                    level,
                    path,
                }),
                transcript: transcript.map(|(enabled, level)| crate::TranscriptSinkConfig {
                    enabled,
                    level,
                }),
                max_content_bytes,
            }
        })
}
```

- [ ] **Step 9: Run proptest round-trip**

Run: `cargo test -p llmenv-config config_roundtrips -- --nocapture 2>&1 | head -30`
Expected: PASS

- [ ] **Step 10: Commit**

```bash
git add crates/llmenv-config/src/schema.rs crates/llmenv-config/src/lib.rs crates/llmenv-config/src/validate.rs
git commit -m "feat: add LogLevel, per-sink configs, backward-compat deserializer"
```

---

### Task 2: SessionLogEvent — add log_level() and trace_fields

**Files:**
- Modify: `src/session_log/event.rs`

**Interfaces:**
- Produces: `SessionLogEvent::log_level() -> LogLevel`
- Produces: `SessionLogEvent.trace_fields: Option<Value>` (serialized, new field)

- [ ] **Step 1: Add LogLevel import and log_level method to EventKind**

Add to `src/session_log/event.rs`:

```rust
use llmenv_config::LogLevel;
```

Add method on `EventKind`:

```rust
impl EventKind {
    /// Minimum log level at which events of this kind are recorded.
    #[must_use]
    pub fn log_level(self) -> LogLevel {
        match self {
            EventKind::LifecycleStart
            | EventKind::LifecycleEnd
            | EventKind::Scope
            | EventKind::Prompt
            | EventKind::Notification
            | EventKind::Stop => LogLevel::Info,
            EventKind::ToolUse | EventKind::ToolResult => LogLevel::Debug,
            EventKind::Internal => LogLevel::Info,
        }
    }
}
```

- [ ] **Step 2: Add log_level and trace_fields to SessionLogEvent**

Add method:

```rust
impl SessionLogEvent {
    /// Minimum level this event should be recorded at.
    #[must_use]
    pub fn log_level(&self) -> LogLevel {
        self.kind.log_level()
    }
    // ... existing methods (to_jsonl, truncated) remain
}
```

Add field to the struct (after `fields`):

```rust
    /// Hook diagnostics for Trace-level events: stdout, stderr, exit code.
    /// Only populated when the event is at Trace level.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace_fields: Option<serde_json::Value>,
```

- [ ] **Step 3: Update all existing SessionLogEvent constructors in the codebase to set trace_fields: None**

These are currently:
- `tracing_layer.rs:49-53` — add `trace_fields: None,`
- `hook_run/mod.rs:796-806` (agent_session_event) — add `trace_fields: None,`
- `hook_run/mod.rs:809-816` (lifecycle_session_event) — goes through agent_session_event, already covered
- `hook_run/mod.rs:819-827` (scope_session_event) — already covered
- `hook_run/mod.rs:831-838` (verbose_session_event) — add `trace_fields: None,`
- All test constructors in event.rs, detached.rs, dispatch.rs, hook_run/mod.rs

Run: `cargo build 2>&1 | head -40`
Expected: compile errors on all the `trace_fields` missing — fix them one by one.

- [ ] **Step 4: Fix all compile errors, verify build**

After fixing all `trace_fields: None` additions, run:
`cargo build 2>&1`
Expected: compiles clean

- [ ] **Step 5: Write unit tests for log_level mapping**

Add to the tests module in `src/session_log/event.rs`:

```rust
#[test]
fn log_level_info_for_lifecycle_events() {
    assert_eq!(EventKind::LifecycleStart.log_level(), LogLevel::Info);
    assert_eq!(EventKind::LifecycleEnd.log_level(), LogLevel::Info);
    assert_eq!(EventKind::Scope.log_level(), LogLevel::Info);
    assert_eq!(EventKind::Prompt.log_level(), LogLevel::Info);
    assert_eq!(EventKind::Notification.log_level(), LogLevel::Info);
    assert_eq!(EventKind::Stop.log_level(), LogLevel::Info);
}

#[test]
fn log_level_debug_for_tool_events() {
    assert_eq!(EventKind::ToolUse.log_level(), LogLevel::Debug);
    assert_eq!(EventKind::ToolResult.log_level(), LogLevel::Debug);
}

#[test]
fn log_level_ordering_is_correct() {
    assert!(LogLevel::Trace > LogLevel::Debug);
    assert!(LogLevel::Debug > LogLevel::Info);
}
```

Run: `cargo test -p llmenv -- session_log::event 2>&1`
Expected: PASS

- [ ] **Step 6: Update proptest for new field**

In the `arb_event` function in the test module, add `trace_fields` to the prop_map closure:

```rust
// After the existing prop_map body, add:
trace_fields: None,
```

And in the proptest round-trip test, verify it survives JSON serialization.

Run: `cargo test -p llmenv -- session_log::event 2>&1`
Expected: PASS

- [ ] **Step 7: Commit**

```bash
git add src/session_log/event.rs
# Add any other files that needed trace_fields: None fixes
git add src/session_log/tracing_layer.rs src/hook_run/mod.rs src/session_log/detached.rs src/session_log/dispatch.rs
git commit -m "feat: add log_level() to SessionLogEvent and trace_fields for hook diagnostics"
```

---

### Task 3: hook_run — replace verbose gating with level filtering

**Files:**
- Modify: `src/hook_run/mod.rs`

**Interfaces:**
- Consumes: `SessionLog::any_sink_enabled()`, `SessionLog::any_sink_wants()`, `SessionLog::file_wants()`, `SessionLog::transcript_wants()`, `EventKind::log_level()`, `LogLevel`
- Produces: Updated `run_session_log`, `emit_session_log`, `event_to_log_kind`
- Removes: `verbose_session_event`

- [ ] **Step 1: Update event_to_log_kind to return LogLevel**

Replace the function at lines 240-254:

```rust
/// Maps a `HookEvent` to its session-log `(kind, role, LogLevel)`. `None` for
/// lifecycle/memory events that `handle_session_log` handles separately.
fn event_to_log_kind(event: HookEvent) -> Option<(EventKind, &'static str)> {
    match event {
        HookEvent::UserPromptSubmit => Some((EventKind::Prompt, "user")),
        HookEvent::PreToolUse => Some((EventKind::ToolUse, "tool")),
        HookEvent::PostToolUse => Some((EventKind::ToolResult, "tool")),
        HookEvent::Notification => Some((EventKind::Notification, "system")),
        HookEvent::Stop | HookEvent::SubagentStop => Some((EventKind::Stop, "assistant")),
        HookEvent::PreCompact => Some((EventKind::Notification, "system")),
        HookEvent::SessionStart | HookEvent::TurnStart | HookEvent::SessionEnd => None,
        HookEvent::PostSession => None,
    }
}
```

No change to the signature — `LogLevel` comes from the event kind, not from this function.

- [ ] **Step 2: Update early-exit gate in run()**

Replace lines 457-468:

```rust
    let log_cfg = config.session_log_resolved();
    if !matches!(
        event,
        HookEvent::SessionStart
            | HookEvent::TurnStart
            | HookEvent::SessionEnd
            | HookEvent::PostToolUse
            | HookEvent::PostSession
    ) && !log_cfg.any_sink_enabled()
    {
        return Ok(String::new());
    }
```

- [ ] **Step 3: Update run_session_log to remove verbose gating**

Replace `run_session_log` (lines 617-653) with:

```rust
/// Dispatch the event's session-log handling: baseline lifecycle/scope events
/// for `SessionStart`/`SessionEnd`, or the per-hook capture event for every
/// other mapped event when any sink is enabled. No-op for unmapped events or
/// when no sink cares about this event's level.
async fn run_session_log(
    event: HookEvent,
    call: &SessionLogCall<'_>,
    stdin_payload: &serde_json::Value,
) {
    if matches!(event, HookEvent::SessionStart | HookEvent::SessionEnd) {
        handle_session_log(
            event,
            call.log_cfg,
            call.client,
            call.claude_session_id,
            call.ctx,
            call.state_path,
        )
        .await;
        return;
    }
    let Some((kind, role)) = event_to_log_kind(event) else {
        return;
    };
    let level = kind.log_level();
    if !call.log_cfg.any_sink_wants(level) {
        return;
    }
    let session_id = match call.claude_session_id {
        Some(csid) => {
            ensure_transcript_session(call.log_cfg, call.client, csid, call.ctx, call.state_path)
                .await
        }
        None => {
            debug!(
                "event captured without claude_session_id — transcript record skipped"
            );
            None
        }
    };
    let (tool_name, content) = verbose_content(event, stdin_payload);
    // Build trace_fields for Trace-level events: extract hook stdout/stderr.
    let trace_fields = if level == LogLevel::Trace {
        let mut tf = serde_json::json!({});
        if let Some(stdout) = stdin_payload.get("stdout").and_then(|v| v.as_str()) {
            tf["hook_stdout"] = serde_json::Value::String(stdout.to_string());
        }
        if let Some(stderr) = stdin_payload.get("stderr").and_then(|v| v.as_str()) {
            tf["hook_stderr"] = serde_json::Value::String(stderr.to_string());
        }
        if let Some(exit) = stdin_payload.get("exit_code") {
            tf["hook_exit_code"] = exit.clone();
        }
        Some(tf)
    } else {
        None
    };
    let mut ev = agent_session_event(kind, role, tool_name, content, serde_json::json!({}));
    ev.trace_fields = trace_fields;
    emit_session_log(ev, call.log_cfg, session_id.as_deref());
}
```

- [ ] **Step 3b: Update handle_session_log gate**

In `handle_session_log` (line 694), replace:
```rust
if !(cfg.file || cfg.transcript) {
```
with:
```rust
if !cfg.any_sink_enabled() {
```

- [ ] **Step 3c: Update ensure_transcript_session gate**

In `ensure_transcript_session` (line 735), replace:
```rust
let (true, Some(client)) = (cfg.transcript, client) else {
```
with:
```rust
let (true, Some(client)) = (cfg.transcript_wants(LogLevel::Info), client) else {
```

Lifecycle events are always at Info level, so this is correct — if no sink wants Info events, don't bother starting/recording the transcript session.

- [ ] **Step 4: Update emit_session_log for per-sink level filtering**

Replace `emit_session_log` (lines 760-784) with:

```rust
/// Append `ev` to the configured sinks: the JSONL file (if enabled and
/// `ev.log_level() >= file.level`, written synchronously) and, for
/// agent-session-scoped events, the ICM transcript (if enabled and
/// `ev.log_level() >= transcript.level` — dispatched via a detached child, see
/// `session_log::detached`, so this never blocks on the network). Fail-soft.
fn emit_session_log(ev: SessionLogEvent, cfg: &SessionLog, session_id: Option<&str>) {
    let max = cfg.max_content_bytes.unwrap_or(16_384);
    let ev = ev.truncated(max);
    let level = ev.log_level();
    if cfg.file_wants(level) {
        let path = cfg.file_path().map(std::path::PathBuf::from).or_else(|| {
            crate::session_log::default_file_path()
                .inspect_err(|e| debug!("session_log: file sink disabled, no path resolved: {e}"))
                .ok()
        });
        if let Some(p) = path {
            crate::session_log::FileSink::new(p).append(&ev.to_jsonl());
        }
    }
    if cfg.transcript_wants(level)
        && ev.scope == EventScope::AgentSession
        && let Some(sid) = session_id
    {
        crate::session_log::detached::spawn_record(sid, &ev);
    }
}
```

- [ ] **Step 5: Remove verbose_session_event function (lines 829-838)**

Delete the `verbose_session_event` function — it's no longer called.

- [ ] **Step 6: Build and fix compile errors**

Run: `cargo build 2>&1`
Expected: may have errors in hook tests — fix as they come up.

- [ ] **Step 7: Update all hook_run tests**

The tests at lines 1139-1260 test `event_to_log_kind` and `verbose_content` — these still work as-is since the signatures didn't change.

The tests at lines 1865-1966 test `ensure_transcript_session` and `handle_session_log` — these construct `SessionLog` directly and need updating:

Update `file_only_cfg`:
```rust
fn file_only_cfg(path: &std::path::Path) -> SessionLog {
    SessionLog {
        file: Some(FileSinkConfig {
            enabled: true,
            level: LogLevel::Info,
            path: Some(path.to_string_lossy().into_owned()),
        }),
        transcript: Some(TranscriptSinkConfig {
            enabled: false,
            level: LogLevel::Info,
        }),
        max_content_bytes: None,
    }
}
```

Update test at line 1897-1900 to use `transcript_wants`:
```rust
// Existing test ensures transcript: true — now it's a TranscriptSinkConfig:
let cfg = SessionLog {
    transcript: Some(TranscriptSinkConfig {
        enabled: true,
        level: LogLevel::Info,
    }),
    ..file_only_cfg(&state_dir.path().join("unused.jsonl"))
};
```

Similarly update lines 1923-1926 and 1946-1949.

- [ ] **Step 8: Run all hook_run tests**

Run: `cargo test -p llmenv -- hook_run 2>&1`
Expected: PASS

- [ ] **Step 9: Commit**

```bash
git add src/hook_run/mod.rs
git commit -m "feat: replace verbose gating with per-sink level filtering in hook_run"
```

---

### Task 4: Adapter — register turn hooks when any sink is enabled

**Files:**
- Modify: `src/adapter/claude_code.rs:73-88, 987-998`
- Modify: `src/adapter/claude_code.rs` — tests at lines 2053-2166

- [ ] **Step 1: Rename VERBOSE_HOOK_EVENTS and update registration gate**

Rename `VERBOSE_HOOK_EVENTS` to `SESSION_LOG_HOOK_EVENTS` (line 80) and update the comment:

```rust
/// `(engine-neutral event, native Claude event)` pairs registered when any
/// session-log sink is enabled — per-hook prompt/tool-use capture (#382).
const SESSION_LOG_HOOK_EVENTS: &[(&str, &str)] = &[
    ("user_prompt_submit", "UserPromptSubmit"),
    ("pre_tool_use", "PreToolUse"),
    ("post_tool_use", "PostToolUse"),
    ("notification", "Notification"),
    ("stop", "Stop"),
    ("subagent_stop", "SubagentStop"),
    ("pre_compact", "PreCompact"),
];
```

Replace lines 987-998:

```rust
    // Session-log turn hooks: per-prompt/tool-use capture, registered when any
    // sink is enabled (#382). The hook-run binary filters by per-sink level.
    if manifest.session_log.any_sink_enabled() {
        for (neutral_event, native_event) in SESSION_LOG_HOOK_EVENTS {
            hooks_by_event
                .entry((*native_event).to_string())
                .or_default()
                .push(json!({
                    "hooks": [{ "type": "command", "command": format!("{HOOK_RUN_COMMAND} {neutral_event}") }],
                }));
        }
    }
```

- [ ] **Step 2: Update baseline test**

In `baseline_injects_sessionstart_sessionend_only` (line 2054): the test assertion about verbose hooks not appearing is now about "session log hooks not appearing when no sink is enabled." The default `MergedManifest::default()` has `SessionLog::default()` → transcript enabled at info → `any_sink_enabled()` is true → the hooks WILL appear. Update the test:

```rust
#[test]
fn baseline_injects_sessionstart_sessionend_only() {
    // Default SessionLog has transcript enabled at info, so turn hooks
    // register. Explicitly disable all sinks for the baseline check.
    let manifest = crate::merge::MergedManifest {
        session_log: crate::config::SessionLog {
            transcript: Some(crate::config::TranscriptSinkConfig {
                enabled: false,
                level: crate::config::LogLevel::Info,
            }),
            ..Default::default()
        },
        ..Default::default()
    };
    let settings = render_settings_for_test(&manifest);

    assert!(
        hook_commands_for(&settings, "SessionStart")
            .contains(&format!("{HOOK_RUN_COMMAND} session_start"))
    );
    assert!(
        hook_commands_for(&settings, "SessionEnd")
            .contains(&format!("{HOOK_RUN_COMMAND} session_end"))
    );
    assert!(
        hook_commands_for(&settings, "PreToolUse")
            .iter()
            .any(|c| c.starts_with(HOOK_RUN_COMMAND)),
        "PreToolUse must carry a hook-run command for read-once"
    );
    for event in [
        "PostToolUse",
        "UserPromptSubmit",
        "Stop",
        "SubagentStop",
        "Notification",
        "PreCompact",
    ] {
        assert!(
            hook_commands_for(&settings, event)
                .iter()
                .all(|c| !c.starts_with(HOOK_RUN_COMMAND)),
            "{event} must not carry a hook-run command when all sinks are disabled; got {:?}",
            hook_commands_for(&settings, event)
        );
    }
}
```

- [ ] **Step 3: Update verbose test → session_log hooks test**

Replace `verbose_injects_all_turn_hooks` (line 2135):

```rust
#[test]
fn session_log_injects_all_turn_hooks_when_sink_enabled() {
    let manifest = crate::merge::MergedManifest {
        session_log: crate::config::SessionLog {
            transcript: Some(crate::config::TranscriptSinkConfig {
                enabled: true,
                level: crate::config::LogLevel::Info,
            }),
            ..Default::default()
        },
        ..Default::default()
    };
    let settings = render_settings_for_test(&manifest);

    for (event, neutral) in [
        ("UserPromptSubmit", "user_prompt_submit"),
        ("PreToolUse", "pre_tool_use"),
        ("PostToolUse", "post_tool_use"),
        ("Notification", "notification"),
        ("Stop", "stop"),
        ("SubagentStop", "subagent_stop"),
        ("PreCompact", "pre_compact"),
    ] {
        let expected = format!("{HOOK_RUN_COMMAND} {neutral}");
        assert!(
            hook_commands_for(&settings, event).contains(&expected),
            "{event} missing {expected:?}; got {:?}",
            hook_commands_for(&settings, event)
        );
    }
    assert!(
        hook_commands_for(&settings, "SessionStart")
            .contains(&format!("{HOOK_RUN_COMMAND} session_start"))
    );
}
```

- [ ] **Step 4: Run adapter tests**

Run: `cargo test -p llmenv -- adapter::claude_code 2>&1`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add src/adapter/claude_code.rs
git commit -m "feat: register session-log turn hooks when any sink is enabled"
```

---

### Task 5: main.rs — update file sink wiring

**Files:**
- Modify: `src/main.rs:8-37`

- [ ] **Step 1: Update session_log_file_path and file sink wiring**

Replace lines 8-37:

```rust
use llmenv::session_log::{FileLogLayer, FileSink, default_file_path};

/// Resolve the session-log file sink's path: explicit `path:` override
/// (tilde-expanded) or `<state_dir>/session-log.jsonl`.
fn session_log_file_path(configured: Option<&str>) -> PathBuf {
    match configured {
        Some(raw) => PathBuf::from(llmenv_paths::expand_tilde(raw)),
        None => default_file_path().unwrap_or_else(|_| PathBuf::from("session-log.jsonl")),
    }
}

fn main() {
    let config_path = llmenv_paths::config_path();
    if let Err(ref e) = config_path {
        eprintln!("llmenv: failed to resolve config path: {e:#}");
    }
    let resolved = config_path
        .ok()
        .and_then(|p| {
            llmenv_config::Config::load(&p)
                .inspect_err(|e| {
                    eprintln!("llmenv: failed to load config from {}: {e:#}", p.display())
                })
                .ok()
        })
        .map(|c| c.session_log_resolved())
        .unwrap_or_default();

    let file_sink_enabled = resolved.file.as_ref().is_some_and(|f| f.enabled);
    let file_layer = file_sink_enabled.then(|| {
        let path = session_log_file_path(resolved.file_path());
        FileLogLayer::new(FileSink::new(path)).with_filter(EnvFilter::from_default_env())
    });

    tracing_subscriber::registry()
        .with(
            tracing_subscriber::fmt::layer()
                .with_writer(std::io::stderr)
                .with_filter(EnvFilter::from_default_env()),
        )
        .with(file_layer)
        .init();

    if let Err(e) = llmenv::cli::run() {
        eprintln!("llmenv: {e:#}");
        std::process::exit(1);
    }
}
```

- [ ] **Step 2: Build and verify**

Run: `cargo build 2>&1`
Expected: compiles clean

- [ ] **Step 3: Commit**

```bash
git add src/main.rs
git commit -m "feat: wire file sink from new SessionLog.file config"
```

---

### Task 6: Integration — end-to-end test and final verification

**Files:**
- No new files — verify the full pipeline

- [ ] **Step 1: Full test suite**

Run: `cargo test --all-targets 2>&1`
Expected: all tests PASS

- [ ] **Step 2: Clippy**

Run: `cargo clippy --all-targets --all-features -- -D warnings 2>&1`
Expected: zero warnings

- [ ] **Step 3: fmt**

Run: `cargo fmt -- --check 2>&1`
Expected: no formatting changes needed

- [ ] **Step 4: Manual smoke test — verify transcript default still works**

```bash
echo '{"session_id":"smoke-1","event":{"ts":"2026-07-13T00:00:00Z","kind":"prompt","scope":"agent_session","role":"user","content":"hello","fields":{},"trace_fields":null}}' | llmenv session-log-record 2>&1
echo "EXIT=$?"
```

Expected: EXIT=0

- [ ] **Step 5: Commit if any fixups needed; otherwise final status check**

```bash
git status
```

---

### Task 7: Changelog

**Files:**
- Modify: `CHANGELOG.md`

- [ ] **Step 1: Add entry under [Unreleased]**

```markdown
### Changed

- `session_log.verbose` replaced with per-sink `level` (info/debug/trace).
  `session_log.file` and `session_log.transcript` are now mapping blocks with
  `enabled` + `level` fields. Old boolean shape still parses. ([#740](https://github.com/phaedrus1992/llmenv/issues/740))
```

- [ ] **Step 2: Commit**

```bash
git add CHANGELOG.md
git commit -m "docs: add changelog entry for session-log levels"
```
