//! Local JSONL sink. Append-only, owner-only, best-effort: a write failure logs
//! at `debug!` and is dropped — session logging never fails a launch.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};

use llmenv_paths::{create_dir_owner_only, reject_non_regular_file, state_dir};

use crate::session_log::rotation::{MAX_BYTES, rotate_if_full, rotation_marker};

/// Default file-sink path: `<state_dir>/session-log.jsonl`.
///
/// # Errors
/// Propagates `state_dir()` resolution failure.
pub fn default_file_path() -> anyhow::Result<PathBuf> {
    Ok(file_path_in(&state_dir()?))
}

/// The default file-sink path under `state_dir`.
pub(crate) fn file_path_in(state_dir: &Path) -> PathBuf {
    state_dir.join("session-log.jsonl")
}

/// Appends rendered events to one JSONL file.
#[derive(Debug, Clone)]
pub struct FileSink {
    path: PathBuf,
}

impl FileSink {
    /// Create a sink writing to `path`. The parent dir is created on first
    /// append.
    #[must_use]
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    /// Append one line (a `\n` is added). Best-effort; errors are logged and
    /// dropped.
    pub(crate) fn append(&self, line: &str) {
        if let Err(e) = self.try_append(line) {
            // Error level: the default filter hides a debug, and the lost event leaves no trace.
            tracing::error!(
                "session_log file append to {} failed: {e}",
                self.path.display()
            );
        }
    }

    fn try_append(&self, line: &str) -> std::io::Result<()> {
        if let Some(parent) = self.path.parent() {
            create_dir_owner_only(parent)?;
        }
        // A symlink can't simply be replaced the way write_owner_only's
        // unlink-then-recreate does — that would destroy the log history
        // this append is meant to preserve — so refuse it outright instead
        // (#1431).
        reject_non_regular_file(&self.path)?;
        // A failed rotation must not stop logging. The event goes to the active file, and the
        // next append tries the rotation again.
        let rotated = match rotate_if_full(&self.path, MAX_BYTES) {
            Ok(rotated) => rotated,
            Err(e) => {
                // Error level: the default filter hides a warn (#2657).
                tracing::error!(
                    path = %self.path.display(),
                    error = %e,
                    "session log rotation failed; appending to the active file"
                );
                None
            }
        };
        let mut opts = OpenOptions::new();
        opts.create(true).append(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut f = crate::paths::refuse_symlink(&mut opts).open(&self.path)?;
        // `mode(0o600)` above only applies on creation (O_CREAT); a file that
        // already existed (e.g. created with a looser umask before this sink
        // ran, or by an older llmenv version) keeps its prior permissions. Set
        // them explicitly on every open via the already-open fd so a
        // pre-existing world-readable file gets locked down before this
        // process appends potentially sensitive session-log content to it.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            f.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        }
        if let Some(name) = rotated {
            f.write_all(rotation_marker(&name).as_bytes())?;
            f.write_all(b"\n")?;
        }
        f.write_all(line.as_bytes())?;
        f.write_all(b"\n")
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::session_log::rotation::{KEPT_ROTATIONS, rotation_path};
    use std::fs::File;

    #[test]
    fn append_writes_lines_and_preserves_existing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session-log.jsonl");
        let sink = FileSink::new(path.clone());
        sink.append("{\"a\":1}");
        sink.append("{\"b\":2}");
        let body = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = body.lines().collect();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0], "{\"a\":1}");
        assert_eq!(lines[1], "{\"b\":2}");
    }

    #[cfg(unix)]
    #[test]
    fn append_creates_owner_only_file() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        FileSink::new(path.clone()).append("{}");
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o077, 0, "group/other bits must be unset: {mode:o}");
    }

    // #1186: append creates its parent directory owner-only, not just the file.
    #[cfg(unix)]
    #[test]
    fn append_creates_parent_dir_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("s.jsonl");
        FileSink::new(path.clone()).append("{}");
        let mode = std::fs::metadata(path.parent().unwrap())
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o700, "log dir must be owner-only, got {mode:o}");
    }

    /// `try_append` re-opens the destination on every call with no symlink
    /// check — a pre-existing symlink there gets appended-to (and its
    /// permissions rewritten) rather than refused, the same TOCTOU class
    /// already fixed for other writers in this codebase (#1341, #1422,
    /// #1423, #1427, #1429; extended here since append can't safely
    /// unlink-and-replace the way those fixes did — that would destroy the
    /// log history append is meant to preserve) (#1431).
    #[cfg(unix)]
    #[test]
    fn try_append_does_not_write_through_a_symlinked_destination() {
        let dir = tempfile::tempdir().unwrap();
        let victim = dir.path().join("victim");
        std::fs::write(&victim, "must-not-be-touched\n").unwrap();
        let path = dir.path().join("session-log.jsonl");
        std::os::unix::fs::symlink(&victim, &path).unwrap();

        let sink = FileSink::new(path.clone());
        let result = sink.try_append("{\"attacker\":true}");

        assert!(
            result.is_err(),
            "appending through a pre-existing symlink must be refused"
        );
        assert_eq!(
            std::fs::read_to_string(&victim).unwrap(),
            "must-not-be-touched\n",
            "the symlink's target must be untouched"
        );
        assert!(
            std::fs::symlink_metadata(&path)
                .unwrap()
                .file_type()
                .is_symlink(),
            "the symlink itself must be left alone, not replaced or removed"
        );
    }

    #[cfg(unix)]
    #[test]
    fn append_re_protects_a_pre_existing_world_readable_file() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        std::fs::write(&path, "").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        FileSink::new(path.clone()).append("{}");
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(
            mode & 0o077,
            0,
            "append must lock down a pre-existing looser-permission file: {mode:o}"
        );
    }

    #[test]
    fn an_append_to_a_full_file_starts_the_new_file_with_a_marker() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session-log.jsonl");
        // A sparse file of the real cap: no 64 MiB of data is written.
        File::create(&path).unwrap().set_len(MAX_BYTES).unwrap();
        FileSink::new(path.clone()).append("{\"after\":1}");
        let body = std::fs::read_to_string(&path).unwrap();
        let mut lines = body.lines();
        assert_eq!(
            lines.next(),
            Some(rotation_marker("session-log.jsonl.1").as_str())
        );
        assert_eq!(lines.next(), Some("{\"after\":1}"));
        assert!(rotation_path(&path, 1).exists());
    }

    #[test]
    fn a_failed_rotation_still_appends_the_event_to_the_active_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session-log.jsonl");
        File::create(&path).unwrap().set_len(MAX_BYTES).unwrap();
        // A non-empty directory where the oldest generation belongs makes its removal fail.
        let oldest = rotation_path(&path, KEPT_ROTATIONS);
        std::fs::create_dir(&oldest).unwrap();
        std::fs::write(oldest.join("keep"), b"k").unwrap();
        FileSink::new(path.clone()).append("{\"after\":1}");
        let body = std::fs::read_to_string(&path).unwrap();
        assert!(body.ends_with("{\"after\":1}\n"), "the event was lost");
    }
}
