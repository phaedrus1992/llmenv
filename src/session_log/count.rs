//! The record count of a session log, for the status file.

use std::path::Path;

/// The number of records in the session log at `path`, or `None` when it is absent or cannot be
/// read. A read failure, a non-regular file included, is logged at warn level with the path.
pub(crate) fn count_records(path: &Path) -> Option<u64> {
    count_with(path, |failure| tracing::warn!("{failure}"))
}

/// [`count_records`] with the failure sent to `report`, so a test reads the text without a shared
/// log capture (#3270).
fn count_with(path: &Path, report: impl FnOnce(&str)) -> Option<u64> {
    count_or_failure(path).unwrap_or_else(|failure| {
        report(&failure);
        None
    })
}

fn count_or_failure(path: &Path) -> Result<Option<u64>, String> {
    let failure = |step: &str, e: std::io::Error| {
        format!(
            "session log stat collection cannot {step} {}, so the record count is empty: {e}. \
             Fix: run `llmenv doctor` to check session_log.file.path.",
            path.display()
        )
    };
    // The log can be large, and only its records are counted, so it is streamed. The open does not
    // block on a FIFO.
    let opened = crate::paths::bounded::open_regular(path).map_err(|e| failure("open", e))?;
    let Some(file) = opened else {
        return Ok(None);
    };
    let mut count = 0u64;
    for line in std::io::BufRead::lines(std::io::BufReader::new(file)) {
        if !line.map_err(|e| failure("read", e))?.trim().is_empty() {
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
    fn a_fifo_is_reported_with_its_path_and_the_fix() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        let status = std::process::Command::new("mkfifo")
            .arg(&path)
            .status()
            .unwrap();
        assert!(status.success());
        let mut reported = None;
        assert_eq!(count_with(&path, |f| reported = Some(f.to_string())), None);
        let failure = reported.unwrap();
        assert!(failure.contains("cannot open"), "{failure}");
        assert!(failure.contains(&path.display().to_string()), "{failure}");
        assert!(failure.contains("llmenv doctor"), "{failure}");
    }

    #[test]
    fn a_missing_file_has_no_count_and_no_report() {
        let dir = tempfile::tempdir().unwrap();
        let mut reported = false;
        let got = count_with(&dir.path().join("absent.jsonl"), |_| reported = true);
        assert_eq!(got, None);
        assert!(!reported);
    }

    #[test]
    fn a_line_that_is_not_utf8_is_a_read_failure() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        std::fs::write(&path, b"{}\n\xff\xfe\n").unwrap();
        let failure = count_or_failure(&path).unwrap_err();
        assert!(failure.contains("cannot read"), "{failure}");
    }

    proptest::proptest! {
        /// The count is the number of lines that hold more than white space, for any text.
        #[test]
        fn the_count_is_the_number_of_non_blank_lines(
            lines in proptest::collection::vec("[ \\t]{0,2}|\\PC{0,12}", 0..12),
            ending in "|\n|\r\n",
        ) {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("s.jsonl");
            let separator = if ending.is_empty() { "\n" } else { ending.as_str() };
            let text = format!("{}{ending}", lines.join(separator));
            std::fs::write(&path, &text).unwrap();
            let expected = text.lines().filter(|l| !l.trim().is_empty()).count();
            proptest::prop_assert_eq!(count_records(&path), Some(u64::try_from(expected).unwrap()));
        }
    }
}
