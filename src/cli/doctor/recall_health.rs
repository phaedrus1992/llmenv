//! Doctor's `Memory recall:` section (#2919). It reads the ring of recent adaptive recall calls
//! that the hooks keep, so it makes no network call.

use std::path::Path;

use super::{CheckLevel, print_check};
use crate::hook_run::recall_health::{self, Health, STUCK_RUN, WARN_FAILED_PERCENT};

/// A stuck ring older than this is old news: no recall ran since, so the server may be fixed.
const STALE_AFTER_SECS: i64 = 24 * 60 * 60;

/// The age of a call made at `at`, seen at `now`, in the largest whole unit.
fn age(at: i64, now: i64) -> String {
    let secs = now.saturating_sub(at).max(0);
    match secs {
        0..60 => format!("{secs} s ago"),
        60..3600 => format!("{} min ago", secs / 60),
        3600..86_400 => format!("{} h ago", secs / 3600),
        _ => format!("{} days ago", secs / 86_400),
    }
}

/// The Info line for a ring that no call has touched for [`STALE_AFTER_SECS`]: its failures are
/// old news (#3912). `None` when the ring holds no failure to report.
fn old_ring_line(h: &Health, last: &str) -> Option<(CheckLevel, String)> {
    let what = if h.stuck {
        format!("was stuck at the last recorded call ({last}): the last {STUCK_RUN} calls failed")
    } else if h.failing_often() {
        format!(
            "failed often at the last recorded calls ({last}): {} of {} calls failed, more than \
             {WARN_FAILED_PERCENT}%",
            h.failed, h.calls
        )
    } else {
        return None;
    };
    Some((
        CheckLevel::Info,
        format!("memory recall {what}, and no adaptive recall ran since"),
    ))
}

fn health_line(
    read: anyhow::Result<Option<Health>>,
    path: &Path,
    now: i64,
) -> (CheckLevel, String) {
    let h = match read {
        Ok(Some(h)) => h,
        Ok(None) => {
            return (
                CheckLevel::Info,
                "no adaptive recall calls recorded yet".to_string(),
            );
        }
        Err(e) => {
            return (
                CheckLevel::Warn,
                format!(
                    "cannot read the recall health file {}: {e:#}. Fix: delete the file, and \
                     the next hook writes a new one.",
                    path.display()
                ),
            );
        }
    };
    let (failed, calls, last) = (h.failed, h.calls, age(h.newest_at, now));
    // A ring from the future is not trusted as recent either, so it reads as old (#3907).
    if !crate::util::ttl::is_within(h.newest_at, now, STALE_AFTER_SECS + 1)
        && let Some(line) = old_ring_line(&h, &last)
    {
        return line;
    }
    if h.stuck {
        return (
            CheckLevel::Warn,
            format!(
                "memory recall is stuck: the last {STUCK_RUN} calls all failed ({failed} of \
                 {calls} recent calls failed, last call {last}). Fix: check the memory server \
                 above, then start a new session."
            ),
        );
    }
    if h.failing_often() {
        return (
            CheckLevel::Warn,
            format!(
                "memory recall fails often: {failed} of {calls} recent calls failed, more than \
                 {WARN_FAILED_PERCENT}% (last call {last}). Fix: check the memory server above."
            ),
        );
    }
    (
        CheckLevel::Pass,
        format!("memory recall: {failed} of {calls} recent calls failed (last call {last})"),
    )
}

