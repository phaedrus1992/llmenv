//! Structured cache hit/miss telemetry (#1260), covering the cache layers
//! that persist across `llmenv` invocations: the content-hash materialize
//! cache, the merge-signature cache, the read-once dedup cache, and the
//! plugin marketplace cache.
//!
//! Gated behind the existing `LLMENV_TRACE_TIMING` convention hook-run's
//! per-phase timing markers already use (`src/hook_run/mod.rs`'s
//! `emit_trace_timing`) rather than a new env var, so there's one flag to
//! know about instead of two.
//!
//! A fifth candidate layer — a "scope context" cache said to have been added
//! across hook-run invocations — was evaluated and excluded: no such cache
//! exists in this codebase (see #1257's discussion), and `hook-run` is a
//! fresh process per invocation, so an in-process cache could never persist
//! "across hook events" the way the original report described.

use std::time::Duration;

/// Emit `[LLMENV_CACHE] <name> <hit|miss> <duration>ms[ <extra>]` to stderr
/// when `LLMENV_TRACE_TIMING` is set (any value). Bare `eprintln!`, not
/// `tracing`, matching `emit_trace_timing`'s precedent: this is a
/// machine-parseable protocol for an external harness, not a log line, so it
/// must not depend on the tracing subscriber's level filtering or format.
pub(crate) fn emit_cache_trace(name: &str, hit: bool, elapsed: Duration, extra: Option<&str>) {
    if std::env::var_os("LLMENV_TRACE_TIMING").is_none() {
        return;
    }
    let status = if hit { "hit" } else { "miss" };
    let ms = elapsed.as_secs_f64() * 1000.0;
    match extra {
        Some(extra) => eprintln!("[LLMENV_CACHE] {name} {status} {ms:.3}ms {extra}"),
        None => eprintln!("[LLMENV_CACHE] {name} {status} {ms:.3}ms"),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    // emit_cache_trace's only observable behavior from a test (no stderr
    // capture here) is that it never panics regardless of env state — the
    // gating itself is exercised indirectly by every cache call site's own
    // tests, which run without LLMENV_TRACE_TIMING set.
    #[test]
    fn emit_cache_trace_never_panics_without_env_var() {
        emit_cache_trace("content_hash", true, Duration::from_micros(12), None);
        emit_cache_trace(
            "read_once",
            false,
            Duration::from_secs(0),
            Some("path=CLAUDE.md"),
        );
    }
}
