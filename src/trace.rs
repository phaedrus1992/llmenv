//! Structured trace telemetry for `LLMENV_TRACE_TIMING` and `LLMENV_TRACE_FILE`.
//!
//! This module is the single sink for every trace line the binary emits:
//! the `llmenv-trace <json>` marker, the `[LLMENV_CACHE]` lines, the
//! `[LLMENV_MCP_CALL]` lines, and the `[LLMENV_SPAWN]` / `[LLMENV_LEDGER]`
//! lines added by later sub-issues. Two env vars control where lines go:
//!
//! - `LLMENV_TRACE_TIMING` — when set, every trace line is also written to
//!   stderr. This is the original path and remains the default for
//!   interactive shells.
//! - `LLMENV_TRACE_FILE` — when set, every trace line is appended as one
//!   JSON object to the named file using `O_APPEND`. Concurrent hooks and
//!   detached jobs use this path so their output reaches a file instead of
//!   a detached stderr.
//!
//! The JSON marker (`llmenv-trace <json>`) carries a `"v": 1` field so a
//! consumer can distinguish formats when new fields are added (#3018).
//! All timing values use microseconds (`_us`).

use std::time::{Duration, Instant};

pub(crate) use llmenv_util::trace::{
    SpawnExit, append_trace_line, emit_spawn_trace, emit_trace_line, enabled, file_sink_path,
    spawn_counted, spawned_jobs,
};

/// Build the `llmenv-trace <json>` marker line. The JSON object is flat: `fields` keeps its
/// own keys, and the line adds `"v": 1` and `"op"`. Existing consumers read the timing keys
/// at the top level.
/// A caller field named `v` or `op` would be overwritten, so the marker would hold duplicate JSON
/// keys. The check runs in every build, not only debug (#3118).
fn json_trace_line(
    op: &str,
    mut fields: serde_json::Map<String, serde_json::Value>,
) -> anyhow::Result<String> {
    if let Some(key) = ["v", "op"]
        .into_iter()
        .find(|key| fields.contains_key(*key))
    {
        anyhow::bail!(
            "trace op {op} has a field named {key}, which the marker reserves. Fix: rename \
             the field."
        );
    }
    fields.insert("v".to_string(), serde_json::json!(1));
    fields.insert("op".to_string(), serde_json::json!(op));
    Ok(format!(
        "llmenv-trace {}",
        serde_json::Value::Object(fields)
    ))
}

/// Emit the `llmenv-trace <json>` marker for `op` to the trace sink.
pub(crate) fn emit_json_trace(op: &str, fields: serde_json::Map<String, serde_json::Value>) {
    match json_trace_line(op, fields) {
        Ok(line) => emit_trace_line(&line),
        // A trace must never fail the command it observes, so the error is logged instead.
        Err(e) => tracing::error!("{e:#}"),
    }
}

/// Measured durations of the `SessionStart` sub-phases that run inside `prep_us` (#3011).
///
/// Each field is `None` when the sub-phase did not run.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SessionStartPhases {
    /// The MCP health probe.
    pub mcp_probe: Option<Duration>,
    /// The restart of the memory proxy, when it was down.
    pub proxy_restart: Option<Duration>,
    /// The second probe of the memory server after a restart.
    pub drift_check: Option<Duration>,
    /// The check for stale state/tmp entries.
    pub stale_hint: Option<Duration>,
    /// The codebase-memory allow-root check.
    pub cbm_roots: Option<Duration>,
}

impl SessionStartPhases {
    /// The phases of a run that probed the MCP servers and took `took` to do it.
    pub(crate) fn probed(took: Duration) -> Self {
        Self {
            mcp_probe: Some(took),
            ..Self::default()
        }
    }
}

/// Emit a `[LLMENV_CACHE] <name> <hit|miss> <duration>ms[ <extra>]` line to
/// the trace sink.
pub(crate) fn emit_cache_trace(name: &str, hit: bool, elapsed: Duration, extra: Option<&str>) {
    if enabled() {
        emit_trace_line(&lines::cache_line(name, hit, elapsed, extra));
    }
}

/// Build the `LLMENV_TRACE_TIMING` marker payload from whichever phase
/// boundaries `run_inner` reached before returning. `t0`/`t_config` are
/// always available (computed before any early return); `t_scope`/`t_chunk`/
/// `t_end` are `None` on a path that returned before reaching them. Each
/// field is included only when its Instant is present, so an early return
/// still reports the phases it actually ran through instead of nothing
/// (#1128: previously only events reaching the full memory-dispatch path —
/// 4 of 11 — emitted this marker at all).
///
/// `phases` carries optional sub-phase boundaries for `SessionStart` that
/// break out `prep_us` + `mcp_us` into individual fields (#3011). Pass a
/// struct with `None` fields when the sub-phase did not run, so the
/// early-return rule holds.
///
/// The JSON marker carries `"v": 1` so a consumer can distinguish formats
/// when new optional fields are added by later sub-issues (#3018).
/// Phase boundaries and per-event timings for one hook run.
///
/// `t0` and `t_config` are always set. Every other field is `None` when the
/// run returned before that phase.
pub(crate) struct HookTiming {
    pub t0: Instant,
    pub t_config: Instant,
    pub t_scope: Option<Instant>,
    pub t_chunk: Option<Instant>,
    pub t_end: Option<Instant>,
    pub phases: SessionStartPhases,
    pub session_log_us: Option<u64>,
    pub checkpoint_us: Option<u64>,
    pub spawn_us: Option<u64>,
    pub spawned_jobs: Option<u64>,
}

