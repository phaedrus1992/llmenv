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
use crate::hook_run::recall_deadline::{Deadline, within};

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

/// Returned in place of a recall call while the breaker is open. The caller counts it as
/// neither a success nor a failure, and the breaker printed its own notice when it opened.
#[derive(Debug, thiserror::Error)]
#[error("memory recall is skipped while the breaker is open")]
pub(crate) struct Skipped;

/// The state file for a hook's recall client. A state directory that cannot be resolved turns
/// the breaker off, with a notice, so recall still runs.
pub(crate) fn default_path() -> Option<PathBuf> {
    match llmenv_paths::state_dir() {
        Ok(dir) => Some(dir.join(STATE_FILE)),
        Err(e) => {
            agent_notice::record(notice_problem(
                "is off, because the state directory cannot be found",
                &e,
            ));
            None
        }
    }
}

/// Reads the state. A missing file means a closed breaker. A corrupt file, an unreadable file,
/// or a window that ends further away than `OPEN_FOR` is an error, so the caller can report it.
fn load(path: &Path, now: u64) -> anyhow::Result<State> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(State::default()),
        Err(e) => {
            return Err(e).with_context(|| format!("cannot read {}", path.display()));
        }
    };
    let state: State = serde_json::from_slice(&bytes)
        .with_context(|| format!("{} is not a valid breaker state", path.display()))?;
    if state.open_until_unix > now.saturating_add(OPEN_FOR.as_secs()) {
        anyhow::bail!(
            "{} holds a window longer than {} s",
            path.display(),
            OPEN_FOR.as_secs()
        );
    }
    Ok(state)
}

