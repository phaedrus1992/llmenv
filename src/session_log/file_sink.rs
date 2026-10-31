//! Local JSONL sink. Append-only, owner-only, best-effort: a write failure logs
//! once at `error!` and is dropped — session logging never fails a launch.

use std::collections::BTreeSet;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

use llmenv_paths::{create_dir_owner_only, state_dir};

use crate::session_log::rotation::{
    MAX_BYTES, non_regular_path_error, rotate_if_full, rotation_failed_marker, rotation_marker,
};

/// Default file-sink path: `<state_dir>/session-log.jsonl`.
///
/// # Errors
/// Propagates `state_dir()` resolution failure.
pub(crate) fn default_file_path() -> anyhow::Result<PathBuf> {
    Ok(file_path_in(&state_dir()?))
}

/// Resolve the file-sink path: the configured `session_log.file.path` with `~` expanded, or
/// `<state_dir>/session-log.jsonl`. `llmenv`, its hooks, and `llmenv doctor` share this rule.
///
/// # Errors
/// Fails when `HOME` or the state folder cannot be resolved, or when the path is relative.
pub fn resolve_file_path(configured: Option<&str>) -> anyhow::Result<PathBuf> {
    resolve_file_path_with(configured, default_file_path)
}

/// [`resolve_file_path`] with an injectable default, so a test can make it fail.
pub(crate) fn resolve_file_path_with(
    configured: Option<&str>,
    default: impl FnOnce() -> anyhow::Result<PathBuf>,
) -> anyhow::Result<PathBuf> {
    let path = match configured {
        Some(raw) => PathBuf::from(llmenv_paths::expand_tilde(raw)?),
        None => default()?,
    };
    // A relative path resolves against the working directory, which is a repository in a hook.
    anyhow::ensure!(
        path.is_absolute(),
        "session_log.file.path {} is relative. Set it to an absolute path or a path that \
         starts with ~/",
        path.display()
    );
    Ok(path)
}

/// The default file-sink path under `state_dir`.
pub(crate) fn file_path_in(state_dir: &Path) -> PathBuf {
    state_dir.join("session-log.jsonl")
}

/// Sink paths whose append failure this process already reported at error level.
static REPORTED_FAILURES: Mutex<BTreeSet<PathBuf>> = Mutex::new(BTreeSet::new());

