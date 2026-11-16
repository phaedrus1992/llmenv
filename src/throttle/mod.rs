//! Usage throttling: injects PreToolUse and UserPromptSubmit hooks that poll a
//! backend's usage state and sleep a capped, adaptive delay to avoid rate limits.
//!
//! `run_throttle_hook(event)` is the CLI entry. `compute_delay` is pure and
//! unit-tested. `resolve_active_throttle` selects the single active config entry
//! by tag intersection (same model as memory).

mod backend;
mod umans_fetch;
pub(crate) use backend::{UsageSnapshot, backend_for};

use std::collections::BTreeSet;
use std::path::Path;
use std::time::Duration;

use crate::config::Throttle;
use anyhow::Context;

/// Store the resolved active throttle config for retrieval by hook invocations.
/// Called during `llmenv export` / materialize after `build_manifest` resolves
/// the merged throttle (top-level + bundle). Mirrors `icm::store_tag_memory`.
///
/// When `throttle` is `None`, removes any stale `throttle.json` so a
/// since-removed config doesn't keep throttling.
///
/// # Errors
/// Returns an error if writing the state file fails.
pub(crate) fn store_active_throttle(throttle: Option<&Throttle>) -> anyhow::Result<()> {
    let state_dir = crate::paths::state_dir()?;
    store_active_throttle_with_state_dir(throttle, &state_dir)
}

/// Internal helper taking an injectable state dir so tests can exercise the
/// real write path (`write_owner_only_atomic`) against a temp directory
/// instead of the real state dir, without env var mutation.
fn store_active_throttle_with_state_dir(
    throttle: Option<&Throttle>,
    state_dir: &Path,
) -> anyhow::Result<()> {
    let path = throttle_state_path(state_dir);
    match throttle {
        Some(cfg) => {
            let json = serde_json::to_string(cfg)?;
            crate::paths::write_owner_only_atomic(&path, json.as_bytes())
                .with_context(|| format!("writing throttle state: {}", path.display()))?;
        }
        None => remove_stale_throttle(&path)?,
    }
    Ok(())
}

/// Remove a stale state file. A missing file already means throttling is off,
/// so only other errors (for example a permission error) reach the caller.
fn remove_stale_throttle(path: &Path) -> anyhow::Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => {
            Err(e).with_context(|| format!("removing stale throttle state: {}", path.display()))
        }
    }
}

/// Read the stored throttle config for the hook. A missing file means throttling
/// is off. Any other read or parse error is returned, so the hook logs it
/// instead of silently turning throttling off.
fn read_throttle_config(path: &Path) -> anyhow::Result<Option<Throttle>> {
    let Some(bytes) = crate::paths::bounded::state(path).context("reading throttle state")? else {
        return Ok(None);
    };
    let cfg = serde_json::from_slice(&bytes).with_context(|| {
        format!(
            "parsing throttle state {}: delete the file, or run `llmenv export` to rewrite it",
            path.display()
        )
    })?;
    Ok(Some(cfg))
}

fn throttle_state_path(state_dir: &Path) -> std::path::PathBuf {
    state_dir.join("throttle.json")
}

/// Read back the currently stored throttle state (written by
/// `store_active_throttle` during materialize/export). Returns `None` only
/// when no state file exists, which means throttling is off. A file that
/// exists but cannot be read or parsed is an error, so the caller can show it
/// instead of reporting throttling as off.
///
/// # Errors
/// Returns an error if the state directory cannot be resolved, or if the
/// state file exists and is unreadable or corrupt.
pub(crate) fn read_active_throttle() -> anyhow::Result<Option<Throttle>> {
    let state_dir = crate::paths::state_dir()?;
    read_active_throttle_with_state_dir(&state_dir)
}

/// Internal helper taking an injectable state dir so tests can read from a
/// temp directory instead of the real state dir, without env var mutation.
fn read_active_throttle_with_state_dir(state_dir: &Path) -> anyhow::Result<Option<Throttle>> {
    let path = throttle_state_path(state_dir);
    let Some(bytes) = crate::paths::bounded::state(&path).context("reading throttle state")? else {
        return Ok(None);
    };
    serde_json::from_slice(&bytes)
        .with_context(|| format!("parsing throttle state: {}", path.display()))
}

