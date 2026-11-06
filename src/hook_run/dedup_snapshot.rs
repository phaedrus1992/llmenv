//! The SessionEnd dedup snapshot: where it lives, and whether a store writes it.

use super::HookEvent;

/// The SessionEnd dedup snapshot path. Other events have none. A missing state directory skips
/// the snapshot and logs, because an error return would drop the notices built above (#2719).
pub(super) fn dedup_snapshot_path(
    event: HookEvent,
    state_dir: impl FnOnce() -> anyhow::Result<std::path::PathBuf>,
) -> Option<std::path::PathBuf> {
    if event != HookEvent::SessionEnd {
        return None;
    }
    match state_dir() {
        Ok(dir) => Some(dir.join(crate::paths::HOOK_STORE_CHUNK)),
        Err(e) => {
            crate::agent_notice::no_state_folder(
                "the session-end duplicate check is skipped, so the context chunk is stored again",
                &e,
            );
            None
        }
    }
}

/// The dedup snapshot to write after a store. A chunk the store already matched is not written
/// again, so the snapshot keeps its earlier content.
pub(super) fn dedup_write_target(
    path: Option<&std::path::Path>,
    unchanged: bool,
) -> Option<&std::path::Path> {
    path.filter(|_| !unchanged)
}

/// The last stored SessionEnd chunk, or `None`. A missing file is the normal first run, so only
/// another read error is logged.
pub(super) fn read_dedup_snapshot(path: &std::path::Path) -> Option<String> {
    crate::paths::bounded::state_text_io(path)
        .inspect_err(|e| {
            if e.kind() != std::io::ErrorKind::NotFound {
                crate::agent_notice::record_logged(format!(
                    "llmenv: the session-end duplicate record {} cannot be read, so the chunk \
                     can be stored again: {e}. Fix: remove the file.",
                    path.display()
                ));
            }
        })
        .ok()
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "test code")]
mod tests {
    use super::read_dedup_snapshot;

    #[test]
    fn a_cut_snapshot_reads_as_the_text_before_the_cut_character() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("session_end_dedup");
        std::fs::write(&path, b"chunk \xe2\x82").expect("write snapshot");
        assert_eq!(read_dedup_snapshot(&path).as_deref(), Some("chunk "));
    }

    #[test]
    fn the_dedup_snapshot_reads_the_stored_chunk_and_a_missing_file_as_none() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("session_end_dedup");
        assert_eq!(
            read_dedup_snapshot(&path),
            None,
            "a first run has no snapshot"
        );
        std::fs::write(&path, "stored chunk").expect("write snapshot");
        assert_eq!(read_dedup_snapshot(&path).as_deref(), Some("stored chunk"));
    }

    proptest::proptest! {
        /// A stored chunk reads back whole, and any cut of it reads as a prefix of the chunk,
        /// never as `None`, so a cut snapshot is compared and replaced, not lost.
        #[test]
        fn a_stored_chunk_or_any_cut_of_it_reads_as_a_prefix(
            chunk in "\\PC{0,40}",
            cut in proptest::prelude::any::<proptest::sample::Index>(),
        ) {
            let dir = tempfile::tempdir().expect("tempdir");
            let path = dir.path().join("session_end_dedup");
            std::fs::write(&path, &chunk).expect("write snapshot");
            proptest::prop_assert_eq!(read_dedup_snapshot(&path), Some(chunk.clone()));
            let bytes = chunk.as_bytes();
            std::fs::write(&path, &bytes[..cut.index(bytes.len() + 1)]).expect("write cut");
            let read = read_dedup_snapshot(&path).expect("a cut snapshot still reads");
            proptest::prop_assert!(chunk.starts_with(&read));
        }
    }
}
