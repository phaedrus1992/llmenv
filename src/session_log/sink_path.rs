//! Setup checks for a session-log sink that writes to its path. A symbolic link at the path
//! is refused here, before the sink starts (#3117).

use std::path::{Path, PathBuf};

use crate::session_log::file_sink::{default_file_path, resolve_file_path_with};

/// Why the session-log sink cannot start.
#[derive(Debug, thiserror::Error)]
pub enum SinkPathError {
    /// The configured or default path cannot be resolved.
    #[error("{0:#}")]
    Resolve(anyhow::Error),
    /// The log path is a symbolic link. The size check follows the link, and the rotation
    /// renames the link itself, so the two would act on different files (#3117).
    #[error(
        "session_log.file.path {} is a symbolic link, and llmenv does not write the session log \
         through a link. Fix: set session_log.file.path to the real file, or replace the link \
         with a regular file.",
        .0.display()
    )]
    Symlink(PathBuf),
}

/// Resolve the session-log path for a sink that writes to it.
///
/// # Errors
/// Returns `SinkPathError::Resolve` when the path cannot be resolved, and
/// `SinkPathError::Symlink` when the path is a symbolic link.
pub fn resolve_sink_path(configured: Option<&str>) -> Result<PathBuf, SinkPathError> {
    resolve_sink_path_with(configured, default_file_path)
}

/// [`resolve_sink_path`] with an injectable default, so a test can make it fail.
///
/// # Errors
/// The same as [`resolve_sink_path`].
pub(crate) fn resolve_sink_path_with(
    configured: Option<&str>,
    default: impl FnOnce() -> anyhow::Result<PathBuf>,
) -> Result<PathBuf, SinkPathError> {
    let path = resolve_file_path_with(configured, default).map_err(SinkPathError::Resolve)?;
    if is_symlink(&path) {
        return Err(SinkPathError::Symlink(path));
    }
    Ok(path)
}

/// Whether `path` itself is a symbolic link. A missing path is not one: the first append creates
/// it. Any other metadata error is not one either: the append then reports the real failure.
fn is_symlink(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    #[cfg(unix)]
    fn a_symlinked_log_path_is_refused_with_the_path_and_the_fix() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real.jsonl");
        std::fs::write(&real, "").unwrap();
        let link = dir.path().join("log.jsonl");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let link_str = link.to_str().unwrap();

        let err = resolve_sink_path(Some(link_str)).unwrap_err();

        assert!(
            matches!(&err, SinkPathError::Symlink(p) if p == &link),
            "got {err:?}"
        );
        let text = err.to_string();
        assert!(
            text.contains(link_str),
            "message must name the path: {text}"
        );
        assert!(
            text.contains("symbolic link"),
            "message must say why: {text}"
        );
        assert!(text.contains("Fix:"), "message must give the fix: {text}");
    }

    #[test]
    fn a_missing_log_path_is_accepted() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session-log.jsonl");
        let got = resolve_sink_path(Some(path.to_str().unwrap())).unwrap();
        assert_eq!(got, path);
    }

    #[test]
    fn a_regular_log_file_is_accepted() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session-log.jsonl");
        std::fs::write(&path, "").unwrap();
        let got = resolve_sink_path(Some(path.to_str().unwrap())).unwrap();
        assert_eq!(got, path);
    }

    #[test]
    #[cfg(unix)]
    fn a_dangling_symlink_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let link = dir.path().join("log.jsonl");
        std::os::unix::fs::symlink(dir.path().join("absent.jsonl"), &link).unwrap();

        let err = resolve_sink_path(Some(link.to_str().unwrap())).unwrap_err();

        assert!(matches!(err, SinkPathError::Symlink(_)), "got {err:?}");
    }

    #[test]
    fn a_relative_path_is_a_resolve_error() {
        let err = resolve_sink_path(Some("relative.jsonl")).unwrap_err();
        assert!(matches!(err, SinkPathError::Resolve(_)), "got {err:?}");
    }
}
