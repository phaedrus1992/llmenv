//! Bounded reads with the path in the error, for the files that a render or a hook reads (#2945).
//!
//! [`llmenv_paths::read_file_bounded`] refuses a FIFO and a file past its cap, and its message
//! gives the reason only. These wrappers add the path, and the caps that fit each kind of file.

use std::path::Path;

/// The largest bundle or plugin file that materialize reads, copies, or translates. It is the
/// skill file cap: a script or an asset is far smaller, and a larger file is a mistake.
pub(crate) const MAX_ASSET_BYTES: u64 = 64 * 1024 * 1024;

/// The largest state file that llmenv writes for itself, such as a task, the merge cache, or the
/// throttle state. Each one holds a small JSON document.
const MAX_STATE_BYTES: u64 = 4 * 1024 * 1024;

/// Read the regular file at `path` of at most `max_bytes` bytes, or `None` when there is none.
///
/// # Errors
/// The file cannot be read, is not a regular file, or is too large. The message names the path.
fn read_optional(path: &Path, max_bytes: u64) -> anyhow::Result<Option<Vec<u8>>> {
    llmenv_paths::read_file_bounded(path, max_bytes)
        .map_err(|e| anyhow::anyhow!("cannot read {}: {e}", path.display()))
}

/// [`read_optional`] for a file that must exist.
///
/// # Errors
/// The errors of [`read_optional`], and a missing file.
fn read(path: &Path, max_bytes: u64) -> anyhow::Result<Vec<u8>> {
    read_optional(path, max_bytes)?
        .ok_or_else(|| anyhow::anyhow!("cannot read {}: it does not exist", path.display()))
}

/// [`read`] for a UTF-8 text file.
///
/// # Errors
/// The errors of [`read`], and a file that is not UTF-8.
fn read_text(path: &Path, max_bytes: u64) -> anyhow::Result<String> {
    read_utf8(path, read(path, max_bytes)?)
}

fn read_utf8(path: &Path, bytes: Vec<u8>) -> anyhow::Result<String> {
    String::from_utf8(bytes).map_err(|e| {
        anyhow::anyhow!(
            "cannot read {}: it is not UTF-8 text (the first bad byte is at offset {})",
            path.display(),
            e.utf8_error().valid_up_to()
        )
    })
}

/// A bundle or plugin file that must exist, capped at [`MAX_ASSET_BYTES`].
///
/// # Errors
/// The errors of [`read`].
pub(crate) fn asset(path: &Path) -> anyhow::Result<Vec<u8>> {
    read(path, MAX_ASSET_BYTES)
}

/// [`asset`] for a UTF-8 text file.
///
/// # Errors
/// The errors of [`read_text`].
pub(crate) fn asset_text(path: &Path) -> anyhow::Result<String> {
    read_text(path, MAX_ASSET_BYTES)
}

/// A state file of llmenv, capped at [`MAX_STATE_BYTES`], or `None` when there is none.
///
/// # Errors
/// The errors of [`read_optional`].
pub(crate) fn state(path: &Path) -> anyhow::Result<Option<Vec<u8>>> {
    read_optional(path, MAX_STATE_BYTES)
}

/// [`asset_text`] for a file that may be absent, or that is not UTF-8 text.
///
/// # Errors
/// The errors of [`read_optional`], and a file that is not UTF-8.
pub(crate) fn asset_text_optional(path: &Path) -> anyhow::Result<Option<String>> {
    read_optional(path, MAX_ASSET_BYTES)?
        .map(|bytes| read_utf8(path, bytes))
        .transpose()
}

/// A state file of llmenv that must exist, capped at [`MAX_STATE_BYTES`].
///
/// # Errors
/// The errors of [`read`].
pub(crate) fn state_bytes(path: &Path) -> anyhow::Result<Vec<u8>> {
    read(path, MAX_STATE_BYTES)
}

/// [`state_bytes`] for a UTF-8 text file.
///
/// # Errors
/// The errors of [`read_text`].
pub(crate) fn state_text(path: &Path) -> anyhow::Result<String> {
    read_text(path, MAX_STATE_BYTES)
}

