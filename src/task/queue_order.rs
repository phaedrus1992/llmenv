//! The queue order of tasks: creation time, then slug. The tree listing, the next-task lookup,
//! and `tracking::open` all sort with it, so they agree on which task is next.

use super::Task;

/// The queue order of two tasks: creation time, then slug for a tie. The tree listing and the
/// next-task lookup both use it, so they cannot disagree about which task is next.
pub(crate) fn queue_order(a: &Task, b: &Task) -> std::cmp::Ordering {
    crate::util::time::cmp_instants(
        &a.created_at,
        &a.slug,
        &b.created_at,
        &b.slug,
        "task created_at",
    )
    .then_with(|| a.slug.cmp(&b.slug))
}
