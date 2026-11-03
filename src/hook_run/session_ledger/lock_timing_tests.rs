//! Lock timing of the ledger store: a busy lock, a skipped update, and a free lock.

use crate::hook_run::session_ledger::tests::store;
use crate::hook_run::session_state::LOCK_ATTEMPTS;

// A busy lock gives no update, but the timing still says how long it polled and that the
// state is unsaved (#3012).
#[test]
fn a_busy_lock_reports_its_poll_count_and_an_unsaved_state() {
    let (_dir, store) = store();
    let held = store.lock("s1").unwrap();
    let (updated, timing) = store.update_reporting_save("s1", |l| l.last_turn_at = 1);
    assert!(updated.is_none());
    assert!(timing.lock_attempts.unwrap() > 1);
    assert_eq!(timing.state_unsaved, Some(true));
    assert!(timing.read_us.is_none() && timing.write_us.is_none());
    drop(held);
    assert_eq!(
        store.load("s1").unwrap().last_turn_at,
        0,
        "a busy lock changes nothing"
    );
}

// `update_timed` gives the same result as `update`, and keeps the timing when the lock is busy.
#[test]
fn update_timed_keeps_the_timing_of_a_skipped_update() {
    let (_dir, store) = store();
    let held = store.lock("s1").unwrap();
    let (skipped, timing) = store.update_timed("s1", |l| l.last_turn_at = 1);
    assert!(skipped.is_none());
    assert!(timing.lock_attempts.unwrap() > 1);
    drop(held);
    let (done, timing) = store.update_timed("s1", |l| l.last_turn_at = 2);
    assert_eq!(done, Some(()));
    // A child process from a parallel test can hold the lock file descriptor for a short time,
    // so the free lock may take a second poll. The exact first-poll count is asserted in
    // `a_free_lock_is_taken_on_the_first_poll`. This check keeps the budget bound.
    let attempts = timing.lock_attempts.unwrap();
    assert!(
        (1..=LOCK_ATTEMPTS).contains(&attempts),
        "took {attempts} polls"
    );
}

// An uncontended update takes the lock on the first poll and reaches disk.
#[test]
fn a_free_lock_is_taken_on_the_first_poll() {
    let (_dir, store) = store();
    let (updated, timing) = store.update_reporting_save("s1", |l| l.last_turn_at = 7);
    assert!(matches!(updated, Some(((), true))));
    assert_eq!(timing.lock_attempts, Some(1));
    assert_eq!(timing.state_unsaved, Some(false));
    assert!(timing.read_us.is_some() && timing.write_us.is_some());
    assert!(timing.lock_wait_us.is_some());
    assert_eq!(store.load("s1").unwrap().last_turn_at, 7);
}
