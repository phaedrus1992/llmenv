#![expect(clippy::expect_used, reason = "test scaffolding")]
//! #2513: `llmenv plugin-sync` aborted on a path marketplace whose checkout is missing on
//! this host, so marketplaces declared after it were never synced.

mod support;

use std::fs;

use support::isolated_llmenv_cmd;

#[test]
fn plugin_sync_skips_a_missing_path_marketplace_and_syncs_the_rest() {
    let tmp = tempfile::TempDir::new().expect("temp dir");
    let present = tmp.path().join("present-checkout");
    fs::create_dir_all(&present).expect("create checkout");
    let config = format!(
        r"
cache:
  cache_dir: {cache}
marketplace:
  - name: elsewhere
    source: {missing}
  - name: here
    source: {present}
",
        cache = tmp.path().join("cache").display(),
        missing = tmp.path().join("no-such-checkout").display(),
        present = present.display(),
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
}