/// Writes the state. A failed write leaves the old file in place, and the next call tries again.
///
/// # Errors
/// Returns an error when the state cannot be encoded or the file cannot be written.
fn save(path: &Path, state: &State) -> anyhow::Result<()> {
    let bytes = serde_json::to_vec(state).context("cannot encode the recall breaker state")?;
    std::fs::write(path, bytes).with_context(|| {
        format!(
            "cannot write the recall breaker state to {}",
            path.display()
        )
    })
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

/// The agent notice for a breaker state problem, with the fix. The caller records it.
fn notice_problem(what: &str, error: &anyhow::Error) -> String {
    tracing::error!(error = %format!("{error:#}"), "recall breaker {what}");
    format!(
        "llmenv: the memory recall breaker {what}: {error:#}. Fix: run `llmenv doctor` to check \
         the state directory."
    )
}

/// Runs one recall call under the breaker. While the breaker is open, `call` does not run and
/// the result is `Skipped`. Otherwise `call` runs, and its outcome moves the breaker.
///
/// A state file that cannot be read, written, or trusted does not stop recall. The problem is
/// reported to the agent, and the call runs with a closed breaker.
///
/// # Errors
/// Returns `Skipped` while the breaker is open. Otherwise returns the error from `call`.
async fn run_guarded<F>(path: &Path, deadline: Option<Deadline>, call: F) -> anyhow::Result<Reply>
where
    F: Future<Output = anyhow::Result<Reply>>,
{
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before the Unix epoch")?
        .as_secs();
    let before = load(path, now).unwrap_or_else(|e| {
        agent_notice::record(notice_problem(
            "cannot be read, so recall runs without it",
            &e,
        ));
        State::default()
    });
    if is_open(&before, now) {
        return Err(Skipped.into());
    }
    // The deadline sits inside the guard, so a call that the budget cuts off still counts as a
    // failed call (#4022).
    let result = within(deadline, call).await;
    let next = after(&before, result.is_ok(), now);
    let saved = (next != before).then(|| save(path, &next));
    if let Some(Err(e)) = saved {
        agent_notice::record(notice_problem("was not saved", &e));
    }
    if let Some(notice) = trip_notice(&before, &next, now) {
        tracing::error!("recall breaker opened after {FAILURE_THRESHOLD} failed recall calls");
        agent_notice::record(notice);
    }
    result
}

/// The notice for the call that opened the window, or `None` for any other call.
fn trip_notice(before: &State, after: &State, now: u64) -> Option<String> {
    (is_open(after, now) && !is_open(before, now)).then(|| {
        format!(
            "llmenv: memory recall is skipped for {} seconds after {FAILURE_THRESHOLD} failed \
             calls in a row. Fix: run `llmenv doctor` to check the memory server.",
            OPEN_FOR.as_secs()
        )
    })
}

/// Runs one recall under the breaker when `path` is set. With no path, `call` runs unguarded.
///
/// # Errors
/// Returns `Skipped` while the breaker is open. Otherwise returns the error from `call`.
pub(crate) async fn guard_recall<F>(
    path: Option<&Path>,
    deadline: Option<Deadline>,
    call: F,
) -> anyhow::Result<Reply>
where
    F: Future<Output = anyhow::Result<Reply>>,
{
    match path {
        Some(path) => run_guarded(path, deadline, call).await,
        None => within(deadline, call).await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

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
    fn the_threshold_failure_opens_the_breaker_for_the_window() {
        let mut state = State::default();
        for _ in 0..FAILURE_THRESHOLD {
            state = after(&state, false, 1_000);
        }
        assert!(is_open(&state, 1_000));
        assert!(is_open(&state, 1_000 + OPEN_FOR.as_secs() - 1));
        assert!(!is_open(&state, 1_000 + OPEN_FOR.as_secs()));
        assert_eq!(state.open_until_unix, 1_000 + OPEN_FOR.as_secs());
    }

    #[test]
    fn a_missing_state_file_is_a_closed_breaker() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        assert_eq!(load(&dir.path().join("absent.json"), 0)?, State::default());
        Ok(())
    }

    #[test]
    fn a_corrupt_state_file_is_an_error() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("bad.json");
        std::fs::write(&path, b"{not json")?;
        assert!(load(&path, 0).is_err());
        Ok(())
    }

    #[test]
    fn a_window_past_the_longest_open_time_is_an_error() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("far.json");
        save(
            &path,
            &State {
                consecutive_failures: 0,
                open_until_unix: u64::MAX,
            },
        )?;
        assert!(load(&path, 1_000).is_err());
        Ok(())
    }

    #[tokio::test]
    async fn the_open_breaker_skips_the_call_without_running_it() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("b.json");
        for _ in 0..FAILURE_THRESHOLD {
            let down =
                run_guarded(&path, None, async { Err(anyhow::anyhow!("server down")) }).await;
            assert!(down.is_err());
        }
        let mut ran = false;
        let skipped = run_guarded(&path, None, async {
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

    // #4022: a server that is slower than the budget on every call opens the breaker.
    #[tokio::test]
    async fn a_call_the_budget_cuts_off_counts_as_a_failed_call() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("b.json");
        for _ in 0..FAILURE_THRESHOLD {
            let deadline = Deadline::expiring_in(Duration::from_millis(10));
            let slow = async {
                tokio::time::sleep(Duration::from_secs(5)).await;
                Ok(Reply::Text(String::new()))
            };
            let cut = run_guarded(&path, Some(deadline), slow).await;
            let err = cut.err().ok_or_else(|| anyhow::anyhow!("expected a cut"))?;
            assert!(err.is::<crate::hook_run::recall_deadline::Cut>(), "{err}");
        }
        let skipped = run_guarded(&path, None, async { Ok(Reply::Text(String::new())) }).await;
        let err = skipped
            .err()
            .ok_or_else(|| anyhow::anyhow!("expected a skip"))?;
        assert!(err.is::<Skipped>(), "the breaker must be open: {err}");
        Ok(())
    }

    #[tokio::test]
    async fn a_success_resets_the_failure_run() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("b.json");
        for _ in 0..FAILURE_THRESHOLD - 1 {
            let _ = run_guarded(&path, None, async { Err(anyhow::anyhow!("down")) }).await;
        }
        let ok = run_guarded(&path, None, async { Ok(Reply::Text(String::new())) }).await;
        assert!(ok.is_ok());
        assert_eq!(load(&path, 0)?, State::default());
        Ok(())
    }

    proptest! {
        /// Saving then loading returns the same state for any window inside the open limit.
        #[test]
        fn saved_state_loads_back_for_any_valid_state(
            failures in any::<u32>(),
            window in 0u64..=OPEN_FOR.as_secs(),
        ) {
            let fail = |e: anyhow::Error| TestCaseError::fail(format!("{e:#}"));
            let dir = tempfile::tempdir().map_err(|e| fail(e.into()))?;
            let path = dir.path().join("state.json");
            let state = State { consecutive_failures: failures, open_until_unix: window };
            save(&path, &state).map_err(fail)?;
            prop_assert_eq!(load(&path, 0).map_err(fail)?, state);
        }

        /// Over any run of outcomes, the count stays below the threshold while the breaker is
        /// closed, an open window holds no failure count, and a success clears the state.
        #[test]
        fn outcomes_keep_the_breaker_consistent(
            outcomes in proptest::collection::vec(any::<bool>(), 0..40),
        ) {
            let mut state = State::default();
            for (step, ok) in outcomes.into_iter().enumerate() {
                let now = 1_000 + step as u64;
                // A call inside an open window is skipped, so its outcome never reaches `after`.
                if is_open(&state, now) {
                    prop_assert_eq!(state.consecutive_failures, 0);
                    continue;
                }
                state = after(&state, ok, now);
                prop_assert!(state.consecutive_failures < FAILURE_THRESHOLD);
                if ok {
                    prop_assert_eq!(state.clone(), State::default());
                }
            }
        }
    }
}

#[cfg(test)]
mod trip_notice_tests {
    use super::*;

    #[test]
    fn the_trip_notice_is_only_for_the_call_that_opens_the_window() {
        let closed = State::default();
        let one_failure = after(&closed, false, 100);
        assert!(trip_notice(&closed, &one_failure, 100).is_none());
        let mut opened = State::default();
        for _ in 0..FAILURE_THRESHOLD {
            opened = after(&opened, false, 100);
        }
        let notice = trip_notice(&closed, &opened, 100);
        assert!(notice.is_some_and(|text| text.contains("after 3 failed calls in a row")));
        assert!(trip_notice(&opened, &opened, 101).is_none());
    }
}

#[cfg(test)]
mod notice_text_tests {
    use super::*;

    #[test]
    fn a_problem_notice_names_the_state_the_cause_and_the_fix() {
        let text = notice_problem("cannot be read", &anyhow::anyhow!("permission denied"));
        assert!(text.contains("cannot be read"), "{text}");
        assert!(text.contains("permission denied"), "{text}");
        assert!(text.contains("llmenv doctor"), "{text}");
    }
}

#[cfg(test)]
mod path_and_read_tests {
    use super::*;

    #[test]
    fn the_default_path_is_the_state_file_in_the_state_directory() {
        assert!(default_path().is_some_and(|p| p.ends_with(STATE_FILE)));
    }

    #[test]
    fn a_read_error_other_than_a_missing_file_is_reported() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        // A directory is not a readable state file, and the error is not a missing file.
        assert!(load(dir.path(), 0).is_err());
        Ok(())
    }
}
