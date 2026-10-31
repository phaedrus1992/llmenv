//! The age test that decides whether a cache or temp entry is old enough to remove (#2954).

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, SystemTime};

/// An mtime this far ahead of the clock is a fresh write, not skew. A coarse file-system
/// timestamp can put a write slightly ahead of the clock read.
pub(crate) const CLOCK_SKEW_TOLERANCE: Duration = Duration::from_secs(5);

/// The paths whose clock-skew error is already logged in this process. A cache sweep scans a
/// folder twice, once for its gate and once for the sweep, so one skewed entry would log twice.
static SKEW_REPORTED: Mutex<BTreeSet<PathBuf>> = Mutex::new(BTreeSet::new());

/// Record that `path` has a clock-skew report. True only the first time in this process (#3077).
fn first_skew_report(path: &Path) -> bool {
    SKEW_REPORTED
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .insert(path.to_path_buf())
}

/// Check whether `mtime` is older than `older_than` relative to `now`.
///
/// An mtime in the future is kept. It stays kept until the clock passes it, and then its age is
/// measured normally. A clock that moved backward must not delete live data, and the `state/tmp`
/// sweep keeps such an entry too (#2860).
///
/// `now` is read once before a walk, so a write made during the walk can be ahead of it. When
/// `now` is behind the mtime, the wall clock is read again before the mtime is called skew (#2954).
pub(crate) fn is_expired(
    now: SystemTime,
    mtime: SystemTime,
    older_than: Duration,
    path: &Path,
) -> bool {
    let age = now
        .duration_since(mtime)
        .or_else(|_| SystemTime::now().duration_since(mtime));
    match age {
        Ok(elapsed) => elapsed > older_than,
        Err(e) if e.duration() <= CLOCK_SKEW_TOLERANCE => {
            tracing::debug!(
                "{} has an mtime {}ms ahead of the clock, within tolerance, so it is kept",
                path.display(),
                e.duration().as_millis()
            );
            false
        }
        Err(e) => {
            // Error level: an entry with a future mtime is kept, so the cause must show.
            if first_skew_report(path) {
                tracing::error!(
                    skew_secs = e.duration().as_secs(),
                    "clock skew: {} has an mtime {}s in the future; keeping the entry",
                    path.display(),
                    e.duration().as_secs()
                );
            }
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_clock_skew_path_is_reported_once_per_process() {
        // A unique path, because the set is process-wide and other tests share it.
        let path = Path::new("/llmenv-test/skew-report-once/entry");
        assert!(first_skew_report(path));
        assert!(!first_skew_report(path));
    }

    const OLDER_THAN: Duration = Duration::from_secs(60);

    #[test]
    fn an_entry_older_than_the_threshold_is_expired() {
        let now = SystemTime::now();
        assert!(is_expired(
            now,
            now - Duration::from_secs(61),
            OLDER_THAN,
            Path::new("e")
        ));
    }

    #[test]
    fn an_entry_exactly_at_the_threshold_is_kept() {
        let now = SystemTime::now();
        assert!(!is_expired(
            now,
            now - OLDER_THAN,
            OLDER_THAN,
            Path::new("e")
        ));
    }

    #[test]
    fn a_future_mtime_beyond_the_tolerance_is_kept() {
        let now = SystemTime::now();
        let future = now + Duration::from_secs(3600);
        assert!(!is_expired(now, future, Duration::ZERO, Path::new("e")));
    }

    #[test]
    fn a_future_mtime_within_the_tolerance_is_kept() {
        let now = SystemTime::now();
        let slightly_ahead = now + Duration::from_secs(1);
        assert!(!is_expired(
            now,
            slightly_ahead,
            Duration::ZERO,
            Path::new("e")
        ));
    }
}