/// Resolve the single active throttle entry by tag intersection.
///
/// Returns `None` when no entry's `when` tags intersect the active tags.
/// Returns an error when more than one entry is simultaneously active (same
/// single-active invariant as `features.memory`).
///
/// # Errors
/// Returns an error if more than one throttle entry is active.
pub(crate) fn resolve_active_throttle(
    throttle: &[Throttle],
    active_tags: &BTreeSet<String>,
) -> anyhow::Result<Option<Throttle>> {
    let active: Vec<&Throttle> = throttle
        .iter()
        .filter(|t| t.when.iter().any(|tag| active_tags.contains(tag)))
        .collect();
    match active.len() {
        0 => Ok(None),
        1 => Ok(Some(active[0].clone())),
        _ => {
            let backends: Vec<String> = active.iter().map(|t| t.backend.clone()).collect();
            anyhow::bail!(
                "throttle: multiple entries active simultaneously — conflicting backends: {}",
                backends.join(", ")
            )
        }
    }
}

/// Compute the sleep delay for a usage snapshot under the given config.
///
/// Pure function, always capped at `cfg.max_wait`. Algorithm:
/// - penalized → `max_wait` (server is deprioritizing us)
/// - `remaining` unknown → 0 (don't block when we can't measure)
/// - `remaining == 0` → `max_wait`
/// - `remaining < soft_threshold` → `max_wait * (soft_threshold - remaining) / soft_threshold`
/// - otherwise → 0
fn compute_delay(snapshot: &UsageSnapshot, cfg: &Throttle) -> Duration {
    let max = Duration::from_secs(cfg.max_wait);
    if snapshot.penalized {
        return max;
    }
    let Some(remaining) = snapshot.remaining else {
        return Duration::ZERO;
    };
    if remaining == 0 {
        return max;
    }
    let threshold = cfg.soft_threshold;
    if remaining < threshold {
        // Scale linearly: delay = max_wait * (threshold - remaining) / threshold
        let numer = cfg.max_wait.saturating_mul(threshold - remaining);
        Duration::from_secs(numer / threshold)
    } else {
        Duration::ZERO
    }
}

/// CLI entry for `llmenv throttle <event>`. Fail-soft: exits 0 on any error. Never panics.
///
/// The engine drops a hook's stderr, so each failure goes to the error log and to the hook
/// output as a notice for the agent (#2746, #2833).
pub(crate) fn run_throttle_hook(event: &str) {
    use std::io::Read;

    // Consume stdin so the pipe doesn't break, even if we don't use the body.
    let mut stdin_buf = String::new();
    let mut notices = Vec::new();
    let hook_event_name = match std::io::stdin().read_to_string(&mut stdin_buf) {
        Ok(_) => hook_event_name(&stdin_buf, event, &mut notices),
        Err(e) => {
            notices.push(format!(
                "llmenv: the throttle hook cannot read its stdin ({e}), so it did not run. \
                 Fix: run `llmenv doctor` to check the hook setup."
            ));
            write_output(&engine_event_name(event), "", &notices);
            return;
        }
    };
    let note = run_throttle_inner(event).unwrap_or_else(|e| {
        notices.push(format!(
            "llmenv: the throttle hook failed: {e:#}. Fix: run `llmenv doctor` to check the \
             throttle state."
        ));
        String::new()
    });
    write_output(&hook_event_name, &note, &notices);
}