pub(super) fn run_doctor_recall_health(use_color: bool, state_dir: &Path) {
    let pass = super::super::doctor_pass(use_color);
    let warn = super::super::doctor_warning(use_color);
    let info = super::super::doctor_info(use_color);
    eprintln!();
    eprintln!("Memory recall:");
    let now = crate::hook_run::session_state::unix_now();
    let line = health_line(
        recall_health::health(state_dir),
        &recall_health::path(state_dir),
        now,
    );
    print_check(line, &pass, &warn, &info);
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_000_000;

    fn line(read: anyhow::Result<Option<Health>>) -> (CheckLevel, String) {
        health_line(read, Path::new("/s/recall_health.json"), NOW)
    }

    fn health(calls: usize, failed: usize, stuck: bool, newest_at: i64) -> Health {
        Health {
            calls,
            failed,
            newest_at,
            stuck,
        }
    }

    #[test]
    fn no_file_is_info() {
        assert_eq!(line(Ok(None)).0, CheckLevel::Info);
    }

    #[test]
    fn a_low_rate_passes_and_a_high_rate_warns() {
        let (level, text) = line(Ok(Some(health(10, 2, false, NOW - 90))));
        assert_eq!(level, CheckLevel::Pass, "{text}");
        assert!(text.contains("last call 1 min ago"), "{text}");
        let (level, text) = line(Ok(Some(health(10, 3, false, NOW))));
        assert_eq!(level, CheckLevel::Warn);
        assert!(text.contains("3 of 10") && text.contains("Fix:"), "{text}");
    }

    #[test]
    fn stuck_wins_over_the_rate() {
        let (level, text) = line(Ok(Some(health(200, 10, true, NOW - 7200))));
        assert_eq!(level, CheckLevel::Warn);
        assert!(text.contains("stuck") && text.contains("2 h ago"), "{text}");
    }

    #[test]
    fn a_stuck_ring_older_than_a_day_is_info() {
        let at = NOW - STALE_AFTER_SECS - 1;
        let (level, text) = line(Ok(Some(health(200, 10, true, at))));
        assert_eq!(level, CheckLevel::Info, "{text}");
        assert!(text.contains("no adaptive recall ran since") && text.contains("1 days ago"));
        let (level, _) = line(Ok(Some(health(200, 10, true, NOW - STALE_AFTER_SECS))));
        assert_eq!(level, CheckLevel::Warn, "exactly a day is still current");
    }

    #[test]
    fn a_failing_often_ring_older_than_a_day_is_info() {
        let at = NOW - STALE_AFTER_SECS - 1;
        let (level, text) = line(Ok(Some(health(10, 6, false, at))));
        assert_eq!(level, CheckLevel::Info, "{text}");
        assert!(
            text.contains("6 of 10") && text.contains("no adaptive recall ran since"),
            "{text}"
        );
        assert!(text.contains("1 days ago"), "{text}");
        let (level, _) = line(Ok(Some(health(10, 6, false, NOW - STALE_AFTER_SECS))));
        assert_eq!(level, CheckLevel::Warn, "exactly a day is still current");
    }

    #[test]
    fn a_failing_ring_from_the_future_reads_as_old() {
        let (level, text) = line(Ok(Some(health(10, 6, false, NOW + 90))));
        assert_eq!(level, CheckLevel::Info, "{text}");
    }

    #[test]
    fn a_healthy_ring_older_than_a_day_still_passes() {
        let at = NOW - STALE_AFTER_SECS - 1;
        assert_eq!(line(Ok(Some(health(10, 1, false, at)))).0, CheckLevel::Pass);
    }

    #[test]
    fn an_unreadable_file_warns_with_its_path() {
        let (level, text) = line(Err(anyhow::anyhow!("bad json")));
        assert_eq!(level, CheckLevel::Warn);
        assert!(
            text.contains("/s/recall_health.json") && text.contains("bad json"),
            "{text}"
        );
    }

    proptest::proptest! {
        #[test]
        fn age_names_the_unit_of_the_clamped_elapsed_time(at: i64, now: i64) {
            let secs = i128::from(now).saturating_sub(i128::from(at)).max(0);
            let text = age(at, now);
            let unit = match secs {
                0..60 => " s ago",
                60..3600 => " min ago",
                3600..86_400 => " h ago",
                _ => " days ago",
            };
            proptest::prop_assert!(text.ends_with(unit), "{} for {}", text, secs);
        }
    }

    #[test]
    fn age_uses_the_largest_whole_unit_and_never_goes_negative() {
        assert_eq!(age(NOW, NOW), "0 s ago");
        assert_eq!(age(NOW + 5, NOW), "0 s ago");
        assert_eq!(age(NOW - 59, NOW), "59 s ago");
        assert_eq!(age(NOW - 3599, NOW), "59 min ago");
        assert_eq!(age(NOW - 86_399, NOW), "23 h ago");
        assert_eq!(age(NOW - 2 * 86_400, NOW), "2 days ago");
    }
}
