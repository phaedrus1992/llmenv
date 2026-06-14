#![expect(clippy::unwrap_used, reason = "test scaffolding")]
#![expect(clippy::expect_used, reason = "test scaffolding")]
#![expect(clippy::panic, reason = "test scaffolding")]
//! Tests for #59: the Claude Code adapter renders resolved plugins +
//! marketplaces into `settings.json` (`enabledPlugins` and
//! `extraKnownMarketplaces`).

use std::collections::BTreeMap;
use std::path::PathBuf;

use llmenv::adapter::AgentAdapter;
use llmenv::adapter::claude_code::ClaudeCodeAdapter;
use llmenv::merge::{BundleRef, merge};
use llmenv::plugins::resolve::{ResolvedMarketplace, ResolvedPlugin};
use tempfile::tempdir;

fn fixture_bundle(name: &str) -> BundleRef {
    BundleRef {
        name: name.into(),
        path: PathBuf::from(format!("tests/fixtures/bundles/{name}")),
        precedence: 1,
    }
}

fn empty_native() -> BTreeMap<String, serde_yaml::Value> {
    BTreeMap::new()
}

fn read_settings(out: &std::path::Path) -> serde_json::Value {
    let s = std::fs::read_to_string(out.join("settings.json")).expect("read settings.json");
    serde_json::from_str(&s).expect("parse settings.json")
}

fn plugin(marketplace: &str, name: &str, collection: &str) -> ResolvedPlugin {
    ResolvedPlugin {
        marketplace: marketplace.into(),
        plugin: name.into(),
        collection: collection.into(),
        install_path: None,
        git_commit_sha: None,
    }
}

fn marketplace(name: &str, location: &str, head: Option<&str>) -> ResolvedMarketplace {
    ResolvedMarketplace {
        name: name.into(),
        source: format!("https://github.com/example/{name}"),
        install_location: Some(location.into()),
        head: head.map(Into::into),
    }
}

#[test]
fn enabled_plugins_keyed_plugin_at_marketplace() {
    let mut m = merge(
        &llmenv::config::Capabilities::default(),
        &empty_native(),
        &[fixture_bundle("base")],
    )
    .expect("merge");
    m.plugins = vec![plugin("superpowers", "caveman", "core")];
    m.marketplaces = vec![marketplace(
        "superpowers",
        "/cache/marketplaces/superpowers",
        Some("abc"),
    )];
    let tmp = tempdir().expect("tempdir");

    ClaudeCodeAdapter
        .materialize(&m, tmp.path())
        .expect("materialize");

    let v = read_settings(tmp.path());
    assert_eq!(
        v["enabledPlugins"]["caveman@superpowers"].as_bool(),
        Some(true)
    );
}

#[test]
fn marketplace_rendered_as_directory_source_at_install_location() {
    let mut m = merge(
        &llmenv::config::Capabilities::default(),
        &empty_native(),
        &[fixture_bundle("base")],
    )
    .expect("merge");
    m.plugins = vec![plugin("superpowers", "caveman", "core")];
    m.marketplaces = vec![marketplace(
        "superpowers",
        "/cache/marketplaces/superpowers",
        Some("abc"),
    )];
    let tmp = tempdir().expect("tempdir");

    ClaudeCodeAdapter
        .materialize(&m, tmp.path())
        .expect("materialize");

    let v = read_settings(tmp.path());
    let entry = v["extraKnownMarketplaces"]["superpowers"].clone();
    assert_eq!(entry["source"]["source"].as_str(), Some("directory"));
    assert_eq!(
        entry["source"]["path"].as_str(),
        Some("/cache/marketplaces/superpowers")
    );
}

#[test]
fn unsynced_marketplace_is_skipped() {
    let mut m = merge(
        &llmenv::config::Capabilities::default(),
        &empty_native(),
        &[fixture_bundle("base")],
    )
    .expect("merge");
    // No install_location → never synced → must not be rendered.
    m.marketplaces = vec![ResolvedMarketplace {
        name: "ghost".into(),
        source: "https://example.com/ghost".into(),
        install_location: None,
        head: None,
    }];
    let tmp = tempdir().expect("tempdir");

    ClaudeCodeAdapter
        .materialize(&m, tmp.path())
        .expect("materialize");

    let v = read_settings(tmp.path());
    assert!(
        v.get("extraKnownMarketplaces").is_none(),
        "an unsynced marketplace must not appear in settings"
    );
}

#[test]
fn no_plugin_keys_when_manifest_empty() {
    let m = merge(
        &llmenv::config::Capabilities::default(),
        &empty_native(),
        &[fixture_bundle("base")],
    )
    .expect("merge");
    let tmp = tempdir().expect("tempdir");

    ClaudeCodeAdapter
        .materialize(&m, tmp.path())
        .expect("materialize");

    let v = read_settings(tmp.path());
    assert!(v.get("enabledPlugins").is_none());
    assert!(v.get("extraKnownMarketplaces").is_none());
}

#[test]
fn multiple_plugins_across_marketplaces() {
    let mut m = merge(
        &llmenv::config::Capabilities::default(),
        &empty_native(),
        &[fixture_bundle("base")],
    )
    .expect("merge");
    m.plugins = vec![
        plugin("superpowers", "caveman", "core"),
        plugin("dev-commons", "nbl-dev", "extra"),
    ];
    m.marketplaces = vec![
        marketplace("superpowers", "/cache/marketplaces/superpowers", Some("a")),
        marketplace("dev-commons", "/cache/marketplaces/dev-commons", Some("b")),
    ];
    let tmp = tempdir().expect("tempdir");

    ClaudeCodeAdapter
        .materialize(&m, tmp.path())
        .expect("materialize");

    let v = read_settings(tmp.path());
    let enabled = v["enabledPlugins"].as_object().expect("enabledPlugins");
    assert_eq!(enabled.len(), 2);
    assert_eq!(enabled["caveman@superpowers"].as_bool(), Some(true));
    assert_eq!(enabled["nbl-dev@dev-commons"].as_bool(), Some(true));
    let markets = v["extraKnownMarketplaces"]
        .as_object()
        .expect("extraKnownMarketplaces");
    assert_eq!(markets.len(), 2);
}
