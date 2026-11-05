//! The record count of a session log, for the status file.

use std::path::Path;

/// The number of records in the session log at `path`, or `None` when it is absent or cannot be
/// read. A read failure, a non-regular file included, is logged at warn level with the path.
pub(crate) fn count_records(path: &Path) -> Option<u64> {
    count_or_failure(path).unwrap_or_else(|failure| {
        tracing::warn!("{failure}");
        None
    })
}

/// [`count_records`] with the failure text returned instead of logged, so a test can read it
/// without a shared log capture (#3270).
fn count_or_failure(path: &Path) -> Result<Option<u64>, String> {
    let failure = |e: std::io::Error| {
        format!(
            "session log stat collection of {} failed, so the record count is empty: {e}. \
             Fix: run `llmenv doctor` to check session_log.file.path.",
            path.display()
        )
    };
    // The log can be large, and only its records are counted, so it is streamed. The open does not
    // block on a FIFO.
    let Some(file) = crate::paths::bounded::open_regular(path).map_err(failure)? else {
        return Ok(None);
    };
    let mut count = 0u64;
    for line in std::io::BufRead::lines(std::io::BufReader::new(file)) {
        if !line.map_err(failure)?.trim().is_empty() {
            count = count.saturating_add(1);
        }
    }
    Ok(Some(count))
}

#[cfg(all(test, unix))]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn a_fifo_is_logged_with_its_path_and_the_fix() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        let status = std::process::Command::new("mkfifo")
            .arg(&path)
            .status()
            .unwrap();
        assert!(status.success());
        assert_eq!(count_records(&path), None);
        let failure = count_or_failure(&path).unwrap_err();
        assert!(failure.contains(&path.display().to_string()), "{failure}");
        assert!(failure.contains("llmenv doctor"), "{failure}");
    }

    #[test]
    fn a_missing_file_has_no_count_and_no_log() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(count_or_failure(&dir.path().join("absent.jsonl")), Ok(None));
    }
}
