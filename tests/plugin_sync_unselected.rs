#![expect(clippy::expect_used, reason = "test scaffolding")]
//! #2615: `llmenv plugin-sync` fetched a marketplace and failed on a plugin that no active
//! plugin-collection selects. Unselected marketplaces and plugins are not fetched or checked.

mod support;

use std::fs;

use support::isolated_llmenv_cmd;

#[test]
fn plugin_sync_ignores_marketplace_of_an_inactive_collection() {
    let tmp = tempfile::TempDir::new().expect("temp dir");
    let checkout = tmp.path().join("switchboard-checkout");
    fs::create_dir_all(&checkout).expect("create checkout");
    let config = format!(
        r"
cache:
  cache_dir: {cache}
marketplace:
  - name: switchboard
    source: {checkout}
plugin-collection:
  - name: work
    when: [no-such-profile]
    plugins: [switchboard:switchboard]
",
        cache = tmp.path().join("cache").display(),
        checkout = checkout.display(),
    );
    fs::write(tmp.path().join("config.yaml"), config).expect("write config");

    let output = isolated_llmenv_cmd(tmp.path())
        .arg("plugin-sync")
        .output()
        .expect("run llmenv plugin-sync");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "plugin-sync failed: {stderr}");
    assert!(
        !stdout.contains("switchboard"),
        "inactive marketplace was synced: {stdout}"
    );
    assert!(
        !stderr.contains("not found"),
        "inactive plugin was validated: {stderr}"
    );
}
