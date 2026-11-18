use super::*;
use proptest::prelude::*;

fn valid_name() -> impl Strategy<Value = String> {
    "[a-zA-Z0-9_-]{1,24}"
}

// ===== #1143: classify_missing_memory() classification over combinatorial state =====

proptest! {
    // A non-empty `all_memory` yields `TagInactive` naming every entry's
    // `server_host` (deduped) whenever nothing is suppressed and every
    // firing bundle loaded cleanly — regardless of how many bundles
    // fired. `TagInactive` outranks `NoBundlesFired` (this test) but is
    // itself outranked by a skipped firing bundle (the test below).
    // A firing bundle `build_bundle_refs` couldn't load outranks a
    // declared-but-tag-inactive entry: the skipped bundle is a real
    // misconfiguration (its own `features.memory`, if any, was never
    // read), while a tag-inactive entry is often intentional.
    // With `all_memory` empty and nothing suppressed: `NoBundlesFired` iff
    // firing is empty; otherwise `NotDeclared`'s `skipped_bundles` is
    // exactly `firing` filtered down to the names `bundle_refs` didn't
    // load — matching production's `firing \ loaded` computation
    // (duplicates and order included, since production filters the
    // `firing` list itself rather than deduplicating first).
}

// Static MCPs resolve ahead of the memory backend, so an unnamed match
// would hand the hooks another server's URL.
#[test]
fn memory_url_skips_other_remote_mcps() {
    let config_root = tempfile::tempdir().expect("test");
    let bundle_dir = config_root.path().join("bundles").join("b");
    std::fs::create_dir_all(&bundle_dir).expect("test");
    std::fs::write(
        bundle_dir.join("bundle.yaml"),
        concat!(
            "features:\n",
            "  memory:\n",
            "    - server_host: still\n",
            "      port: 7878\n",
            "      when: [mytag]\n",
            "host:\n",
            "  still:\n",
            "    addr: still.local\n",
        ),
    )
    .expect("test");
    let cache_dir = tempfile::tempdir().expect("test");
    let other: crate::config::McpServer = serde_json::from_value(serde_json::json!({
        "name": "aaa-remote", "when": ["mytag"], "type": "http",
        "url": "http://other.invalid/mcp"
    }))
    .expect("test");
    let config = crate::config::Config {
        bundle: vec![crate::config::Bundle {
            name: "b".into(),
            when: vec!["mytag".into()],
        }],
        mcp: vec![other],
        cache: crate::config::Cache {
            cache_dir: cache_dir.path().to_string_lossy().into_owned(),
            ..Default::default()
        },
        ..Default::default()
    };
    let active = crate::scope::ActiveScopes {
        unknown_content: Default::default(),
        scopes: vec![],
        tags: std::collections::BTreeSet::from(["mytag".to_string()]),
        extra_tags: std::collections::BTreeSet::new(),
    };
    let url = memory_url(&config, config_root.path(), &active).expect("test");
    assert_eq!(
        url,
        MemoryEndpoint::Active {
            url: "http://still.local:7878/mcp".into(),
            settings: default_memory_hook(),
        }
    );
}

// #1133: the detached memory children were spawned with
// `stderr(Stdio::null())`, so nothing they reported could reach anyone —
// including the `tracing` events meant to compensate, whose sink is that
// same discarded stderr.

// ===== #1143: classify_missing_memory() classification over combinatorial state =====

/// A minimal but fully-populated `Memory` entry naming `server_host` —
/// `classify_missing_memory` only reads `server_host` off `all_memory`,
/// but the struct has no `Default` shorthand for the rest.
fn memory_with_host(server_host: &str) -> crate::config::Memory {
    crate::config::Memory {
        server_host: server_host.to_string(),
        port: 7878,
        listen_host: "127.0.0.1".into(),
        global_topics: vec![],
        ..crate::config::Memory::default()
    }
}

