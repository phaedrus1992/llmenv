//! Usage throttling: injects PreToolUse and UserPromptSubmit hooks that poll a
//! backend's usage state and sleep a capped, adaptive delay to avoid rate limits.
//!
//! `run_throttle_hook(event)` is the CLI entry. `compute_delay` is pure and
//! unit-tested. `resolve_active_throttle` selects the single active config entry
//! by tag intersection (same model as memory).

mod backend;
pub use backend::{ThrottleBackend, UsageSnapshot, backend_for};

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
pub fn store_active_throttle(throttle: Option<&Throttle>) -> anyhow::Result<()> {
    let state_dir = crate::paths::state_dir()?;
    let path = throttle_state_path(&state_dir);
    match throttle {
        Some(cfg) => {
            let json = serde_json::to_string(cfg)?;
            crate::paths::write_owner_only_atomic(&path, json.as_bytes())
                .with_context(|| format!("writing throttle state: {}", path.display()))?;
        }
        None => {
            // Remove stale file; missing file = throttling off.
            let _ = std::fs::remove_file(&path);
        }
    }
    Ok(())
}

fn throttle_state_path(state_dir: &Path) -> std::path::PathBuf {
    state_dir.join("throttle.json")
}

/// Resolve the single active throttle entry by tag intersection.
///
/// Returns `None` when no entry's `when` tags intersect the active tags.
/// Returns an error when more than one entry is simultaneously active (same
/// single-active invariant as `features.memory`).
///
/// # Errors
/// Returns an error if more than one throttle entry is active.
pub fn resolve_active_throttle(
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
pub fn compute_delay(snapshot: &UsageSnapshot, cfg: &Throttle) -> Duration {
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

/// CLI entry for `llmenv throttle <event>`. Fail-soft: warns on stderr and
/// exits 0 on any error. Never panics.
pub fn run_throttle_hook(event: &str) {
    use std::io::Read;

    // Consume stdin so the pipe doesn't break, even if we don't use the body.
    let mut stdin_buf = String::new();
    if let Err(e) = std::io::stdin().read_to_string(&mut stdin_buf) {
        eprintln!("llmenv throttle: failed to read stdin: {e}");
        return;
    }

    // Parse hook_event_name for emit_hook_context (needed for prompt output).
    let hook_event_name = match serde_json::from_str::<serde_json::Value>(&stdin_buf) {
        Ok(v) => v["hook_event_name"]
            .as_str()
            .map(str::to_owned)
            .unwrap_or_default(),
        Err(e) if !stdin_buf.trim().is_empty() => {
            eprintln!("llmenv throttle: stdin is not valid JSON (budget note suppressed): {e}");
            String::new()
        }
        Err(_) => String::new(),
    };

    if let Err(e) = run_throttle_inner(event, &hook_event_name) {
        eprintln!("llmenv throttle: {e}");
    }
}

/// Detect which adapter is running this throttle hook by checking each
/// registered adapter's environment signal. Falls back to Claude Code.
fn active_adapter() -> Box<dyn crate::adapter::AgentAdapter> {
    crate::adapter::registered_adapters()
        .into_iter()
        .find(|a| match a.name() {
            "claude-code" => std::env::var("CLAUDE_CONFIG_DIR").is_ok(),
            "crush" => std::env::var("CRUSH_GLOBAL_CONFIG").is_ok(),
            _ => false,
        })
        .unwrap_or_else(|| Box::new(crate::adapter::claude_code::ClaudeCodeAdapter))
}

fn run_throttle_inner(event: &str, hook_event_name: &str) -> anyhow::Result<()> {
    let state_dir = crate::paths::state_dir()?;
    let path = throttle_state_path(&state_dir);

    let cfg: Throttle = match std::fs::read(&path) {
        Err(_) => return Ok(()), // No state file = throttling off.
        Ok(bytes) => match serde_json::from_slice(&bytes) {
            Ok(cfg) => cfg,
            Err(e) => {
                eprintln!("llmenv throttle: failed to parse throttle.json (skipping): {e}");
                return Ok(());
            }
        },
    };

    let Some(backend) = backend_for(&cfg) else {
        return Ok(());
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

    if event == "prompt" {
        let note = budget_note(&snapshot, &cfg);
        if !note.is_empty() {
            let adapter = active_adapter();
            let out = adapter.emit_hook_context(hook_event_name, &note);
            if !out.is_empty() {
                use std::io::Write;
                let _ = writeln!(std::io::stdout(), "{out}");
            }
        }
    }

    Ok(())
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
mod tests {
    use super::*;
    use crate::config::Throttle;
    use proptest::prelude::*;

    fn cfg(max_wait: u64, soft_threshold: u64) -> Throttle {
        Throttle {
            backend: "umans".to_string(),
            when: vec!["work".to_string()],
            cache_ttl: 30,
            max_wait,
            soft_threshold,
        }
    }

    fn snap(remaining: Option<u64>, penalized: bool) -> UsageSnapshot {
        UsageSnapshot {
            remaining,
            limit: Some(200),
            resets_at: None,
            penalized,
        }
    }

    #[test]
    fn penalized_returns_max_wait() {
        let d = compute_delay(&snap(Some(100), true), &cfg(300, 20));
        assert_eq!(d, Duration::from_secs(300));
    }

    #[test]
    fn penalized_ignores_boxed_until_in_snapshot() {
        // penalized flag is set by the backend, regardless of remaining
        let d = compute_delay(&snap(Some(150), true), &cfg(300, 20));
        assert_eq!(d, Duration::from_secs(300));
    }

    #[test]
    fn unknown_remaining_returns_zero() {
        let d = compute_delay(&snap(None, false), &cfg(300, 20));
        assert_eq!(d, Duration::ZERO);
    }

    #[test]
    fn remaining_zero_returns_max_wait() {
        let d = compute_delay(&snap(Some(0), false), &cfg(300, 20));
        assert_eq!(d, Duration::from_secs(300));
    }

    #[test]
    fn healthy_remaining_returns_zero() {
        let d = compute_delay(&snap(Some(50), false), &cfg(300, 20));
        assert_eq!(d, Duration::ZERO);
    }

    #[test]
    fn at_threshold_returns_zero() {
        // remaining == soft_threshold → not under threshold
        let d = compute_delay(&snap(Some(20), false), &cfg(300, 20));
        assert_eq!(d, Duration::ZERO);
    }

    #[test]
    fn just_under_threshold_returns_scaled() {
        // remaining=19, threshold=20, max_wait=300 → 300 * 1 / 20 = 15
        let d = compute_delay(&snap(Some(19), false), &cfg(300, 20));
        assert_eq!(d, Duration::from_secs(15));
    }

    #[test]
    fn remaining_one_returns_near_max() {
        // remaining=1, threshold=20, max_wait=300 → 300 * 19 / 20 = 285
        let d = compute_delay(&snap(Some(1), false), &cfg(300, 20));
        assert_eq!(d, Duration::from_secs(285));
    }

    #[test]
    fn delay_grows_as_remaining_shrinks() {
        let c = cfg(300, 20);
        let d10 = compute_delay(&snap(Some(10), false), &c);
        let d5 = compute_delay(&snap(Some(5), false), &c);
        assert!(d5 > d10);
    }

    #[test]
    fn single_active_entry_selected() {
        let entries = vec![cfg(300, 20)];
        let mut tags = BTreeSet::new();
        tags.insert("work".to_string());
        let result = resolve_active_throttle(&entries, &tags).unwrap();
        assert!(result.is_some());
    }

    #[test]
    fn no_active_entry_when_tags_inactive() {
        let entries = vec![cfg(300, 20)];
        let tags = BTreeSet::new();
        let result = resolve_active_throttle(&entries, &tags).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn multiple_active_entries_is_error() {
        let mut e2 = cfg(300, 20);
        e2.backend = "umans2".to_string();
        e2.when = vec!["work".to_string()];
        let entries = vec![cfg(300, 20), e2];
        let mut tags = BTreeSet::new();
        tags.insert("work".to_string());
        let result = resolve_active_throttle(&entries, &tags);
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("multiple entries active simultaneously"));
    }

    #[test]
    fn throttle_config_defaults() {
        let yaml = "backend: umans\nwhen: [work]";
        let t: Throttle = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(t.cache_ttl, 30);
        assert_eq!(t.max_wait, 300);
        assert_eq!(t.soft_threshold, 20);
    }

    proptest! {
        #[test]
        fn prop_compute_delay_capped(
            max_wait in 0u64..3600,
            soft_threshold in 1u64..1000,
            remaining in any::<Option<u64>>(),
            penalized in any::<bool>(),
        ) {
            let c = cfg(max_wait, soft_threshold);
            let s = snap(remaining, penalized);
            let d = compute_delay(&s, &c);
            prop_assert!(d <= Duration::from_secs(max_wait));
        }

        #[test]
        fn prop_compute_delay_monotone(
            max_wait in 1u64..3600,
            soft_threshold in 1u64..1000,
            r1 in 0u64..2000,
            r2 in 0u64..2000,
        ) {
            // Higher remaining → smaller or equal delay (non-increasing).
            let c = cfg(max_wait, soft_threshold);
            let (lo, hi) = if r1 <= r2 { (r1, r2) } else { (r2, r1) };
            let d_lo = compute_delay(&snap(Some(lo), false), &c);
            let d_hi = compute_delay(&snap(Some(hi), false), &c);
            prop_assert!(
                d_lo >= d_hi,
                "delay should be non-increasing as remaining increases: \
                 lo={lo} d={d_lo:?} hi={hi} d={d_hi:?}"
            );
        }
    }
}
