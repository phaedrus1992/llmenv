//! The identity of a file for read-once: its modification time and its size.

use std::path::Path;

/// The modification time (Unix seconds) and the size of the file at `path`.
///
/// Returns `None` when the file cannot be stat'ed or its mtime is unusable. The caller must not
/// deduplicate the read in that case. A changed file with no usable mtime would look unchanged,
/// so a fixed value such as `0` would let the agent miss the change (#3109).
pub(super) fn file_stamp(path: &Path) -> Option<(i64, u64)> {
    let metadata = match std::fs::metadata(path) {
        Ok(m) => m,
        Err(e) => {
            crate::agent_notice::record_logged(format!(
                "llmenv: read-once cannot stat {}: {e}. Fix: check the file, then retry the read.",
                path.display()
            ));
            return None;
        }
    };
    let mtime = metadata
        .modified()
        .inspect_err(|e| crate::agent_notice::record_logged(format!("llmenv: read-once cannot read the mtime of {}: {e}. Fix: check the file. Read-once does not deduplicate it.", path.display())))
        .ok()
        .and_then(|t| {
            t.duration_since(std::time::UNIX_EPOCH)
                .inspect_err(|e| {
                    crate::agent_notice::record_logged(format!("llmenv: read-once: the mtime of {} is before 1970 ({e}). Fix: set the file time after 1970. Read-once does not deduplicate it.", path.display()));
                })
                .ok()
        })
        .map(|d| d.as_secs() as i64)?;
    Some((mtime, metadata.len()))
}

#[cfg(test)]
#[allow(clippy::expect_used, reason = "a test: a failed setup must stop it")]
mod tests {
    use super::*;
    use crate::test_log_capture::capture_logs;
    use std::time::{Duration, UNIX_EPOCH};

    #[test]
    fn a_file_with_a_pre_epoch_mtime_has_no_stamp_and_logs_why() {
        let dir = tempfile::tempdir().expect("temp dir");
        let file = dir.path().join("old.txt");
        std::fs::write(&file, b"hello").expect("write a test file");
        let before_epoch = UNIX_EPOCH - Duration::from_secs(60);
        std::fs::OpenOptions::new()
            .write(true)
            .open(&file)
            .expect("open a test file")
            .set_modified(before_epoch)
            .expect("set a pre-epoch mtime");
        let mut got = Some((0, 0));
        let logs = capture_logs(|| got = file_stamp(&file));
        assert_eq!(got, None, "a pre-epoch mtime must not produce a stamp");
        assert!(
            logs.contains("before 1970"),
            "the log must name the cause: {logs}"
        );
    }

    #[test]
    fn a_missing_file_has_no_stamp() {
        let dir = tempfile::tempdir().expect("temp dir");
        assert_eq!(file_stamp(&dir.path().join("absent")), None);
    }

    #[test]
    fn a_readable_file_has_its_size_and_a_recent_mtime() {
        let dir = tempfile::tempdir().expect("temp dir");
        let file = dir.path().join("new.txt");
        std::fs::write(&file, b"hello").expect("write a test file");
        let (mtime, size) = file_stamp(&file).expect("a readable file has a stamp");
        assert_eq!(size, 5);
        assert!(mtime > 0, "{mtime}");
    }
}
