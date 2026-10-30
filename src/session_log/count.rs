//! The record count of a session log, for the status file.

use std::path::Path;

/// The number of records in the session log at `path`, or `None` when it is absent or cannot be
/// read. A read failure, a non-regular file included, is logged at warn level with the path.
pub(crate) fn count_records(path: &Path) -> Option<u64> {
    // The log can be large, and only its records are counted, so it is streamed. The open does not
    // block on a FIFO.
    let file = match crate::paths::bounded::open_regular(path) {
        Ok(Some(file)) => file,
        Ok(None) => return None,
        Err(e) => {
            tracing::warn!(
                "session log stat collection of {} failed, so the record count is empty: {e}. \
                 Fix: run `llmenv doctor` to check session_log.file.path.",
                path.display()
            );
            return None;
        }
    };
    let mut count = 0u64;
    for line in std::io::BufRead::lines(std::io::BufReader::new(file)) {
        match line {
            Ok(line) if !line.trim().is_empty() => count = count.saturating_add(1),
            Ok(_) => {}
            Err(e) => {
                tracing::warn!(
                    "session log stat collection of {} failed, so the record count is empty: {e}. \
                     Fix: run `llmenv doctor` to check session_log.file.path.",
                    path.display()
                );
                return None;
            }
        }
    }
    Some(count)
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
        let mut got = Some(0);
        let logs = crate::test_log_capture::capture_logs(|| got = count_records(&path));
        assert_eq!(got, None);
        assert!(logs.contains(&path.display().to_string()), "{logs}");
        assert!(logs.contains("llmenv doctor"), "{logs}");
    }

    #[test]
    fn a_missing_file_has_no_count_and_no_log() {
        let dir = tempfile::tempdir().unwrap();
        let mut got = Some(0);
        let logs = crate::test_log_capture::capture_logs(|| {
            got = count_records(&dir.path().join("absent.jsonl"));
        });
        assert_eq!(got, None);
        assert!(logs.is_empty(), "{logs}");
    }
}