/// The engine name of the hook event in `stdin`. Text that is not JSON, or JSON with no name or an
/// empty name, falls back to the name that `event` maps to, and adds a notice. Empty stdin is a
/// hook run by hand: it falls back with no notice. A name that stays empty adds a notice too.
fn hook_event_name(stdin: &str, event: &str, notices: &mut Vec<String>) -> String {
    let parsed = match serde_json::from_str::<serde_json::Value>(stdin) {
        Ok(v) => match &v["hook_event_name"] {
            serde_json::Value::String(name) if !name.is_empty() => Some(name.clone()),
            serde_json::Value::Null | serde_json::Value::String(_) => {
                notices.push(
                    "llmenv: the throttle hook got a payload with no hook_event_name. Fix: run \
                     `llmenv regenerate` so the hook matches this engine."
                        .to_owned(),
                );
                None
            }
            other => {
                notices.push(format!(
                    "llmenv: the throttle hook got a hook_event_name that is not a string ({other}). \
                     Fix: run `llmenv regenerate` so the hook matches this engine."
                ));
                None
            }
        },
        Err(e) if !stdin.trim().is_empty() => {
            notices.push(format!(
                "llmenv: the throttle hook got stdin that is not valid JSON ({e}). Fix: run \
                 `llmenv regenerate` so the hook matches this engine."
            ));
            None
        }
        Err(_) => None,
    };
    let name = parsed
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| engine_event_name(event));
    if name.is_empty() {
        notices.push(format!(
            "llmenv: the throttle hook has no event name, and `{event}` is not a known event. Fix: \
             run `llmenv regenerate` so the hook command is valid."
        ));
    }
    name
}

/// The Claude Code event that runs `llmenv throttle <event>`. Empty for an unknown event.
fn engine_event_name(event: &str) -> String {
    match event {
        "prompt" => "UserPromptSubmit",
        "pre-tool" => "PreToolUse",
        _ => "",
    }
    .to_string()
}

/// Log each notice, then write the budget note and the notices as one hook context line.
fn write_output(hook_event_name: &str, note: &str, notices: &[String]) {
    for notice in notices {
        tracing::error!("{notice}");
    }
    let text = [note.to_string()]
        .into_iter()
        .chain(notices.iter().cloned())
        .chain(crate::agent_notice::current())
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    if text.is_empty() {
        return;
    }
    let out = crate::adapter::active_adapter().emit_hook_context(hook_event_name, "", &text);
    if out.is_empty() {
        tracing::error!("throttle hook {hook_event_name:?} has no channel to the agent: {text}");
    } else if let Err(e) = write_hook_context(&mut std::io::stdout(), &out) {
        tracing::error!("llmenv throttle: failed to write hook output: {e}");
    }
}

/// Apply the delay, and return the budget note for a `prompt` event (empty otherwise).
fn run_throttle_inner(event: &str) -> anyhow::Result<String> {
    let state_dir = crate::paths::state_dir()?;
    let path = throttle_state_path(&state_dir);

    let Some(cfg) = read_throttle_config(&path)? else {
        return Ok(String::new()); // No state file = throttling off.
    };

    let Some(backend) = backend_for(&cfg) else {
        return Ok(String::new());
    };

    let snapshot = backend::fetch_cached(backend.as_ref(), &state_dir, &cfg)?;

    let delay = compute_delay(&snapshot, &cfg);
    if delay > Duration::ZERO {
        eprintln!(
            "llmenv throttle: remaining={:?}, sleeping {}s",
            snapshot.remaining,
            delay.as_secs()
        );
        std::thread::sleep(delay);
    }

    Ok(if event == "prompt" {
        budget_note(&snapshot, &cfg)
    } else {
        String::new()
    })
}

/// Write the hook context line. A closed reader (`BrokenPipe`) is expected, so it is not an error.
/// Any other write error is returned for the caller to log.
fn write_hook_context(writer: &mut impl std::io::Write, out: &str) -> std::io::Result<()> {
    match writeln!(writer, "{out}") {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => {
            tracing::debug!("throttle: reader closed before the hook context was written");
            Ok(())
        }
        Err(e) => Err(e),
    }
}

/// One-line budget note for the prompt event's `additionalContext`.
fn budget_note(snapshot: &UsageSnapshot, cfg: &Throttle) -> String {
    match (snapshot.remaining, snapshot.limit) {
        (Some(remaining), Some(limit)) => {
            if remaining < cfg.soft_threshold {
                format!(
                    "Throttle: {remaining}/{limit} requests remaining \
                     (below soft cap of {}; delays active).",
                    cfg.soft_threshold
                )
            } else {
                let calls_before_soft = remaining - cfg.soft_threshold;
                format!(
                    "Throttle: {remaining}/{limit} requests remaining in window. \
                     {calls_before_soft} call(s) before soft cap."
                )
            }
        }
        (Some(remaining), None) => {
            format!("Throttle: {remaining} requests remaining in window.")
        }
        _ => String::new(),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests;
