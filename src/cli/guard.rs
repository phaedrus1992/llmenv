//! Guard command dispatch: config-guard, config-context, check-stale, throttle.
//!
//! These commands run on every tool call, outside `hook-run`. This module
//! handles their dispatch from the CLI match arm.

/// Run a guard command and emit its `llmenv-trace` marker (`op: "guard"`).
///
/// The marker carries the command name and its total wall time, so a harness can compare the
/// per-call cost of each guard with `hook-run pre_tool_use` (#3016).
pub(super) fn timed<R>(command: &str, f: impl FnOnce() -> R) -> R {
    let t0 = std::time::Instant::now();
    let result = f();
    crate::trace::emit_guard_trace(command, t0.elapsed());
    result
}

/// Dispatch `check-stale`. Called from the CLI `run` match arm.
pub(super) fn dispatch_check_stale(
    use_color: bool,
    auto_fix: bool,
    engine: &str,
) -> anyhow::Result<()> {
    dispatch_guard_command(engine, "check-stale");
    timed("check-stale", || {
        crate::cli::run_check_stale(use_color, auto_fix)
    })
}

/// Dispatch `config-context`. Called from the CLI `run` match arm.
pub(super) fn dispatch_config_context(engine: &str) -> anyhow::Result<()> {
    dispatch_guard_command(engine, "config-context");
    timed("config-context", crate::cli::run_config_context)
}

/// Dispatch `config-guard`. Called from the CLI `run` match arm.
pub(super) fn dispatch_config_guard(engine: &str) {
    dispatch_guard_command(engine, "config-guard");
    timed("config-guard", || crate::cli::config_guard::run(engine));
}

/// Dispatch `tmp-guard`. Called from the CLI `run` match arm.
pub(super) fn dispatch_tmp_guard(engine: &str) {
    dispatch_guard_command(engine, "tmp-guard");
    timed("tmp-guard", || crate::cli::tmp_guard::run(engine));
}

/// Dispatch `throttle`. Called from the CLI `run` match arm.
pub(super) fn dispatch_throttle(event: &str) {
    dispatch_guard_command(event, "throttle");
    timed("throttle", || crate::throttle::run_throttle_hook(event));
}

/// Log a guard command invocation and warn about unknown engines.
fn dispatch_guard_command(engine: &str, name: &str) {
    tracing::debug!(engine, "{name} invoked by engine");
    crate::cli::warn_if_unknown_engine(engine);
}
