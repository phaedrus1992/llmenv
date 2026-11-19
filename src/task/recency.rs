//! Picks the most recently updated task from a set.

use crate::task::Task;
use crate::util::time::RecordId;

/// The most recently updated of `tasks`, by the instant of `updated_at` (see `cmp_instants`, not
/// text order, #3796). Ties resolve to `max_by`'s last-element-wins rule. Callers that need a
/// stable tiebreak sort first (see `append_forest`).
pub(super) fn most_recently_updated<'a>(tasks: impl Iterator<Item = &'a Task>) -> Option<&'a Task> {
    let tasks: Vec<&Task> = tasks.collect();
    let winner = tasks.iter().copied().max_by(|a, b| {
        crate::util::time::cmp_instants(
            &a.updated_at,
            RecordId(&a.slug),
            &b.updated_at,
            RecordId(&b.slug),
            "task updated_at",
        )
    })?;
    // Ties by instant, not by text: a bad value sorts equal to every other bad value (#3851).
    let shared = tasks
        .iter()
        .filter(|t| {
            crate::util::time::cmp_instants(
                &t.updated_at,
                RecordId(&t.slug),
                &winner.updated_at,
                RecordId(&winner.slug),
                "task updated_at",
            )
            .is_eq()
        })
        .count();
    crate::util::time::warn_if_bad_max_winner(
        "task updated_at",
        RecordId(&winner.slug),
        &winner.updated_at,
        shared,
    );
    Some(winner)
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "a test fails loudly when its fixture is wrong"
)]
mod tests {
    use proptest::prelude::*;

    use super::most_recently_updated;
    use crate::task::Task;

    fn task(slug: &str, updated_at: &str) -> Task {
        let text = format!(
            r#"{{"slug":"{slug}","title":"t","created_at":"2026-01-01T00:00:00Z","updated_at":"{updated_at}"}}"#
        );
        serde_json::from_str(&text).expect("test fixture is valid task JSON")
    }

    #[test]
    fn no_tasks_gives_no_winner() {
        assert!(most_recently_updated(std::iter::empty::<&Task>()).is_none());
    }

    #[test]
    fn a_valid_timestamp_beats_a_bad_one() {
        let tasks = [
            task("bad", "not-a-time"),
            task("ok", "2026-02-01T00:00:00Z"),
        ];
        let winner = most_recently_updated(tasks.iter()).expect("two tasks");
        assert_eq!(winner.slug, "ok");
    }

    proptest! {
        // Over mixed valid and junk values, the winner holds the newest parseable instant. A
        // junk value never wins over a valid one, and an empty set has no winner.
        #[test]
        fn the_winner_holds_the_newest_parseable_instant(
            stamps in proptest::collection::vec(
                prop_oneof![
                    (0u32..60).prop_map(|s| format!("2026-01-01T00:00:{s:02}Z")),
                    "[a-z]{1,6}",
                ],
                0..6,
            ),
        ) {
            let tasks: Vec<Task> = stamps
                .iter()
                .enumerate()
                .map(|(i, s)| task(&format!("t{i}"), s))
                .collect();
            let winner = most_recently_updated(tasks.iter());
            prop_assert_eq!(winner.is_none(), tasks.is_empty());
            let newest = stamps
                .iter()
                .filter_map(|s| s.parse::<jiff::Timestamp>().ok())
                .max();
            if let (Some(winner), Some(newest)) = (winner, newest) {
                prop_assert_eq!(winner.updated_at.parse::<jiff::Timestamp>().ok(), Some(newest));
            }
        }
    }

    #[test]
    fn a_bad_value_still_gives_a_winner_when_every_value_is_bad() {
        let tasks = [task("a", "junk"), task("b", "junk")];
        let winner = most_recently_updated(tasks.iter()).expect("two tasks");
        assert!(
            winner.slug == "a" || winner.slug == "b",
            "got {}",
            winner.slug
        );
    }
}