/// True on the first append failure for `path` in this process.
fn first_failure(path: &Path) -> bool {
    // A panic cannot leave the set half-changed, because `insert` is its only write. So a
    // poisoned lock holds a valid set, and the sink uses it.
    REPORTED_FAILURES
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .insert(path.to_path_buf())
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
    /// dropped. The first failure for a path logs at error level, later ones at debug.
    pub(crate) fn append(&self, line: &str) {
        let Err(e) = self.try_append(line) else {
            return;
        };
        // Error level: the lost event leaves no other trace. A persistent failure such as a symlinked path fails on every event, so only the
        // first one is shown (#3136).
        if first_failure(&self.path) {
            tracing::error!(
                "session_log file append to {} failed: {e}. Later failures for this path are \
                 logged at debug level only. Run `llmenv doctor` to check the path.",
                self.path.display()
            );
        } else {
            tracing::debug!(
                "session_log file append to {} failed: {e}",
                self.path.display()
            );
        }
    }

    fn try_append(&self, line: &str) -> std::io::Result<()> {
        if let Some(parent) = self.path.parent() {
            create_dir_owner_only(parent)?;
        }
        // A failed rotation must not stop logging. The event goes to the active file, and the
        // next append tries the rotation again.
        let (rotated, rotation_error) = match rotate_if_full(&self.path, MAX_BYTES) {
            Ok(rotated) => (rotated, None),
            Err(e) if e.kind() == std::io::ErrorKind::InvalidInput => return Err(e),
            Err(e) => {
                tracing::error!(
                    path = %self.path.display(),
                    error = %e,
                    "session log rotation failed; appending to the active file"
                );
                (None, Some(e))
            }
        };
        let mut opts = OpenOptions::new();
        opts.create(true).append(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
            opts.custom_flags(
                (rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::NONBLOCK)
                    .bits()
                    .cast_signed(),
            );
        }
        #[cfg(not(unix))]
        crate::paths::refuse_symlink(&mut opts);
        let mut f = opts.open(&self.path)?;
        let metadata = f.metadata().map_err(|e| {
            std::io::Error::new(
                e.kind(),
                format!(
                    "cannot inspect opened session log {}: {e}",
                    self.path.display()
                ),
            )
        })?;
        if !metadata.file_type().is_file() {
            return Err(non_regular_path_error(&self.path));
        }
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
        // The layer drops the error event above while it writes to this file, so the file
        // records the failure itself.
        if let Some(e) = rotation_error {
            f.write_all(rotation_failed_marker(&e).as_bytes())?;
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

    #[cfg(unix)]
    #[test]
    fn append_rejects_a_symlink_without_moving_it_or_its_target() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target.jsonl");
        let path = dir.path().join("session-log.jsonl");
        File::create(&target).unwrap().set_len(MAX_BYTES).unwrap();
        symlink(&target, &path).unwrap();

        let error = FileSink::new(path.clone())
            .try_append("{\"event\":true}")
            .unwrap_err();

        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
        assert!(error.to_string().contains("Fix:"));
        assert!(error.to_string().contains("session_log.file.path"));
        assert_eq!(std::fs::read_link(&path).unwrap(), target);
        assert_eq!(std::fs::metadata(&target).unwrap().len(), MAX_BYTES);
        assert!(!rotation_path(&path, 1).exists());
    }

    #[cfg(unix)]
    #[test]
    fn append_rejects_a_fifo_before_opening_it() {
        use std::os::unix::fs::FileTypeExt;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session-log.fifo");
        let status = std::process::Command::new("mkfifo")
            .arg(&path)
            .status()
            .unwrap();
        assert!(status.success(), "mkfifo exited with {status}");

        let error = FileSink::new(path.clone())
            .try_append("{\"event\":true}")
            .unwrap_err();

        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
        assert!(error.to_string().contains("not a regular file"));
        assert!(
            std::fs::symlink_metadata(path)
                .unwrap()
                .file_type()
                .is_fifo()
        );
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
        // The sparse fixture has no newline, so the marker shares the last line of NUL bytes.
        let marker = body.find(r#"{"error":"#).unwrap();
        let event = body.rfind(r#"{"after":1}"#).unwrap();
        assert!(marker < event, "the marker must come before the event");
        let line = body[marker..event].trim_end();
        let v: serde_json::Value = serde_json::from_str(line).unwrap();
        assert_eq!(v["llmenv_log"], "rotation_failed", "{line}");
    }
    #[test]
    fn a_configured_tilde_path_is_expanded() {
        let home = std::env::var("HOME").unwrap();
        let got = resolve_file_path_with(Some("~/logs/s.jsonl"), || unreachable!()).unwrap();
        assert_eq!(got, Path::new(&home).join("logs/s.jsonl"));
    }

    #[test]
    fn a_relative_configured_path_is_rejected_with_the_key() {
        let err = resolve_file_path_with(Some("logs/s.jsonl"), || unreachable!()).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("session_log.file.path"), "{msg}");
        assert!(msg.contains("logs/s.jsonl"), "{msg}");
    }

    #[test]
    fn no_configured_path_uses_the_default() {
        let got =
            resolve_file_path_with(None, || Ok(PathBuf::from("/state/session-log.jsonl"))).unwrap();
        assert_eq!(got, PathBuf::from("/state/session-log.jsonl"));
    }

    mod properties {
        use super::*;
        use proptest::prelude::*;

        proptest! {
            /// An absolute configured path comes back unchanged, and the default is not read.
            #[test]
            fn an_absolute_configured_path_is_kept(rest in "[a-zA-Z0-9_./-]{0,40}") {
                let raw = format!("/{rest}");
                let got = resolve_file_path_with(Some(&raw), || unreachable!()).unwrap();
                prop_assert_eq!(got, PathBuf::from(&raw));
            }

            /// A relative configured path, the empty path included, is an error that names the key.
            #[test]
            fn a_relative_configured_path_is_an_error(raw in "([a-zA-Z0-9_.-][a-zA-Z0-9_./-]{0,30})?") {
                prop_assume!(!raw.starts_with('~'));
                let err = resolve_file_path_with(Some(&raw), || unreachable!()).unwrap_err();
                let msg = format!("{err:#}");
                prop_assert!(msg.contains("session_log.file.path"), "{}", msg);
            }

            /// Each distinct path reports once, and every later failure for it does not.
            #[test]
            fn each_path_reports_its_first_failure_only(
                names in proptest::collection::btree_set("[a-z]{1,8}", 1..6),
                repeats in 1usize..4,
            ) {
                let dir = tempfile::tempdir().unwrap();
                let paths: Vec<PathBuf> = names.iter().map(|n| dir.path().join(n)).collect();
                let firsts = paths.iter().filter(|p| first_failure(p)).count();
                prop_assert_eq!(firsts, paths.len());
                for _ in 0..repeats {
                    prop_assert!(paths.iter().all(|p| !first_failure(p)));
                }
            }
        }
    }

    // #3136: a sink that keeps failing reports the failure once, not once per event.
    #[cfg(unix)]
    #[test]
    fn a_repeated_append_failure_is_reported_once() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("real.jsonl");
        std::fs::write(&target, "").unwrap();
        let link = dir.path().join("link.jsonl");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let log = dir.path().join("log.jsonl");
        crate::session_log::tracing_layer::capture_file_logs_at(
            &log,
            tracing_subscriber::filter::LevelFilter::TRACE,
            || {
                FileSink::new(link.clone()).append("{}");
                FileSink::new(link.clone()).append("{}");
            },
        );
        let body = std::fs::read_to_string(&log).unwrap();
        let reports = body.lines().filter(|l| l.contains("append to")).count();
        assert_eq!(reports, 1, "log body: {body}");
    }
}
