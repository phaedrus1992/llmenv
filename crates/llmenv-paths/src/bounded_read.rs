//! Bounded reads of a regular file (#2933, #2942).
//!
//! A file that a user or a cloned repository controls can be a FIFO, which blocks a plain open
//! forever, or a file large enough to exhaust memory. These readers open without blocking, accept
//! only a regular file, and stop at a byte cap. The checks and the read use the same open file, so
//! a swap between them cannot change what is read.

use std::io::{self, Read as _};
use std::path::Path;

/// Read the regular file at `path`, or `None` when there is no file there. A link is followed. A
/// link to a missing file is an error, because the link shows that the file was meant to exist.
///
/// # Errors
/// The file cannot be opened or read, is not a regular file, or holds more than `max_bytes`
/// bytes. The message gives the reason only, so the caller must add the path.
pub fn read_file_bounded(path: &Path, max_bytes: u64) -> io::Result<Option<Vec<u8>>> {
    match open_nonblocking(path) {
        Ok(file) => read_open_file(file, max_bytes).map(Some),
        Err(e) if e.kind() == io::ErrorKind::NotFound => match std::fs::symlink_metadata(path) {
            Ok(_) => Err(invalid(
                "it is a link to a file that does not exist; correct or remove the link",
            )),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        },
        Err(e) => Err(e),
    }
}

/// [`read_file_bounded`] for a UTF-8 text file.
///
/// # Errors
/// The errors of [`read_file_bounded`], and `InvalidData` with the offset of the first bad byte
/// when the file is not UTF-8.
pub fn read_text_bounded(path: &Path, max_bytes: u64) -> io::Result<Option<String>> {
    let Some(bytes) = read_file_bounded(path, max_bytes)? else {
        return Ok(None);
    };
    String::from_utf8(bytes).map(Some).map_err(|e| {
        invalid(&format!(
            "it is not UTF-8 text (the first bad byte is at offset {}); save it as UTF-8",
            e.utf8_error().valid_up_to()
        ))
    })
}

/// Read all of an open file that must be a regular file of at most `max_bytes` bytes. The caller
/// opens it, so that it can choose the open flags, such as `O_NOFOLLOW` relative to a folder.
/// On Unix the caller must open with `O_NONBLOCK`, because the open of a FIFO otherwise blocks
/// before this check can run.
///
/// # Errors
/// The file is not a regular file, holds more than `max_bytes` bytes, or cannot be read.
pub fn read_open_file(file: std::fs::File, max_bytes: u64) -> io::Result<Vec<u8>> {
    check_regular_file(&file)?;
    let mut bytes = Vec::new();
    file.take(max_bytes.saturating_add(1))
        .read_to_end(&mut bytes)?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > max_bytes {
        return Err(invalid(&format!(
            "it holds more than {max_bytes} bytes; make the file smaller"
        )));
    }
    Ok(bytes)
}

/// Fail unless the open `file` is a regular file.
///
/// # Errors
/// `InvalidData` that names the file type, or the error of the `fstat`.
pub fn check_regular_file(file: &std::fs::File) -> io::Result<()> {
    let kind = file.metadata()?.file_type();
    if kind.is_file() {
        return Ok(());
    }
    Err(invalid(&format!(
        "it is {}, not a regular file; replace it with a regular file",
        kind_name(kind)
    )))
}

#[cfg(unix)]
fn kind_name(kind: std::fs::FileType) -> &'static str {
    use std::os::unix::fs::FileTypeExt as _;
    if kind.is_dir() {
        "a folder"
    } else if kind.is_fifo() {
        "a FIFO"
    } else if kind.is_socket() {
        "a socket"
    } else if kind.is_char_device() || kind.is_block_device() {
        "a device"
    } else {
        "a special file"
    }
}

#[cfg(not(unix))]
fn kind_name(kind: std::fs::FileType) -> &'static str {
    if kind.is_dir() {
        "a folder"
    } else {
        "a special file"
    }
}

#[cfg(unix)]
fn open_nonblocking(path: &Path) -> io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(rustix::fs::OFlags::NONBLOCK.bits().cast_signed())
        .open(path)
}

#[cfg(not(unix))]
fn open_nonblocking(path: &Path) -> io::Result<std::fs::File> {
    std::fs::File::open(path)
}

