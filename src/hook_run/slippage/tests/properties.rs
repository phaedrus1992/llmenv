use super::*;
use proptest::prelude::*;

proptest! {
    /// Any mix of Bash reads and other Bash calls keeps every read and counts every call. The
    /// read record and the metric share one load of the stats log (#3373).
    #[test]
    fn every_bash_call_is_counted_and_every_read_is_kept(
        ops in proptest::collection::vec(proptest::option::of(0usize..3), 0..12),
    ) {
        let state = tempfile::tempdir().unwrap();
        let work = tempfile::tempdir().unwrap();
        let names = ["a.txt", "b.txt", "c.txt"];
        for name in names {
            std::fs::write(work.path().join(name), "x").unwrap();
        }
        let cfg = crate::config::SlippageControl {
            enabled: true,
            read_before_edit: true,
            metrics: true,
            ..Default::default()
        };
        let mut expected_paths = std::collections::BTreeSet::new();
        for op in &ops {
            let command = match op {
                Some(i) => {
                    let file = work.path().join(names[*i]).to_string_lossy().into_owned();
                    expected_paths.insert(path_key(&file));
                    format!("cat {}", names[*i])
                }
                None => "ls".to_string(),
            };
            let payload = serde_json::json!({
                "tool_name": "Bash",
                "tool_input": { "command": command },
                "cwd": work.path(),
            });
            handle_post_tool_use(Some(&cfg), &payload, Some("s1"), state.path());
        }

        let stats = load_stats(state.path(), "s1");
        prop_assert_eq!(stats.paths, expected_paths);
        prop_assert_eq!(stats.tools.get("Bash").copied().unwrap_or(0), ops.len() as u64);
    }
}

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

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]
    /// Every alias of one existing file gets one key: a `.` segment, a `dir/..` detour, a link to
    /// the file, and the key itself. A missing path keys as written (#3389).
    #[test]
    fn every_alias_of_a_file_shares_one_key(name in "[a-z]{1,8}", missing in "[a-z]{1,8}") {
        let work = tempfile::tempdir().unwrap();
        let sub = work.path().join("sub");
        std::fs::create_dir(&sub).unwrap();
        let file = sub.join(format!("{name}.txt"));
        std::fs::write(&file, "x").unwrap();
        let key = path_key(&file.to_string_lossy());
        let root = work.path().to_string_lossy().into_owned();
        let dotted = format!("{root}/./sub/{name}.txt");
        let detour = format!("{root}/sub/../sub/{name}.txt");
        prop_assert_eq!(&path_key(&dotted), &key);
        prop_assert_eq!(&path_key(&detour), &key);
        prop_assert_eq!(&path_key(&key), &key);
        #[cfg(unix)]
        {
            let link = work.path().join("link");
            std::os::unix::fs::symlink(&file, &link).unwrap();
            prop_assert_eq!(&path_key(&link.to_string_lossy()), &key);
        }
        let absent = format!("{root}/sub/{missing}.absent");
        prop_assert_eq!(path_key(&absent), absent);
    }
}
