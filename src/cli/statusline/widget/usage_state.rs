//! Usage-state files that carry the last reading of each usage window, for the pace delta.

use super::*;

/// Delta from the previously-recorded used percentage for `key`, e.g.
/// `" (+4.5)"`. Best-effort: reads/writes a small `<state_dir>/<key>` file
/// (`"<pct> <unix_ts>"`), rewriting at most once a minute so the delta
/// reflects real change rather than per-render noise. Empty when there's no
/// state dir, no prior sample, or the change rounds to zero.
pub(super) fn usage_delta(state_dir: Option<&Path>, key: &str, pct: f64, now: i64) -> String {
    let Some(dir) = state_dir else {
        return String::new();
    };
    let path = dir.join(key);
    let prev = read_usage_state(&path);
    if prev.is_none_or(|(_, ts)| now - ts >= 60) {
        write_usage_state(&path, pct, now);
    }
    match prev {
        Some((prev_pct, _)) => format_delta(pct - prev_pct),
        None => String::new(),
    }
}

fn read_usage_state(path: &Path) -> Option<(f64, i64)> {
    let contents = match std::fs::read_to_string(path) {
        Ok(contents) => contents,
        // A missing file is the first sample. Any other read error is logged, not hidden.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(e) => {
            tracing::error!("usage state {} cannot be read: {e}", path.display());
            return None;
        }
    };
    let mut it = contents.split_whitespace();
    let pct = it.next()?.parse().ok()?;
    let ts = it.next()?.parse().ok()?;
    Some((pct, ts))
}

pub(super) fn write_usage_state(path: &Path, pct: f64, now: i64) {
    if let Some(parent) = path.parent()
        && let Err(e) = crate::paths::create_dir_owner_only(parent)
    {
        tracing::error!(
            "usage delta state dir {} unavailable: {e}",
            parent.display()
        );
        return;
    }
    // Best-effort: a failed write only means the next render's `{delta}`
    // placeholder is empty, not a broken widget — but still worth a trace.
    if let Err(e) = crate::paths::write_owner_only(path, format!("{pct} {now}").as_bytes()) {
        tracing::error!("usage delta state write to {} failed: {e}", path.display());
    }
}

fn format_delta(delta: f64) -> String {
    if delta.abs() < 0.05 {
        String::new()
    } else if delta > 0.0 {
        format!(" (+{delta:.1})")
    } else {
        format!(" ({delta:.1})")
    }
}

/// Directory for usage delta state, alongside the materialized session data
/// (`$CLAUDE_CONFIG_DIR/statusline-state`). `None` when the engine didn't
/// export `CLAUDE_CONFIG_DIR`, which disables delta tracking.
pub(super) fn usage_state_dir() -> Option<PathBuf> {
    // A value that is not UTF-8 turns tracking off, so the log says why (#2607).
    usage_state_dir_with_env(&|name| {
        crate::env_var::utf8_var(name)
            .inspect_err(|e| tracing::warn!("usage state dir: {e:#}"))
            .ok()
            .flatten()
    })
}

/// [`usage_state_dir`] with an injectable env-var provider so tests can
/// exercise the set-but-empty case without mutating real process env vars.
pub(super) fn usage_state_dir_with_env(
    get_env: &impl Fn(&str) -> Option<String>,
) -> Option<PathBuf> {
    // A set-but-empty `CLAUDE_CONFIG_DIR=` must disable delta tracking like an
    // unset one, not resolve to the relative path `statusline-state` (#1111).
    get_env("CLAUDE_CONFIG_DIR")
        .filter(|d| !d.is_empty())
        .map(|d| PathBuf::from(d).join("statusline-state"))
}
