//! The time budget for the recall calls of one `SessionStart` (#3899).
//!
//! Each recall call waited out its own timeout, so one start could take over a minute when the
//! memory server was slow. The budget bounds the whole recall chain. A call that misses the
//! deadline is dropped, and the agent hears about it.

use std::future::Future;
use std::time::Duration;

use tokio::time::Instant;

/// The total wall time that the recall calls of one session start may take.
/// A healthy local server answers in milliseconds, so a few seconds covers a slow start.
/// It is a fixed value for now, because no config key exists for it yet.
pub(crate) const DEFAULT_SESSION_START_RECALL_BUDGET: Duration = Duration::from_secs(5);

/// The instant when the session start budget runs out.
pub(super) fn session_start_deadline() -> Instant {
    Instant::now() + DEFAULT_SESSION_START_RECALL_BUDGET
}

/// The notice for a recall that the budget cut off.
pub(super) fn cut_notice() -> String {
    format!(
        "llmenv: memory recall was skipped, because the session start recall budget of {} s ran \
         out. Fix: run `llmenv doctor` to check the memory server.",
        DEFAULT_SESSION_START_RECALL_BUDGET.as_secs()
    )
}

/// Runs `call` until `deadline`. With no deadline, `call` runs unbounded. A call that misses the
/// deadline is dropped, and it fails with the budget error.
pub(super) async fn within<T>(
    deadline: Option<Instant>,
    call: impl Future<Output = anyhow::Result<T>>,
) -> anyhow::Result<T> {
    let Some(deadline) = deadline else {
        return call.await;
    };
    match tokio::time::timeout_at(deadline, call).await {
        Ok(result) => result,
        Err(_) => Err(anyhow::anyhow!(
            "the session start recall budget of {} s ran out",
            DEFAULT_SESSION_START_RECALL_BUDGET.as_secs()
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_call_that_misses_the_deadline_fails_with_the_budget_error() -> anyhow::Result<()> {
        let deadline = Some(Instant::now() + Duration::from_millis(20));
        let slow = async {
            tokio::time::sleep(Duration::from_secs(2)).await;
            Ok::<_, anyhow::Error>(1)
        };
        let err = within(deadline, slow)
            .await
            .err()
            .ok_or_else(|| anyhow::anyhow!("a call past the deadline must fail"))?;
        assert!(
            err.to_string().contains("session start recall budget"),
            "{err}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn a_call_that_finishes_in_time_keeps_its_value() -> anyhow::Result<()> {
        let deadline = Some(Instant::now() + Duration::from_secs(5));
        let value = within(deadline, async { Ok::<_, anyhow::Error>(7) }).await?;
        assert_eq!(value, 7);
        Ok(())
    }

    #[tokio::test]
    async fn no_deadline_runs_the_call_unbounded() -> anyhow::Result<()> {
        let value = within(None, async { Ok::<_, anyhow::Error>("done") }).await?;
        assert_eq!(value, "done");
        Ok(())
    }
}
