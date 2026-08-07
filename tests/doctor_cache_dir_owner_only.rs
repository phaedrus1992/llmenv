#![expect(clippy::expect_used, reason = "test scaffolding")]
//! #1198: `llmenv doctor`'s "cache directory is writable" check created a
//! missing cache dir via bare `create_dir_all`, leaving it at default
//! permissions instead of owner-only like every other llmenv-owned
//! state/cache directory (#1178/#1186/#1196).

mod support;

use std::fs;

use support::isolated_llmenv_cmd;

fn setup_test_config(cache_dir: &str) -> tempfile::TempDir {
    let tmp = tempfile::TempDir::new().expect("failed to create temp dir");
    let minimal_config = format!(
        r#"
scope:
  network: []
  host: []
  user: []
cache:
  cache_dir: {cache_dir}
  cache_retention_hours: 168
capabilities:
  hooks: []
bundle: []
mcp: []
plugin_marketplace: []
plugin_collection: []
"#
    );
    fs::write(tmp.path().join("config.yaml"), minimal_config).expect("failed to write config");
    tmp
}

#[cfg(unix)]
#[test]
fn doctor_creates_missing_cache_dir_owner_only() {
    use std::os::unix::fs::PermissionsExt;

    // HOME is isolated to `tmp.path()` by isolated_llmenv_cmd, so `~/cache`
    // expands to a path fully contained in the temp dir.
    let tmp = setup_test_config("~/cache");
    let cache_dir = tmp.path().join("cache");
    assert!(!cache_dir.exists(), "cache dir must not pre-exist");

    let output = isolated_llmenv_cmd(tmp.path())
        .arg("doctor")
        .output()
        .expect("failed to run llmenv doctor");
    assert!(
        output.status.success(),
        "doctor should succeed; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let mode = fs::metadata(&cache_dir)
        .expect("stat cache dir")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o700, "cache dir must be owner-only, got {mode:o}");
}
