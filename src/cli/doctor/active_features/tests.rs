use super::*;

fn scopes(tag: &str) -> ActiveScopes {
    ActiveScopes {
        scopes: vec![crate::scope::ActiveScope {
            id: "host".to_string(),
            kind: "host",
            tags: vec![tag.to_string()],
            ..crate::scope::ActiveScope::default()
        }],
        tags: [tag.to_string()].into(),
        ..ActiveScopes::default()
    }
}

fn memory(when: &str, server_host: &str, backend: crate::config::ConsolidationBackend) -> Memory {
    Memory {
        always_load: None,
        server_host: server_host.into(),
        port: 4343,
        listen_host: "127.0.0.1".into(),
        when: vec![when.into()],
        default_topics: vec![],
        default_type: None,
        default_importance: None,
        type_importance: BTreeMap::new(),
        retention: None,
        auto_prune: false,
        consolidation: Some(crate::config::ConsolidationConfig {
            enabled: true,
            backend,
            max_rules_per_session: 10,
        }),
        mcp_permissions: None,
        wakeup_max_tokens: None,
        adaptive_recall: true,
        failure_recall: true,
        global_topics: vec![],
    }
}

fn config(memory: Vec<Memory>, codebase_memory: Vec<CodebaseMemory>) -> Config {
    Config {
        features: Some(crate::config::Features {
            memory,
            codebase_memory,
            ..Default::default()
        }),
        ..Config::default()
    }
}

fn cbm(when: &str) -> CodebaseMemory {
    CodebaseMemory {
        when: vec![when.to_string()],
        ..CodebaseMemory::default()
    }
}

#[test]
fn an_inactive_memory_entry_asks_for_no_proxy_and_no_model() {
    let api = crate::config::ConsolidationBackend::AnthropicApi;
    let cfg = config(vec![memory("work", "far", api)], vec![]);
    let home = ActiveFeatures::root_only(&cfg, &scopes("home"));
    assert!(home.memory.is_empty(), "the icm check is skipped");
    assert!(!home.has_remote_memory_host());
    assert_eq!(home.anthropic_model_warning(Ok("opus".into())), None);

    let work = ActiveFeatures::root_only(&cfg, &scopes("work"));
    assert_eq!(work.memory.len(), 1, "the icm check runs");
    assert!(work.has_remote_memory_host(), "an unknown host is remote");
    assert!(work.anthropic_model_warning(Ok("opus".into())).is_some());
}

#[test]
fn a_local_host_needs_no_proxy() {
    let cli = crate::config::ConsolidationBackend::ClaudeCli;
    let mut cfg = config(vec![memory("work", "here", cli)], vec![]);
    cfg.host.insert(
        "here".into(),
        HostEntry {
            addr: "127.0.0.1".into(),
        },
    );
    assert!(!ActiveFeatures::root_only(&cfg, &scopes("work")).has_remote_memory_host());
}

#[test]
fn an_inactive_codebase_memory_entry_needs_no_binary() {
    let cfg = config(vec![], vec![cbm("work")]);
    assert!(
        ActiveFeatures::root_only(&cfg, &scopes("home"))
            .codebase_memory
            .is_empty()
    );
    assert_eq!(
        ActiveFeatures::root_only(&cfg, &scopes("work"))
            .codebase_memory
            .len(),
        1
    );
}

#[test]
fn an_entry_that_only_a_firing_bundle_declares_is_checked() {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("bundles").join("cbm");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("bundle.yaml"),
        "features:\n  codebase_memory:\n    - when: [work]\n",
    )
    .unwrap();
    let cfg = Config {
        bundle: vec![crate::config::Bundle {
            name: "cbm".to_string(),
            when: vec!["work".to_string()],
        }],
        ..Config::default()
    };
    let work = ActiveFeatures::resolve(&cfg, root.path(), &scopes("work")).unwrap();
    assert_eq!(work.codebase_memory.len(), 1, "{work:?}");
    let home = ActiveFeatures::resolve(&cfg, root.path(), &scopes("home")).unwrap();
    assert!(home.codebase_memory.is_empty(), "{home:?}");
}

#[test]
fn the_model_warning_names_an_alias_and_passes_a_full_id() {
    let api = crate::config::ConsolidationBackend::AnthropicApi;
    let cfg = config(vec![memory("work", "far", api)], vec![]);
    let work = ActiveFeatures::root_only(&cfg, &scopes("work"));
    let warning = work.anthropic_model_warning(Ok("opus".into())).unwrap();
    assert!(
        warning.contains("\"opus\" is not a full model ID"),
        "{warning}"
    );
    assert!(warning.contains("anthropic-api backend"), "{warning}");
    assert_eq!(
        work.anthropic_model_warning(Ok("claude-sonnet-5".into())),
        None
    );
    let cli = crate::config::ConsolidationBackend::ClaudeCli;
    let cfg = config(vec![memory("work", "far", cli)], vec![]);
    let cli_only = ActiveFeatures::root_only(&cfg, &scopes("work"));
    assert_eq!(cli_only.anthropic_model_warning(Ok("opus".into())), None);
}
