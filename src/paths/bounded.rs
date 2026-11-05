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
/// A file whose only bad bytes are an incomplete character at the end reads as the text before
/// it. A crash can cut a write there, and such a file is bad content for the caller to replace,
/// not an unreadable file that blocks every later write (#3445). Any other bytes that are not
/// UTF-8 stay an error, so a file with a bad byte inside a string is never rewritten unseen.
///
/// # Errors
/// The errors of [`llmenv_paths::read_file_bounded`], a missing file, and `InvalidData` for
/// bytes that are not UTF-8 before the end.
pub(crate) fn state_text_io(path: &Path) -> std::io::Result<String> {
    decode_state_text(state_bytes_io(path)?)
}

/// The text of state bytes by the rule of [`state_text_io`]: an incomplete last character is cut
/// off, and any other bytes that are not UTF-8 are an error.
///
/// # Errors
/// `InvalidData` for bytes that are not UTF-8 before the end.
pub(crate) fn decode_state_text(bytes: Vec<u8>) -> std::io::Result<String> {
    match String::from_utf8(bytes) {
        Ok(text) => Ok(text),
        Err(e) => {
            let error = e.utf8_error();
            let valid = error.valid_up_to();
            if error.error_len().is_none() {
                // The cut character is the last thing in the file.
                return Ok(
                    String::from_utf8_lossy(e.as_bytes().get(..valid).unwrap_or_default())
                        .into_owned(),
                );
            }
            Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "it is not UTF-8 text (the first bad byte is at offset {valid}); save it as \
                     UTF-8"
                ),
            ))
        }
    }
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
    fn decoded_state_bytes_cut_the_last_character_and_refuse_other_bad_bytes() {
        assert_eq!(decode_state_text(b"ok \xe2\x82".to_vec()).unwrap(), "ok ");
        assert_eq!(decode_state_text(b"ok".to_vec()).unwrap(), "ok");
        let err = decode_state_text(b"a\xffb".to_vec()).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    }

    #[test]
    fn a_bad_byte_before_the_end_is_still_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("latin1.json");
        std::fs::write(&path, b"{\"k\": \"caf\xe9\"}").unwrap();
        let err = state_text_io(&path).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData, "{err}");
    }

    proptest::proptest! {
        /// Any cut of a saved JSON state file, inside a character or not, reads as the text
        /// before the cut, and that text never parses as a different value (#3445).
        #[test]
        fn any_cut_state_file_reads_as_the_text_before_the_cut(
            values in proptest::collection::vec("[a-zé€😀]{0,4}", 1..5),
            cut in proptest::prelude::any::<proptest::sample::Index>(),
        ) {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("cut.json");
            let saved = serde_json::to_vec(&values).unwrap();
            let len = cut.index(saved.len());
            std::fs::write(&path, &saved[..len]).unwrap();
            let text = state_text_io(&path).unwrap();
            proptest::prop_assert!(saved.starts_with(text.as_bytes()));
            proptest::prop_assert!(serde_json::from_str::<Vec<String>>(&text).is_err());
        }
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