/// A state file of llmenv, capped at [`MAX_STATE_BYTES`], with the I/O error kept: a drop-in
/// for `std::fs::read` where the caller already handles `NotFound` and names the path.
///
/// # Errors
/// The errors of [`llmenv_paths::read_file_bounded`], and a missing file.
pub(crate) fn state_bytes_io(path: &Path) -> std::io::Result<Vec<u8>> {
    llmenv_paths::read_file_bounded(path, MAX_STATE_BYTES)?
        .ok_or_else(|| std::io::ErrorKind::NotFound.into())
}

/// [`state_bytes_io`] capped at [`MAX_ASSET_BYTES`], for a file of the engine such as
/// `.claude.json`, which Claude Code lets grow past the state cap.
///
/// # Errors
/// The errors of [`llmenv_paths::read_file_bounded`], and a missing file.
pub(crate) fn large_bytes_io(path: &Path) -> std::io::Result<Vec<u8>> {
    llmenv_paths::read_file_bounded(path, MAX_ASSET_BYTES)?
        .ok_or_else(|| std::io::ErrorKind::NotFound.into())
}

/// [`state_text`] with the I/O error kept, for a caller that tells a read failure from bad
/// content. A missing file is a `NotFound` error.
///
/// Bytes that are not UTF-8 become U+FFFD instead of an error. A crash can cut a write inside a
/// multi-byte character, and such a file is bad content for the caller to replace, not an
/// unreadable file that blocks every later write (#3445).
///
/// # Errors
/// The errors of [`llmenv_paths::read_file_bounded`], and a missing file.
pub(crate) fn state_text_io(path: &Path) -> std::io::Result<String> {
    state_bytes_io(path).map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
}

/// Open the regular file at `path` for a streamed read, or `None` when there is no file there.
/// The open does not block on a FIFO, and the check uses the same open file.
///
/// # Errors
/// The file cannot be opened, or is not a regular file.
pub(crate) fn open_regular(path: &Path) -> std::io::Result<Option<std::fs::File>> {
    let mut opts = std::fs::OpenOptions::new();
    opts.read(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::custom_flags(
        &mut opts,
        rustix::fs::OFlags::NONBLOCK.bits().cast_signed(),
    );
    let file = match opts.open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    llmenv_paths::check_regular_file(&file)?;
    Ok(Some(file))
}

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "test code")]
mod tests {
    use super::*;

    #[test]
    fn a_state_file_cut_inside_a_utf8_character_reads_as_bad_text_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cut.json");
        std::fs::write(&path, b"{\"k\": \"caf\xc3").unwrap();
        let text = state_text_io(&path).unwrap();
        assert!(text.ends_with('\u{FFFD}'), "{text:?}");
        assert!(serde_json::from_str::<serde_json::Value>(&text).is_err());
    }

    #[test]
    fn a_missing_file_is_none_or_an_error_that_names_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("gone.json");
        assert!(read_optional(&path, 10).unwrap().is_none());
        let err = read(&path, 10).unwrap_err().to_string();
        assert!(
            err.contains("gone.json") && err.contains("does not exist"),
            "{err}"
        );
    }

    #[test]
    fn a_file_past_the_cap_fails_with_its_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("big");
        std::fs::write(&path, "0123456789").unwrap();
        let err = read(&path, 4).unwrap_err().to_string();
        assert!(
            err.contains("big") && err.contains("more than 4 bytes"),
            "{err}"
        );
        assert_eq!(read_text(&path, 10).unwrap(), "0123456789");
    }

    #[cfg(unix)]
    #[test]
    fn a_fifo_fails_at_once() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pipe");
        let made = std::process::Command::new("mkfifo")
            .arg(&path)
            .status()
            .unwrap();
        assert!(made.success());
        let err = read(&path, 10).unwrap_err().to_string();
        assert!(err.contains("not a regular file"), "{err}");
    }
}

#[cfg(all(test, unix))]
#[expect(clippy::unwrap_used, reason = "test code")]
mod site_tests;
