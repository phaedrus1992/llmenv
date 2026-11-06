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

proptest::proptest! {
    /// For any saved checkpoint, any strict prefix fails to load and lists as unreadable.
    #[test]
    fn any_cut_off_checkpoint_lists_as_unreadable(
        index_job in proptest::prelude::any::<bool>(),
        n in proptest::num::u32::ANY,
        cut in proptest::prelude::any::<proptest::sample::Index>(),
    ) {
        let kind = if index_job { JobKind::CbmIndex } else { JobKind::IcmStore };
        let dir = tempfile::tempdir().unwrap();
        let path = write(dir.path(), &cp(kind, n)).unwrap().unwrap();
        let saved = std::fs::read_to_string(&path).unwrap();
        let saved = saved.trim_end();
        let len = cut.index(saved.len());
        std::fs::write(&path, &saved.as_bytes()[..len]).unwrap();
        proptest::prop_assert!(load(&path).is_err());
        let entries = list(dir.path());
        proptest::prop_assert!(
            matches!(entries.as_slice(), [Entry::Unreadable(listed, _)] if *listed == path),
            "{:?}",
            entries
        );
    }
}
