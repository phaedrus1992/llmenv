use super::*;
use proptest::prelude::*;

proptest! {
    /// Every stats value that `save_stats` writes is read back unchanged by `load_stats`.
    #[test]
    fn saved_stats_load_back_unchanged(
        paths in proptest::collection::btree_set("[a-zA-Z0-9/._-]{1,40}", 0..8),
        tools in proptest::collection::btree_map("[A-Za-z_]{1,12}", 0u64..1_000, 0..5),
    ) {
        let state = tempfile::tempdir().unwrap();
        let stats = SessionStats { paths: paths.clone(), tools: tools.clone() };

        save_stats(state.path(), "s1", &stats);
        let loaded = load_stats(state.path(), "s1");

        prop_assert_eq!(loaded.paths, paths);
        prop_assert_eq!(loaded.tools, tools);
    }

    /// Bytes that are not a stats log are kept byte for byte when a rewrite moves them aside.
    #[test]
    fn a_corrupt_log_is_kept_byte_for_byte(
        bytes in proptest::collection::vec(any::<u8>(), 1..200),
    ) {
        prop_assume!(serde_json::from_slice::<SessionStats>(&bytes).is_err());
        let state = tempfile::tempdir().unwrap();
        let log = stats_path(state.path(), "s1");
        std::fs::create_dir_all(log.parent().unwrap()).unwrap();
        std::fs::write(&log, &bytes).unwrap();

        prop_assert!(load_stats_for_rewrite(state.path(), "s1", true).is_some());

        let kept = format!("{}.corrupt", log.display());
        prop_assert_eq!(std::fs::read(&kept).unwrap(), bytes);
    }
}
