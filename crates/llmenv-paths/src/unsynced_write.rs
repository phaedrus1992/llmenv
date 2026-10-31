//! The atomic owner-only write without the `fsync`.

use std::path::Path;

/// [`crate::write_owner_only_atomic`] without the `fsync`. The write is still atomic and
/// owner-only. A power loss can leave the file empty. Use it only for state whose reader treats an
/// empty or unparsable file as missing, and where that loss costs nothing that matters. Each
/// caller says why it accepts the loss. On macOS `fsync` is a full flush to the disk, which costs
/// milliseconds on every hook call.
///
/// # Errors
/// The same errors as [`crate::write_owner_only_atomic`].
pub fn write_owner_only_atomic_unsynced(path: &Path, content: &[u8]) -> std::io::Result<()> {
    crate::atomic_write::write_atomic(path, content, false)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    #[cfg(unix)]
    fn the_write_is_owner_only_and_replaces_the_file() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("state.json");
        write_owner_only_atomic_unsynced(&path, b"first").unwrap();
        write_owner_only_atomic_unsynced(&path, b"second").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"second");
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        let files = std::fs::read_dir(dir.path()).unwrap().count();
        assert_eq!(files, 1, "the temp file is renamed away");
    }
}