fn invalid(reason: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, reason.to_string())
}

#[cfg(all(test, unix))]
#[expect(clippy::unwrap_used, reason = "test code")]
mod tests {
    use super::*;

    const CAP: u64 = 64;

    fn mkfifo(path: &Path) {
        let made = std::process::Command::new("mkfifo")
            .arg(path)
            .status()
            .unwrap();
        assert!(made.success());
    }

    #[test]
    fn a_missing_file_is_none_and_a_text_file_is_read() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.yaml");
        assert_eq!(read_text_bounded(&path, CAP).unwrap(), None);
        std::fs::write(&path, "id: x").unwrap();
        assert_eq!(
            read_text_bounded(&path, CAP).unwrap().as_deref(),
            Some("id: x")
        );
    }

    #[test]
    fn a_link_to_a_file_is_followed() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("real"), "shared").unwrap();
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(dir.path().join("real"), &link).unwrap();
        assert_eq!(
            read_text_bounded(&link, CAP).unwrap().as_deref(),
            Some("shared")
        );
    }

    #[test]
    fn a_dangling_link_a_folder_and_bad_text_are_errors() {
        let dir = tempfile::tempdir().unwrap();
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(dir.path().join("moved"), &link).unwrap();
        let err = read_file_bounded(&link, CAP).unwrap_err();
        assert!(err.to_string().contains("does not exist"), "{err}");
        let err = read_file_bounded(dir.path(), CAP).unwrap_err();
        assert!(
            err.to_string()
                .contains("it is a folder, not a regular file"),
            "{err}"
        );
        let bad = dir.path().join("bad");
        std::fs::write(&bad, b"ok\xffno").unwrap();
        let err = read_text_bounded(&bad, CAP).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(err.to_string().contains("offset 2"), "{err}");
    }

    #[test]
    fn a_fifo_fails_at_once_instead_of_blocking() {
        let dir = tempfile::tempdir().unwrap();
        let fifo = dir.path().join("fifo");
        mkfifo(&fifo);
        let err = read_file_bounded(&fifo, CAP).unwrap_err();
        assert!(
            err.to_string().contains("it is a FIFO, not a regular file"),
            "{err}"
        );
    }

    #[test]
    fn a_file_over_the_cap_fails_and_a_file_at_the_cap_is_read() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("big");
        let cap = usize::try_from(CAP).unwrap();
        std::fs::write(&path, vec![b'a'; cap + 1]).unwrap();
        let err = read_file_bounded(&path, CAP).unwrap_err();
        assert!(err.to_string().contains("more than 64 bytes"), "{err}");
        std::fs::write(&path, vec![b'a'; cap]).unwrap();
        assert_eq!(
            read_file_bounded(&path, CAP).unwrap().map(|b| b.len()),
            Some(cap)
        );
    }

    proptest::proptest! {
        #[test]
        fn a_file_reads_exactly_when_its_length_is_within_the_cap(
            len in 0usize..=256,
            max_bytes in 0u64..=256,
        ) {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("f");
            std::fs::write(&path, vec![b'x'; len]).unwrap();
            let within = u64::try_from(len).unwrap() <= max_bytes;
            match read_file_bounded(&path, max_bytes) {
                Ok(Some(bytes)) => {
                    proptest::prop_assert!(within);
                    proptest::prop_assert_eq!(bytes.len(), len);
                }
                Err(e) => {
                    proptest::prop_assert!(!within, "{e}");
                    proptest::prop_assert!(e.to_string().contains("more than"), "{e}");
                }
                Ok(None) => proptest::prop_assert!(false, "the file exists"),
            }
        }

        #[test]
        fn small_files_read_back_exactly_or_fail_as_bad_text(
            bytes in proptest::collection::vec(proptest::prelude::any::<u8>(), 0..512),
        ) {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("f");
            std::fs::write(&path, &bytes).unwrap();
            match (read_text_bounded(&path, 512), std::str::from_utf8(&bytes)) {
                (Ok(Some(text)), Ok(expected)) => proptest::prop_assert_eq!(text.as_str(), expected),
                (Err(e), Err(_)) => {
                    proptest::prop_assert_eq!(e.kind(), io::ErrorKind::InvalidData);
                }
                (got, want) => proptest::prop_assert!(false, "{got:?} vs {want:?}"),
            }
        }
    }
}
