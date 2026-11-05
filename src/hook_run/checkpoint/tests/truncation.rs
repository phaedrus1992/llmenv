//! A cut-off checkpoint file, the shape a crash or a power loss leaves (#3130).

use super::super::*;
use super::cp;

/// Each strict prefix of a saved checkpoint, the empty file included, fails to load and lists as
/// an unreadable entry that names the file, so the resume path never runs a half-read job.
#[test]
fn every_cut_off_checkpoint_lists_as_unreadable() {
    let dir = tempfile::tempdir().unwrap();
    let path = write(dir.path(), &cp(JobKind::IcmStore, 7))
        .unwrap()
        .unwrap();
    let saved = std::fs::read_to_string(&path).unwrap();
    let saved = saved.trim_end();
    assert!(load(&path).is_ok());
    for len in 0..saved.len() {
        std::fs::write(&path, &saved[..len]).unwrap();
        let err = format!("{:#}", load(&path).unwrap_err());
        assert!(err.contains("parsing checkpoint"), "{len}: {err}");
        let entries = list(dir.path());
        let listed = matches!(
            entries.as_slice(),
            [Entry::Unreadable(listed, why)] if *listed == path && why.contains("parsing checkpoint")
        );
        assert!(listed, "prefix of {len} bytes listed as {entries:?}");
    }
}
