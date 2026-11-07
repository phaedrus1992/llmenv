//! Local JSONL sink. Append-only, owner-only, best-effort: a write failure logs
//! once as an agent notice and is dropped — session logging never fails a launch.

use std::collections::BTreeSet;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

use llmenv_paths::{create_dir_owner_only_refusing_unreadable, state_dir};

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
pub(crate) fn resolve_file_path(configured: Option<&str>) -> anyhow::Result<PathBuf> {
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
    /// dropped. The first failure for a path records an agent notice, and later ones log at debug.
    pub(crate) fn append(&self, line: &str) {
        let Err(e) = self.try_append(line) else {
            return;
        };
        // The lost event leaves no other trace, so the first failure is a notice for the agent. A
        // persistent failure, such as a symlinked path, fails on every event, so only the first one
        // is shown (#3136).
        if first_failure(&self.path) {
            crate::agent_notice::record_logged(format!(
                "llmenv: session log append to {} failed ({e}). Later failures for this path are \
                 logged at debug level only. Fix: run `llmenv doctor` to check the path.",
                self.path.display()
            ));
        } else {
            tracing::debug!(
                "session_log file append to {} failed: {e}",
                self.path.display()
            );
        }
    }

    fn try_append(&self, line: &str) -> std::io::Result<()> {
        if let Some(parent) = self.path.parent() {
            // The parent is a folder the user chose, so an unreadable one is refused, not
            // chmod'd by path (#3223).
            create_dir_owner_only_refusing_unreadable(parent)?;
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
mod tests;
