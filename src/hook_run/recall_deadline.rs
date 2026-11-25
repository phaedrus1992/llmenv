//! The time budget for the recall calls of one hook run (#3899, #4023).
//!
//! Each recall call waited out its own timeout, so one hook run could take over a minute when
//! the memory server was slow. The budget bounds the whole recall chain. A call that misses the
//! deadline is dropped, and the agent hears about it.

use std::future::Future;
use std::time::Duration;

use tokio::time::Instant;

mod cut;

pub(crate) use cut::Cut;

/// A hook flow that bounds the wall time of its recall calls.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Flow {
    SessionStart,
    TurnStart,
    SubagentStart,
}

impl Flow {
    fn name(self) -> &'static str {
        match self {
            Self::SessionStart => "session start",
            Self::TurnStart => "turn start",
            Self::SubagentStart => "subagent start",
        }
    }

    /// The total wall time that the recall calls of one run of the flow may take.
    /// A healthy local server answers in milliseconds, so a few seconds covers a slow server.
    /// The values are fixed for now, because no config key exists for them yet.
    fn budget(self) -> Duration {
        match self {
            Self::SessionStart => Duration::from_secs(5),
            // The prompt waits for this hook, so its bound is shorter than the session start one.
            Self::TurnStart => Duration::from_secs(3),
            // The subagent waits for this hook before it starts, as a prompt does.
            Self::SubagentStart => Duration::from_secs(3),
        }
    }
}

/// The instant when the budget of a flow runs out.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Deadline {
    at: Instant,
    flow: Flow,
}

impl Deadline {
    /// The deadline for a session start that begins now.
    pub(crate) fn session_start() -> Self {
        Self::new(Flow::SessionStart)
    }

    /// The deadline for a prompt that begins now.
    pub(crate) fn turn_start() -> Self {
        Self::new(Flow::TurnStart)
    }

    /// The deadline for a subagent start that begins now.
    pub(crate) fn subagent_start() -> Self {
        Self::new(Flow::SubagentStart)
    }

    fn new(flow: Flow) -> Self {
        Self {
            at: Instant::now() + flow.budget(),
            flow,
        }
    }

    /// True once the deadline has passed.
    pub(crate) fn has_passed(self) -> bool {
        Instant::now() >= self.at
    }

    /// The cut error when the deadline passed before the call began. Such a call never
    /// reaches the server, so it says nothing about the server.
    pub(crate) fn expired_cut(self) -> Option<Cut> {
        self.has_passed().then(|| self.cut())
    }

    /// The notice for a recall that the budget cut off.
    pub(crate) fn cut_notice(self) -> String {
        format!(
            "llmenv: memory recall was skipped, because the {} recall budget of {} s ran out. \
             Fix: run `llmenv doctor` to check the memory server.",
            self.flow.name(),
            self.flow.budget().as_secs()
        )
    }

    /// A session start deadline that runs out after `wait`, for tests that need a short budget.
    #[cfg(test)]
    pub(crate) fn expiring_in(wait: Duration) -> Self {
        Self {
            at: Instant::now() + wait,
            flow: Flow::SessionStart,
        }
    }

    fn cut(self) -> Cut {
        Cut::new(self.flow.name(), self.flow.budget().as_secs())
    }
}

/// Runs `call` until `deadline`. With no deadline, `call` runs unbounded. A call that misses the
/// deadline is dropped, and it fails with `Cut`.
pub(crate) async fn within<T>(
    deadline: Option<Deadline>,
    call: impl Future<Output = anyhow::Result<T>>,
) -> anyhow::Result<T> {
    let Some(deadline) = deadline else {
        return call.await;
    };
    match tokio::time::timeout_at(deadline.at, call).await {
        Ok(result) => result,
        Err(_) => Err(deadline.cut().into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_call_that_misses_the_deadline_fails_with_the_budget_error() -> anyhow::Result<()> {
        let deadline = Deadline {
            at: Instant::now() + Duration::from_millis(20),
            flow: Flow::SessionStart,
        };
        let slow = async {
            tokio::time::sleep(Duration::from_secs(2)).await;
            Ok::<_, anyhow::Error>(1)
        };
        let err = within(Some(deadline), slow)
            .await
            .err()
            .ok_or_else(|| anyhow::anyhow!("a call past the deadline must fail"))?;
        assert_eq!(
            err.to_string(),
            "the session start recall budget of 5 s ran out"
        );
        assert!(err.is::<Cut>());
        Ok(())
    }

    #[tokio::test]
    async fn a_call_that_finishes_in_time_keeps_its_value() -> anyhow::Result<()> {
        let value = within(Some(Deadline::turn_start()), async {
            Ok::<_, anyhow::Error>(7)
        })
        .await?;
        assert_eq!(value, 7);
        Ok(())
    }

    #[tokio::test]
    async fn no_deadline_runs_the_call_unbounded() -> anyhow::Result<()> {
        let value = within(None, async { Ok::<_, anyhow::Error>("done") }).await?;
        assert_eq!(value, "done");
        Ok(())
    }

    #[test]
    fn each_flow_names_its_own_budget_in_the_notice() {
        let start = Deadline::session_start().cut_notice();
        assert!(
            start.contains("session start recall budget of 5 s"),
            "{start}"
        );
        let turn = Deadline::turn_start().cut_notice();
        assert!(turn.contains("turn start recall budget of 3 s"), "{turn}");
        assert!(turn.contains("llmenv doctor"), "{turn}");
    }

    #[test]
    fn the_subagent_start_notice_names_its_own_budget() {
        let text = Deadline::subagent_start().cut_notice();
        assert!(
            text.contains("subagent start recall budget of 3 s"),
            "{text}"
        );
    }

    #[test]
    fn a_deadline_in_the_past_has_passed_and_a_new_one_has_not() {
        assert!(!Deadline::turn_start().has_passed());
        let Some(earlier) = Instant::now().checked_sub(Duration::from_secs(1)) else {
            return;
        };
        let past = Deadline {
            at: earlier,
            flow: Flow::TurnStart,
        };
        assert!(past.has_passed());
    }
}
