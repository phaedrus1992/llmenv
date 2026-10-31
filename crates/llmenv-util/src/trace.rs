//! Trace sink and spawn trace shared by every crate (#3010, #3019).
//!
//! Two env vars control where trace lines go: `LLMENV_TRACE_TIMING` writes each line to stderr,
//! and `LLMENV_TRACE_FILE` appends each line to a file as one JSON object.

use std::io::Write;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// Whether any trace sink is on: `LLMENV_TRACE_TIMING` (stderr) or `LLMENV_TRACE_FILE` (file).
///
/// An emitter returns early when this is `false`, so an untraced run pays for no formatting.
pub fn enabled() -> bool {
    std::env::var_os("LLMENV_TRACE_TIMING").is_some()
        || std::env::var_os("LLMENV_TRACE_FILE").is_some()
}

/// The file named by `LLMENV_TRACE_FILE`, or `None` when it is unset or empty.
pub fn file_sink_path() -> Option<std::path::PathBuf> {
    std::env::var_os("LLMENV_TRACE_FILE")
        .filter(|p| !p.is_empty())
        .map(std::path::PathBuf::from)
}

/// Emit `line` to stderr when `LLMENV_TRACE_TIMING` is set, and append it to the file named by
/// `LLMENV_TRACE_FILE` when that is set.
///
/// The file gets one JSON object per line with `ts` (Unix seconds), `pid`, and `line`, so a
/// consumer can parse it as JSON Lines.
pub fn emit_trace_line(line: &str) {
    if std::env::var_os("LLMENV_TRACE_TIMING").is_some() {
        // A closed stderr must not abort the hook that is being traced. The loss itself is
        // recorded for the agent, because hook stderr never reaches it (#3205).
        if let Err(e) = writeln!(std::io::stderr(), "{line}") {
            crate::agent_notice::record_logged(format!(
                "llmenv: cannot write a trace line to stderr: {e}. \
                 Fix: unset LLMENV_TRACE_TIMING."
            ));
        }
    }
    if let Some(path) = file_sink_path() {
        append_trace_line(&path, line);
    }
}

/// Say once per process that the trace file cannot be written. A hook or a detached job has no
/// visible log, so a bad path would otherwise give an empty trace with no explanation.
fn report_sink_error(path: &std::path::Path, error: &std::io::Error) {
    static REPORTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if !REPORTED.swap(true, std::sync::atomic::Ordering::Relaxed) {
        crate::agent_notice::record_logged(sink_error_notice(path, error));
    }
}

/// The notice for a trace file that cannot be written. Kept apart from [`report_sink_error`]
/// so a test can check the text without the once-per-process flag.
pub fn sink_error_notice(path: &std::path::Path, error: &std::io::Error) -> String {
    format!(
        "llmenv: cannot write the trace file {}: {error}. \
         Fix: set LLMENV_TRACE_FILE to a path in a writable directory that is not a symlink.",
        path.display()
    )
}

/// Append `line` to `path` as one JSON object, in a single write.
///
/// The file is created owner-only, an existing looser file is tightened, and a symlink at `path`
/// is refused, because a planted link would redirect the appends.
pub fn append_trace_line(path: &std::path::Path, line: &str) {
    let mut file = match llmenv_paths::open_append_owner_only(path) {
        Ok(file) => file,
        Err(e) => {
            report_sink_error(path, &e);
            return;
        }
    };
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let obj = serde_json::json!({ "ts": ts, "pid": std::process::id(), "line": line });
    // One write call keeps concurrent `O_APPEND` writers from interleaving inside a line.
    if let Err(e) = file.write_all(format!("{obj}\n").as_bytes()) {
        report_sink_error(path, &e);
    }
}

/// How a traced child process ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpawnExit {
    /// The child exited with this code.
    Code(i32),
    /// The child ended on a signal.
    Signal,
    /// The child ran past its deadline and was killed.
    Timeout,
    /// The child did not start, or its output could not be read.
    Error,
    /// The parent started the child and did not wait for it.
    Detached,
}

impl SpawnExit {
    /// The outcome of a probe of a child over a pipe: it answered, it timed out, or it failed.
    pub fn of_probe(answered: bool, timed_out: bool) -> Self {
        match (answered, timed_out) {
            (true, _) => Self::Code(0),
            (false, true) => Self::Timeout,
            (false, false) => Self::Error,
        }
    }

    /// Classify the result of a waited child.
    pub fn of(result: &std::io::Result<std::process::Output>) -> Self {
        match result {
            Ok(out) => out.status.code().map_or(Self::Signal, Self::Code),
            Err(e) if e.kind() == std::io::ErrorKind::TimedOut => Self::Timeout,
            Err(_) => Self::Error,
        }
    }
}

/// Emit a `[LLMENV_SPAWN] <label> <duration>us exit=<code|signal|timeout|spawn-error|detached>`
/// line to the trace sink. The label is a fixed string such as `probe:ssid` — never print
/// arguments, because they can carry secrets such as keychain item names.
pub fn emit_spawn_trace(label: &str, elapsed: Duration, exit: SpawnExit) {
    if enabled() {
        emit_trace_line(&spawn_line(label, elapsed, exit));
    }
}

/// `[LLMENV_SPAWN] <label> <us>us exit=<exit>`
pub fn spawn_line(label: &str, elapsed: Duration, exit: SpawnExit) -> String {
    use SpawnExit;
    let us = elapsed.as_micros();
    let exit = match exit {
        SpawnExit::Code(code) => code.to_string(),
        SpawnExit::Signal => "signal".to_string(),
        SpawnExit::Timeout => "timeout".to_string(),
        SpawnExit::Error => "spawn-error".to_string(),
        SpawnExit::Detached => "detached".to_string(),
    };
    format!("[LLMENV_SPAWN] {label} {us}us exit={exit}")
}

/// Detached jobs this process has started. The hook marker reports it as `spawned_jobs`.
static SPAWNED_JOBS: AtomicU64 = AtomicU64::new(0);

/// Start a detached job, count it when the start succeeds, and print one `[LLMENV_SPAWN]` line
/// with `exit=detached` (or `spawn-error` when the start fails). `label` is a fixed tag. The
/// duration is the time the start took, not the job's own run time (#3013).
pub fn spawn_counted(
    label: &str,
    cmd: &mut std::process::Command,
) -> std::io::Result<std::process::Child> {
    let started = std::time::Instant::now();
    let result = cmd.spawn();
    let exit = if result.is_ok() {
        SPAWNED_JOBS.fetch_add(1, Ordering::Relaxed);
        SpawnExit::Detached
    } else {
        SpawnExit::Error
    };
    emit_spawn_trace(label, started.elapsed(), exit);
    result
}

/// How many detached jobs this process has started.
pub fn spawned_jobs() -> u64 {
    SPAWNED_JOBS.load(Ordering::Relaxed)
}

/// `[LLMENV_MCP_CALL] <name> <us>us req=<n> resp=<n>[ status=<code>]`
pub fn mcp_call_line(
    name: &str,
    elapsed: Duration,
    req_bytes: u64,
    resp_bytes: u64,
    status: Option<u16>,
) -> String {
    let us = elapsed.as_micros();
    let mut line = format!("[LLMENV_MCP_CALL] {name} {us}us req={req_bytes} resp={resp_bytes}");
    if let Some(code) = status {
        line.push_str(&format!(" status={code}"));
    }
    line
}
