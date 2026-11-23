//! Circuit breaker for recall calls (#3896).
//!
//! Each hook runs as its own process, so the failure count lives in a state file. After
//! `FAILURE_THRESHOLD` failed recalls in a row, the breaker opens for `OPEN_FOR`, and recall
//! is skipped without a network call. The first call after the window tries again. A
//! success closes the breaker at once.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::Context;
use serde::{Deserialize, Serialize};

use crate::agent_notice;
use crate::hook_run::action::Reply;

/// Consecutive failed recalls that open the breaker. Each failure waits out one call timeout,
/// so three bound the cost of a dead server to three waits per window.
const FAILURE_THRESHOLD: u32 = 3;

/// How long an open breaker skips recall. Five minutes covers a burst of hooks, and a server
/// that comes back is retried within the same session.
const OPEN_FOR: Duration = Duration::from_secs(300);

const STATE_FILE: &str = "recall-breaker.json";

/// The persisted breaker. `open_until_unix` is zero when the breaker is closed.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct State {
    consecutive_failures: u32,
    open_until_unix: u64,
}

/// Returned in place of a recall call while the breaker is open. The caller counts it and
/// does not print a per-hook failure.
#[derive(Debug, thiserror::Error)]
#[error("memory recall is skipped while the breaker is open")]
pub(crate) struct Skipped;

/// The state file, in the llmenv state directory.
///
/// # Errors
/// Returns an error when the state directory cannot be resolved.
fn state_path() -> anyhow::Result<PathBuf> {
    Ok(llmenv_paths::state_dir()
        .context("cannot find the llmenv state directory for the recall breaker")?
        .join(STATE_FILE))
}

/// Reads the state. A missing file means a closed breaker. A corrupt file also yields the
/// closed state, with a warning, so one bad file cannot switch recall off for good.
fn load(path: &Path) -> State {
    match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|e| {
            tracing::warn!(path = %path.display(), error = %e, "recall breaker state is corrupt; treating it as closed");
            State::default()
        }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => State::default(),
        Err(e) => {
            tracing::warn!(path = %path.display(), error = %e, "recall breaker state is unreadable; treating it as closed");
            State::default()
        }
    }
}

/// Writes the state through a temp file, so a reader never sees a half-written file.
///
/// # Errors
/// Returns an error when the file cannot be written or renamed into place.
fn save(path: &Path, state: &State) -> anyhow::Result<()> {
    let tmp = path.with_extension("json.tmp");
    let bytes = serde_json::to_vec(state).context("cannot encode the recall breaker state")?;
    std::fs::write(&tmp, bytes)
        .with_context(|| format!("cannot write recall breaker state to {}", tmp.display()))?;
    std::fs::rename(&tmp, path)
        .with_context(|| format!("cannot move recall breaker state into {}", path.display()))
}

/// True while the breaker skips recall. The window ends at `open_until_unix`, exclusive.
fn is_open(state: &State, now: u64) -> bool {
    now < state.open_until_unix
}

/// The state after one recall outcome at time `now`.
fn after(state: &State, ok: bool, now: u64) -> State {
    if ok {
        return State::default();
    }
    let failures = state.consecutive_failures.saturating_add(1);
    if failures < FAILURE_THRESHOLD {
        return State {
            consecutive_failures: failures,
            open_until_unix: state.open_until_unix,
        };
    }
    State {
        consecutive_failures: 0,
        open_until_unix: now.saturating_add(OPEN_FOR.as_secs()),
    }
}

/// Current time in whole seconds since the Unix epoch.
///
/// # Errors
/// Returns an error when the system clock is before the epoch.
fn now_unix() -> anyhow::Result<u64> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before the Unix epoch")?
        .as_secs())
}

/// Runs one recall call under the breaker. While the breaker is open, `call` does not run and
/// the result is `Skipped`. Otherwise `call` runs, and its outcome moves the breaker.
///
/// A state file that cannot be read or written does not stop recall. The problem is logged,
/// and the call still runs without the breaker.
///
/// # Errors
/// Returns `Skipped` while the breaker is open. Otherwise returns the error from `call`.
async fn run_guarded<F>(path: &Path, call: F) -> anyhow::Result<Reply>
where
    F: Future<Output = anyhow::Result<Reply>>,
{
    let now = now_unix()?;
    let before = load(path);
    if is_open(&before, now) {
        return Err(Skipped.into());
    }
    let result = call.await;
    let next = after(&before, result.is_ok(), now);
    let saved = (next != before).then(|| save(path, &next));
    if let Some(Err(e)) = saved {
        tracing::warn!(error = %format!("{e:#}"), "recall breaker state was not saved");
    }
    if is_open(&next, now) && !is_open(&before, now) {
        tracing::error!("recall breaker opened after {FAILURE_THRESHOLD} failed recall calls");
        agent_notice::record(format!(
            "llmenv: memory recall is skipped for {} seconds after {FAILURE_THRESHOLD} failed \
             calls in a row. Fix: run `llmenv doctor` to check the memory server.",
            OPEN_FOR.as_secs()
        ));
    }
    result
}

