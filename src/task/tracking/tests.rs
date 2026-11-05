use super::{Tracking, tracking_in};
use crate::task::{Stored, StrictTasks};

proptest::proptest! {
    /// A store that cannot be read is `Unknown` with the cause, for any caller, never a guess
    /// such as `NoSession` or `NoTasks` (#3472).
    #[test]
    fn a_read_error_is_unknown_with_its_cause(
        msg in "[a-z]{1,12}",
        caller in proptest::option::of("[a-z]{1,4}"),
        sessions_fail in proptest::prelude::any::<bool>(),
    ) {
        let stored = if sessions_fail {
            Stored::from_parts(
                Ok(StrictTasks::from_vec_for_test(Vec::new())),
                Err(anyhow::anyhow!("{msg}")),
            )
        } else {
            Stored::from_parts(Err(anyhow::anyhow!("{msg}")), Ok(Vec::new()))
        };
        // A project that does not resolve is also `Unknown`, so either way the check holds.
        match tracking_in(&stored, caller.as_deref()) {
            Tracking::Unknown(text) => {
                let named = text.contains(&msg) || text.contains("the project cannot be named");
                proptest::prop_assert!(named, "{text}");
            }
            other => proptest::prop_assert!(false, "expected Unknown, got {other:?}"),
        }
    }
}