proptest! {
    // A non-empty `all_memory` yields `TagInactive` naming every entry's
    // `server_host` (deduped) whenever nothing is suppressed and every
    // firing bundle loaded cleanly — regardless of how many bundles
    // fired. `TagInactive` outranks `NoBundlesFired` (this test) but is
    // itself outranked by a skipped firing bundle (the test below).
    #[test]
    fn prop_classify_missing_memory_tag_inactive_wins_regardless_of_firing(
        server_hosts in proptest::collection::vec(valid_name(), 1..5),
        firing_names in proptest::collection::vec(valid_name(), 0..5),
    ) {
        let config = crate::config::Config::default();
        let config_dir = std::path::Path::new("/nonexistent");
        let active = crate::scope::ActiveScopes::default();
        let bundles: Vec<crate::config::Bundle> = firing_names
            .iter()
            .map(|n| crate::config::Bundle { name: n.clone(), when: vec![] })
            .collect();
        let firing: Vec<&crate::config::Bundle> = bundles.iter().collect();
        // Every firing bundle "loads" successfully (a matching `BundleRef`
        // for each name), so `skipped_bundles` stays empty and this
        // isolates the `TagInactive`-vs-`NoBundlesFired` priority — a
        // skipped bundle outranks `TagInactive` (see the test below), so
        // it must not leak into this one.
        let bundle_refs: Vec<crate::merge::BundleRef> = firing_names
            .iter()
            .map(|n| crate::merge::BundleRef {
                name: n.clone(),
                path: std::path::PathBuf::new(),
                precedence: 0,
            })
            .collect();
        let all_memory: Vec<crate::config::Memory> =
            server_hosts.iter().map(|h| memory_with_host(h)).collect();

        let result = classify_missing_memory(
            &config,
            config_dir,
            &active,
            &firing,
            &bundle_refs,
            &all_memory,
        );
        // `classify_missing_memory` dedups `server_hosts` (two entries can
        // legitimately share a `server_host`), so compare against the same
        // deduped/sorted form rather than `server_hosts` as generated.
        let expected: Vec<String> = server_hosts
            .iter()
            .cloned()
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect();
        prop_assert_eq!(
            result,
            MemoryEndpoint::TagInactive { server_hosts: expected }
        );
    }

    // A firing bundle `build_bundle_refs` couldn't load outranks a
    // declared-but-tag-inactive entry: the skipped bundle is a real
    // misconfiguration (its own `features.memory`, if any, was never
    // read), while a tag-inactive entry is often intentional.
    #[test]
    fn prop_classify_missing_memory_skipped_bundle_outranks_tag_inactive(
        server_hosts in proptest::collection::vec(valid_name(), 1..5),
        skipped_name in valid_name(),
    ) {
        let config = crate::config::Config::default();
        let config_dir = std::path::Path::new("/nonexistent");
        let active = crate::scope::ActiveScopes::default();
        let bundles = [crate::config::Bundle { name: skipped_name.clone(), when: vec![] }];
        let firing: Vec<&crate::config::Bundle> = bundles.iter().collect();
        let all_memory: Vec<crate::config::Memory> =
            server_hosts.iter().map(|h| memory_with_host(h)).collect();

        // No bundle_refs at all: `skipped_name` is firing but unloaded.
        let result =
            classify_missing_memory(&config, config_dir, &active, &firing, &[], &all_memory);
        prop_assert_eq!(
            result,
            MemoryEndpoint::NotDeclared { skipped_bundles: vec![skipped_name] }
        );
    }

    // With `all_memory` empty and nothing suppressed: `NoBundlesFired` iff
    // firing is empty; otherwise `NotDeclared`'s `skipped_bundles` is
    // exactly `firing` filtered down to the names `bundle_refs` didn't
    // load — matching production's `firing \ loaded` computation
    // (duplicates and order included, since production filters the
    // `firing` list itself rather than deduplicating first).
    #[test]
    fn prop_classify_missing_memory_skipped_bundles_is_firing_minus_loaded(
        firing_with_loaded in proptest::collection::vec((valid_name(), any::<bool>()), 0..6),
    ) {
        let config = crate::config::Config::default();
        let config_dir = std::path::Path::new("/nonexistent");
        let active = crate::scope::ActiveScopes::default();
        let firing_names: Vec<String> =
            firing_with_loaded.iter().map(|(n, _)| n.clone()).collect();
        let bundles: Vec<crate::config::Bundle> = firing_names
            .iter()
            .map(|n| crate::config::Bundle { name: n.clone(), when: vec![] })
            .collect();
        let firing: Vec<&crate::config::Bundle> = bundles.iter().collect();
        let loaded_names: std::collections::HashSet<&str> = firing_with_loaded
            .iter()
            .filter(|(_, keep)| *keep)
            .map(|(n, _)| n.as_str())
            .collect();
        let bundle_refs: Vec<crate::merge::BundleRef> = loaded_names
            .iter()
            .map(|n| crate::merge::BundleRef {
                name: (*n).to_string(),
                path: std::path::PathBuf::new(),
                precedence: 0,
            })
            .collect();

        let result =
            classify_missing_memory(&config, config_dir, &active, &firing, &bundle_refs, &[]);

        if firing_names.is_empty() {
            prop_assert_eq!(result, MemoryEndpoint::NoBundlesFired);
        } else {
            let MemoryEndpoint::NotDeclared { skipped_bundles } = result else {
                return Err(proptest::test_runner::TestCaseError::fail(format!(
                    "expected NotDeclared for non-empty firing, got {result:?}"
                )));
            };
            // Independent membership invariants rather than recomputing
            // production's `firing \ loaded` formula (#1141 pre-pr-review
            // finding): every skipped name is a firing bundle that wasn't
            // loaded, and every unloaded firing bundle is reported.
            for name in &skipped_bundles {
                prop_assert!(
                    firing_names.contains(name),
                    "skipped name {name:?} must be one of the firing bundles"
                );
                prop_assert!(
                    !loaded_names.contains(name.as_str()),
                    "skipped name {name:?} must not be among the loaded bundle_refs"
                );
            }
            for name in &firing_names {
                if !loaded_names.contains(name.as_str()) {
                    prop_assert!(
                        skipped_bundles.contains(name),
                        "unloaded firing bundle {name:?} must appear in skipped_bundles"
                    );
                }
            }
        }
    }
}
