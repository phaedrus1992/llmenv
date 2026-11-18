//! The queue order of tasks: creation time, then slug. The tree listing, the next-task lookup,
//! and `tracking::open` all sort with it, so they agree on which task is next.

use super::Task;

pub fn queue_order(a: &Task, b: &Task) -> std::cmp::Ordering {
    llmenv_util::time::cmp_instants(
        &a.created_at,
        &a.slug,
        &b.created_at,
        &b.slug,
        "task created_at",
    )
    .then_with(|| a.slug.cmp(&b.slug))
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "a test fails loudly when its fixture is wrong"
)]
mod tests {
    use super::queue_order;
    use crate::Task;
    use jiff::Timestamp;
    use proptest::prelude::*;

    /// A task created at `secs` plus `frac` tenths of a second. Equal instants and slugs from
    /// the small ranges make the tie-break run often.
    fn task_at(slug: &str, secs: i64, frac: u8) -> Task {
        let nanos = i32::from(frac) * 100_000_000;
        let at = Timestamp::new(1_767_225_600 + secs, nanos).expect("in range");
        serde_json::from_value(serde_json::json!({
            "slug": slug,
            "title": slug,
            "created_at": at.to_string(),
            "updated_at": "",
        }))
        .expect("a minimal task parses")
    }

    fn arb_task() -> impl Strategy<Value = Task> {
        ("[a-c]{1,2}", 0i64..4, 0u8..10).prop_map(|(slug, secs, frac)| task_at(&slug, secs, frac))
    }

    fn slugs(tasks: &[Task]) -> Vec<&str> {
        tasks.iter().map(|t| t.slug.as_str()).collect()
    }

    proptest! {
        // Swapping the arguments reverses the order. A sort needs this to be a valid order.
        #[test]
        fn queue_order_is_antisymmetric(a in arb_task(), b in arb_task()) {
            prop_assert_eq!(queue_order(&a, &b), queue_order(&b, &a).reverse());
        }

        // Transitivity makes one sorted order possible for any three tasks.
        #[test]
        fn queue_order_is_transitive(a in arb_task(), b in arb_task(), c in arb_task()) {
            if queue_order(&a, &b).is_le() && queue_order(&b, &c).is_le() {
                prop_assert!(queue_order(&a, &c).is_le());
            }
        }

        // The listing and the next-task lookup sort the same way, so sorting a sorted list must
        // not change it.
        #[test]
        fn sorting_by_queue_order_is_idempotent(tasks in proptest::collection::vec(arb_task(), 0..12)) {
            let mut once = tasks;
            once.sort_by(queue_order);
            let mut twice = once.clone();
            twice.sort_by(queue_order);
            prop_assert_eq!(slugs(&once), slugs(&twice));
        }

        // Two different instants order as the instants do, not as the text does (#3796).
        #[test]
        fn queue_order_follows_the_instant_when_instants_differ(a in arb_task(), b in arb_task()) {
            let at = |t: &Task| -> Timestamp { t.created_at.parse().expect("a valid instant") };
            if at(&a) != at(&b) {
                prop_assert_eq!(queue_order(&a, &b), at(&a).cmp(&at(&b)));
            }
        }
    }
}
