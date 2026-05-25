use std::path::PathBuf;

use llme::adapter::AgentAdapter;
use llme::adapter::claude_code::ClaudeCodeAdapter;
use llme::merge::{BundleRef, merge};
use tempfile::tempdir;

fn fixture_bundle(name: &str) -> BundleRef {
    BundleRef {
        name: name.into(),
        path: PathBuf::from(format!("tests/fixtures/bundles/{name}")),
    }
}

#[test]
fn claude_code_layout() {
    let bundles = vec![fixture_bundle("base"), fixture_bundle("rust-defaults")];
    let m = merge(&bundles).expect("merge");
    let tmp = tempdir().expect("tempdir");
    let adapter = ClaudeCodeAdapter;
    adapter
        .materialize(&m, tmp.path())
        .expect("materialize claude-code layout");

    let mut files: Vec<String> = walkdir::WalkDir::new(tmp.path())
        .into_iter()
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_file())
        .map(|e| {
            e.path()
                .strip_prefix(tmp.path())
                .expect("strip prefix")
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    files.sort();
    insta::assert_yaml_snapshot!(files);
}

#[test]
fn claude_md_matches_merged_agents_md() {
    let bundles = vec![fixture_bundle("base"), fixture_bundle("rust-defaults")];
    let m = merge(&bundles).expect("merge");
    let tmp = tempdir().expect("tempdir");
    ClaudeCodeAdapter
        .materialize(&m, tmp.path())
        .expect("materialize");

    let claude_md = std::fs::read_to_string(tmp.path().join("CLAUDE.md")).expect("read CLAUDE.md");
    assert_eq!(claude_md, m.agents_md);
    assert!(!tmp.path().join("AGENTS.md").exists(), "no AGENTS.md");
}

#[test]
fn env_vars_set_claude_config_dir() {
    let tmp = tempdir().expect("tempdir");
    let vars = ClaudeCodeAdapter
        .env_vars(tmp.path())
        .expect("utf-8 tempdir");
    assert_eq!(vars.len(), 1);
    assert_eq!(vars[0].0, "CLAUDE_CONFIG_DIR");
    assert_eq!(vars[0].1, tmp.path().to_str().expect("tempdir utf-8"));
}

#[cfg(unix)]
#[test]
fn env_vars_rejects_non_utf8_cache_dir() {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;
    use std::path::Path;
    let bad = Path::new(OsStr::from_bytes(b"/tmp/\xff\xfe-not-utf8"));
    let err = ClaudeCodeAdapter
        .env_vars(bad)
        .expect_err("should reject non-utf8 cache dir");
    assert!(err.to_string().contains("not valid UTF-8"));
}

#[test]
fn name_is_stable() {
    assert_eq!(ClaudeCodeAdapter.name(), "claude-code");
}
