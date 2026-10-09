#![expect(clippy::expect_used, reason = "test scaffolding")]
//! #2513: `llmenv plugin-sync` aborted on a path marketplace whose checkout is missing on
//! this host, so marketplaces declared after it were never synced.
//!
//! A marketplace is synced only when an active plugin-collection selects it (#2615), so
//! the fixture selects a plugin from each marketplace under a tag that is always active.

mod support;

use std::fs;

use support::isolated_llmenv_cmd;

#[test]
fn plugin_sync_skips_a_missing_path_marketplace_and_syncs_the_rest() {
    let tmp = tempfile::TempDir::new().expect("temp dir");
    let present = tmp.path().join("present-checkout");
    fs::create_dir_all(present.join(".claude-plugin")).expect("create checkout");
    fs::write(
        present.join(".claude-plugin").join("marketplace.json"),
        r#"{"name": "here", "plugins": [{"name": "tool", "source": "./tool"}]}"#,
    )
    .expect("write manifest");
    let config = format!(
        r"
cache:
  cache_dir: {cache}
marketplace:
  - name: elsewhere
    source: {missing}
  - name: here
    source: {present}
plugin-collection:
  - name: always
    when: [{os}]
    plugins: [elsewhere:tool, here:tool]
",
        cache = tmp.path().join("cache").display(),
        missing = tmp.path().join("no-such-checkout").display(),
        present = present.display(),
        os = std::env::consts::OS,
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
        stdout.contains("✓ here"),
        "later marketplace not synced: {stdout}"
    );
    assert!(
        stderr.contains("skipping marketplace 'elsewhere'"),
        "no skip warning: {stderr}"
    );
    assert!(
        stderr.contains("1 marketplace(s) skipped") && stderr.contains("elsewhere"),
        "no skip summary: {stderr}"
    );
    assert!(
        stderr.contains("tool@elsewhere: skipped"),
        "a plugin of the skipped marketplace is not noted: {stderr}"
    );
}