/// The state file for a hook's recall client. A state path that cannot be resolved turns the
/// breaker off, with a warning, so recall still runs.
pub(crate) fn default_path() -> Option<PathBuf> {
    match state_path() {
        Ok(path) => Some(path),
        Err(e) => {
            tracing::warn!(error = %format!("{e:#}"), "recall breaker is off for this client");
            None
        }
    }
}

/// Runs one recall under the breaker when `path` is set. With no path, `call` runs unguarded.
///
/// # Errors
/// Returns `Skipped` while the breaker is open. Otherwise returns the error from `call`.
pub(crate) async fn guard_recall<F>(path: Option<&Path>, call: F) -> anyhow::Result<Reply>
where
    F: Future<Output = anyhow::Result<Reply>>,
{
    match path {
        Some(path) => run_guarded(path, call).await,
        None => call.await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_success_clears_the_failure_count() {
        let failing = State {
            consecutive_failures: 2,
            open_until_unix: 0,
        };
        assert_eq!(after(&failing, true, 100), State::default());
    }

    #[test]
    fn failures_below_the_threshold_do_not_open_the_breaker() {
        let once = after(&State::default(), false, 100);
        assert_eq!(once.consecutive_failures, 1);
        assert!(!is_open(&once, 100));
        let twice = after(&once, false, 101);
        assert_eq!(twice.consecutive_failures, 2);
        assert!(!is_open(&twice, 101));
    }

    #[test]
    fn the_threshold_failure_opens_the_breaker_for_the_window() -> anyhow::Result<()> {
        let mut state = State::default();
        for _ in 0..FAILURE_THRESHOLD {
            state = after(&state, false, 1_000);
        }
        assert!(is_open(&state, 1_000));
        assert!(is_open(&state, 1_000 + OPEN_FOR.as_secs() - 1));
        assert!(!is_open(&state, 1_000 + OPEN_FOR.as_secs()));
        assert_eq!(state.open_until_unix, 1_000 + OPEN_FOR.as_secs());
        Ok(())
    }

    #[test]
    fn a_missing_state_file_is_a_closed_breaker() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        assert_eq!(load(&dir.path().join("absent.json")), State::default());
        Ok(())
    }

    #[test]
    fn a_corrupt_state_file_is_a_closed_breaker() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("bad.json");
        std::fs::write(&path, b"{not json")?;
        assert_eq!(load(&path), State::default());
        Ok(())
    }

    #[test]
    fn saved_state_loads_back_unchanged() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("state.json");
        let state = State {
            consecutive_failures: 1,
            open_until_unix: 42,
        };
        save(&path, &state)?;
        assert_eq!(load(&path), state);
        Ok(())
    }

    #[tokio::test]
    async fn the_open_breaker_skips_the_call_without_running_it() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("b.json");
        for _ in 0..FAILURE_THRESHOLD {
            let down = run_guarded(&path, async { Err(anyhow::anyhow!("server down")) }).await;
            assert!(down.is_err());
        }
        let mut ran = false;
        let skipped = run_guarded(&path, async {
            ran = true;
            Ok(Reply::Text(String::new()))
        })
        .await;
        let err = skipped
            .err()
            .ok_or_else(|| anyhow::anyhow!("expected a skip"))?;
        assert!(err.is::<Skipped>());
        assert!(!ran, "the call must not run while the breaker is open");
        Ok(())
    }

    #[tokio::test]
    async fn a_success_resets_the_failure_run() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("b.json");
        for _ in 0..FAILURE_THRESHOLD - 1 {
            let _ = run_guarded(&path, async { Err(anyhow::anyhow!("down")) }).await;
        }
        let ok = run_guarded(&path, async { Ok(Reply::Text(String::new())) }).await;
        assert!(ok.is_ok());
        assert_eq!(load(&path), State::default());
        Ok(())
    }
}
