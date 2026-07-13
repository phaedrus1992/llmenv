//! Local JSONL sink. Append-only, owner-only, best-effort: a write failure logs
//! at `debug!` and is dropped — session logging never fails a launch.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;

use llmenv_paths::state_dir;

/// Default file-sink path: `<state_dir>/session-log.jsonl`.
///
/// # Errors
/// Propagates `state_dir()` resolution failure.
pub fn default_file_path() -> anyhow::Result<PathBuf> {
    Ok(state_dir()?.join("session-log.jsonl"))
}

/// `default_file_path` as a string, falling back to a relative name if the
/// state dir cannot be resolved (the open will then fail-soft).
#[must_use]
pub fn default_file_path_string() -> String {
    default_file_path()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| "session-log.jsonl".to_string())
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
    pub fn append(&self, line: &str) {
        if let Err(e) = self.try_append(line) {
            tracing::debug!("session_log file append failed: {e}");
        }
    }

    fn try_append(&self, line: &str) -> std::io::Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut opts = OpenOptions::new();
        opts.create(true).append(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut f = opts.open(&self.path)?;
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
        f.write_all(line.as_bytes())?;
        f.write_all(b"\n")
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

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
}
