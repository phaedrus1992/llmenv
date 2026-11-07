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
/// A file cut inside its last character is an error, not a shorter text. A reader that accepted
/// the cut text could return a value that looks complete, and a caller could write it back.
///
/// # Errors
/// The errors of [`llmenv_paths::read_file_bounded`], a missing file, and `InvalidData` for
/// bytes that are not UTF-8. The error names the path.
pub(crate) fn state_text_io(path: &Path) -> std::io::Result<String> {
    let bytes = state_bytes_io(path)?;
    decode_state_text(bytes)
        .map_err(|e| std::io::Error::new(e.kind(), format!("{}: {e}", path.display())))
}

/// [`state_text_io`] for a JSON state file whose caller resets corrupt content. A cut inside the
/// last character leaves bad JSON, and the caller replaces that file. An error here would block
/// every later save instead (#3445).
///
/// # Errors
/// The errors of [`state_bytes_io`], and `InvalidData` for bytes that are not UTF-8 before the
/// end. The error names the path.
pub(crate) fn state_json_text_io(path: &Path) -> std::io::Result<String> {
    decode_text_cut_at_end(state_bytes_io(path)?)
        .map_err(|e| std::io::Error::new(e.kind(), format!("{}: {e}", path.display())))
}

/// The text of bytes that a read-only caller classifies, where a cut at the end is harmless. An
/// incomplete last character is dropped, and any other bad byte is an error. A caller that
/// writes the text back MUST use [`decode_state_text`] instead, because a dropped character is
/// lost data on the next write.
///
/// # Errors
/// `InvalidData` for bytes that are not UTF-8 before the end.
pub(crate) fn decode_text_cut_at_end(bytes: Vec<u8>) -> std::io::Result<String> {
    match String::from_utf8(bytes) {
        Ok(text) => Ok(text),
        Err(e) if e.utf8_error().error_len().is_none() => {
            let valid = e.utf8_error().valid_up_to();
            Ok(String::from_utf8_lossy(e.as_bytes().get(..valid).unwrap_or_default()).into_owned())
        }
        Err(e) => decode_state_text(e.into_bytes()),
    }
}

/// The text of state bytes, with every bad byte an error. A cut write leaves an incomplete last
/// character, and that is an error too: a reader that cut it off would return a text that looks
/// complete, and a caller could write that text back. Each caller that parses JSON already fails
/// on a cut file, so the error costs no caller a working read.
///
/// # Errors
/// `InvalidData` for bytes that are not UTF-8, and it names the offset of the first bad byte.
fn decode_state_text(bytes: Vec<u8>) -> std::io::Result<String> {
    String::from_utf8(bytes).map_err(|e| {
        let error = e.utf8_error();
        let valid = error.valid_up_to();
        let cause = if error.error_len().is_none() {
            "it ends inside a UTF-8 character, so the file may be cut off"
        } else {
            "it is not UTF-8 text"
        };
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "{cause} (the first bad byte is at offset {valid}). Fix: save the file again as \
                 UTF-8, or remove it"
            ),
        )
    })
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
    fn a_cut_character_at_the_end_is_an_error_that_says_the_file_may_be_cut() {
        assert_eq!(decode_state_text(b"ok".to_vec()).unwrap(), "ok");
        let err = decode_state_text(b"ok \xe2\x82".to_vec()).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
        let text = err.to_string();
        assert!(text.contains("may be cut off"), "{text}");
        assert!(text.contains("offset 3"), "{text}");
        assert!(text.contains("Fix:"), "{text}");
        let err = decode_state_text(b"a\xffb".to_vec()).unwrap_err();
        assert!(err.to_string().contains("not UTF-8"), "{err}");
    }

    #[test]
    fn a_cut_state_file_is_refused_by_name_so_no_caller_reads_the_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pidfile");
        std::fs::write(&path, b"4242\xe2\x82").unwrap();
        let err = state_text_io(&path).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData, "{err}");
        assert!(err.to_string().contains("pidfile"), "{err}");
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
        /// For any bytes, the decode equals the std UTF-8 check: valid text is kept whole, an
        /// incomplete sequence at the very end is cut off, and any other bad byte is
        /// `InvalidData`.
        #[test]
        fn decoded_bytes_follow_the_std_utf8_check(
            bytes in proptest::collection::vec(proptest::prelude::any::<u8>(), 0..64),
        ) {
            match std::str::from_utf8(&bytes) {
                Ok(text) => {
                    proptest::prop_assert_eq!(decode_state_text(bytes.clone()).unwrap(), text);
                }
                Err(_) => {
                    let err = decode_state_text(bytes.clone()).unwrap_err();
                    proptest::prop_assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
                }
            }
        }

        /// Any cut of a saved JSON state file is an error, or the whole text when the cut falls on
        /// a character boundary. A cut file never reads as a shorter value (#3445, #3508).
        #[test]
        fn any_cut_state_file_is_refused_or_read_whole(
            values in proptest::collection::vec("[a-zé€😀]{0,4}", 1..5),
            cut in proptest::prelude::any::<proptest::sample::Index>(),
        ) {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("cut.json");
            let saved = serde_json::to_vec(&values).unwrap();
            let len = cut.index(saved.len());
            std::fs::write(&path, &saved[..len]).unwrap();
            match state_text_io(&path) {
                Ok(text) => proptest::prop_assert_eq!(text.as_bytes(), &saved[..len]),
                Err(e) => proptest::prop_assert_eq!(e.kind(), std::io::ErrorKind::InvalidData),
            }
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

    proptest::proptest! {
        /// The cut-tolerant decoder keeps valid text whole, drops only an incomplete last character,
        /// and refuses any other bad byte (#3508).
        #[test]
        fn cut_tolerant_decode_drops_only_an_incomplete_tail(
            bytes in proptest::collection::vec(proptest::prelude::any::<u8>(), 0..64),
        ) {
            match std::str::from_utf8(&bytes) {
                Ok(text) => {
                    proptest::prop_assert_eq!(decode_text_cut_at_end(bytes.clone()).unwrap(), text);
                }
                Err(e) if e.error_len().is_none() => {
                    let valid = &bytes[..e.valid_up_to()];
                    let kept = decode_text_cut_at_end(bytes.clone()).unwrap();
                    proptest::prop_assert_eq!(kept.as_bytes(), valid);
                }
                Err(_) => {
                    let err = decode_text_cut_at_end(bytes.clone()).unwrap_err();
                    proptest::prop_assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
                }
            }
        }
    }
}

#[cfg(all(test, unix))]
#[expect(clippy::unwrap_used, reason = "test code")]
mod site_tests;
