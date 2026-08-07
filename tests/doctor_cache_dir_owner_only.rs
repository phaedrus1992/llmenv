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

// #1198 (found during pre-pr-review): doctor's job is to report whether the
// cache dir is writable, not to mutate an existing one's permissions as a
// side effect. Forcing 0700 on a pre-existing dir could hard-fail on one
// owned by a different uid (shared cache location, container volume
// mapping) — the exact regression #1196 already walked back for
// codebase_memory.index_path. Hardening only applies when doctor itself
// creates the dir; an existing one is left exactly as its owner set it.
#[cfg(unix)]
#[test]
fn doctor_does_not_force_permissions_on_a_preexisting_cache_dir() {
    use std::os::unix::fs::PermissionsExt;

    let tmp = setup_test_config("~/cache");
    let cache_dir = tmp.path().join("cache");
    fs::create_dir_all(&cache_dir).expect("pre-create cache dir");
    fs::set_permissions(&cache_dir, fs::Permissions::from_mode(0o755)).expect("chmod 755");

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
    assert_eq!(
        mode, 0o755,
        "doctor must not change an existing cache dir's permissions, got {mode:o}"
    );
}
