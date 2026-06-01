#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
use llmenv::config::Config;

#[test]
fn parses_fixture() {
    let s = std::fs::read_to_string("tests/fixtures/llmenv.yaml").unwrap();
    let cfg: Config = serde_yaml::from_str(&s).unwrap();
    assert_eq!(cfg.cache.sync_interval_minutes, 15);
    assert_eq!(cfg.scope.host.len(), 1);
    assert_eq!(cfg.scope.host[0].id, "fixed");
    assert_eq!(cfg.bundle.len(), 2);
    assert_eq!(cfg.host.len(), 1);
    assert_eq!(cfg.host["fixed"].addr, "fixed.local");
    assert_eq!(cfg.mcp.len(), 1);
    assert_eq!(cfg.mcp[0].name, "playwright");
    let mem = cfg
        .features
        .as_ref()
        .and_then(|f| f.memory.as_ref())
        .expect("memory block");
    assert_eq!(mem.server_host, "fixed");
    assert_eq!(mem.port, 7878);
    assert_eq!(cfg.marketplace.len(), 2);
    assert_eq!(cfg.marketplace[0].name, "superpowers");
    assert_eq!(cfg.plugin_collection.len(), 2);
    assert_eq!(cfg.plugin_collection[1].name, "rust-tools");
    assert_eq!(cfg.plugin_collection[1].plugins.len(), 2);
}

#[test]
fn rejects_duplicate_scope_ids() {
    let s = r#"
scope:
  host:
    - id: x
      match: { hostname: a }
    - id: x
      match: { hostname: b }
"#;
    let cfg: Config = serde_yaml::from_str(s).unwrap();
    assert!(cfg.validate().is_err());
}

#[test]
fn rejects_bundle_with_no_tags() {
    let s = r#"
bundle:
  - name: x
    tags: []
"#;
    let cfg: Config = serde_yaml::from_str(s).unwrap();
    assert!(cfg.validate().is_err());
}

#[test]
fn duplicate_scope_id_across_kinds_is_rejected() {
    let s = r#"
scope:
  host:
    - id: shared
      match: { hostname: a }
  user:
    - id: shared
      match: { user: b }
"#;
    let cfg: Config = serde_yaml::from_str(s).unwrap();
    assert!(cfg.validate().is_err());
}

#[test]
fn rejects_duplicate_bundle_names() {
    let s = r#"
bundle:
  - name: dup
    tags: [a]
  - name: dup
    tags: [b]
"#;
    let cfg: Config = serde_yaml::from_str(s).unwrap();
    assert!(cfg.validate().is_err());
}

#[test]
fn fixture_passes_validation() {
    let s = std::fs::read_to_string("tests/fixtures/llmenv.yaml").unwrap();
    let cfg: Config = serde_yaml::from_str(&s).unwrap();
    cfg.validate().expect("fixture should validate");
}