impl HookTiming {
    /// Add the instant when scope evaluation ended.
    pub(crate) fn through_scope(self, t_scope: Instant) -> Self {
        Self {
            t_scope: Some(t_scope),
            ..self
        }
    }

    /// Add the instants of the end of prep and of the end of the memory calls.
    pub(crate) fn through_mcp(self, t_chunk: Instant, t_end: Instant) -> Self {
        Self {
            t_chunk: Some(t_chunk),
            t_end: Some(t_end),
            ..self
        }
    }

    /// Add the per-event timings and the sub-phase durations of a finished run.
    pub(crate) fn with_events(
        self,
        phases: SessionStartPhases,
        session_log_us: Option<u64>,
        checkpoint_us: Option<u64>,
        spawn_us: Option<u64>,
    ) -> Self {
        Self {
            phases,
            session_log_us,
            checkpoint_us,
            spawn_us,
            spawned_jobs: Some(spawned_jobs()),
            ..self
        }
    }

    /// Timing for a run that returned right after the config load.
    pub(crate) fn new(t0: Instant, t_config: Instant) -> Self {
        Self {
            t0,
            t_config,
            t_scope: None,
            t_chunk: None,
            t_end: None,
            phases: SessionStartPhases::default(),
            session_log_us: None,
            checkpoint_us: None,
            spawn_us: None,
            spawned_jobs: None,
        }
    }
}

fn trace_timing_json(timing: HookTiming) -> serde_json::Map<String, serde_json::Value> {
    // Cap rather than panic on the (unreachable) overflow of an in-process
    // Instant delta past u64::MAX microseconds (~585,000 years).
    let us = |d: Duration| u64::try_from(d.as_micros()).unwrap_or(u64::MAX);
    let HookTiming {
        t0,
        t_config,
        t_scope,
        t_chunk,
        t_end,
        phases,
        ..
    } = timing;
    let mut fields = serde_json::Map::new();
    fields.insert(
        "config_load_us".to_string(),
        serde_json::json!(us(t_config.saturating_duration_since(t0))),
    );
    if let Some(t_scope) = t_scope {
        fields.insert(
            "scope_eval_us".to_string(),
            serde_json::json!(us(t_scope.saturating_duration_since(t_config))),
        );
        if let Some(t_chunk) = t_chunk {
            fields.insert(
                "prep_us".to_string(),
                serde_json::json!(us(t_chunk.saturating_duration_since(t_scope))),
            );
            let sub_phases = [
                ("mcp_probe_us", phases.mcp_probe),
                ("proxy_restart_us", phases.proxy_restart),
                ("drift_check_us", phases.drift_check),
                ("stale_hint_us", phases.stale_hint),
                ("cbm_roots_us", phases.cbm_roots),
            ];
            for (name, took) in sub_phases {
                if let Some(took) = took {
                    fields.insert(name.to_string(), serde_json::json!(us(took)));
                }
            }
            if let Some(t_end) = t_end {
                fields.insert(
                    "mcp_us".to_string(),
                    serde_json::json!(us(t_end.saturating_duration_since(t_chunk))),
                );
            }
        }
    }
    // Per-hook-event fields for session-log and checkpoint timing (#3013).
    let extras = [
        ("session_log_us", timing.session_log_us),
        ("checkpoint_us", timing.checkpoint_us),
        ("spawn_us", timing.spawn_us),
        ("spawned_jobs", timing.spawned_jobs),
    ];
    for (name, value) in extras {
        if let Some(v) = value {
            fields.insert(name.to_string(), serde_json::json!(v));
        }
    }
    fields
}

/// The hook marker as parsed JSON, as a consumer reads it. For tests.
#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "a test helper: a broken marker must fail the test"
)]
pub(crate) fn hook_marker(timing: HookTiming) -> serde_json::Value {
    let line = json_trace_line("hook_run", trace_timing_json(timing)).expect("hook marker keys");
    serde_json::from_str(line.strip_prefix("llmenv-trace ").expect("marker prefix"))
        .expect("marker json")
}

/// Emit the per-phase timing marker to the trace sink.
pub(crate) fn emit_trace_timing(timing: HookTiming) {
    emit_json_trace("hook_run", trace_timing_json(timing));
}

/// Emit the `llmenv-trace` marker of a guard command: `{"op":"guard","command":<name>,
/// "total_us":<n>}`. `command` is one of the five fixed guard names (#3016).
pub(crate) fn emit_guard_trace(command: &str, total: Duration) {
    if !enabled() {
        return;
    }
    let total_us = u64::try_from(total.as_micros()).unwrap_or(u64::MAX);
    let mut fields = serde_json::Map::new();
    fields.insert("command".to_string(), serde_json::json!(command));
    fields.insert("total_us".to_string(), serde_json::json!(total_us));
    emit_json_trace("guard", fields);
}

/// Emit a `[LLMENV_LEDGER] <op> <duration>us` line to the trace sink.
///
/// Tracks the time to load or update a per-session recall ledger file.
pub(crate) fn emit_ledger_trace(op: &str, elapsed: Duration) {
    if enabled() {
        emit_trace_line(&lines::ledger_update_line(op, elapsed));
    }
}

mod detached;
pub(crate) mod lines;

#[cfg(test)]
pub(crate) mod docs_sync;

pub(crate) use detached::{DetachedJob, emit_detached_trace, json_str};

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests;
