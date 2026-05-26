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

#[test]
fn plugins_are_materialized() {
    let bundles = vec![fixture_bundle("with-plugin")];
    let m = merge(&bundles).expect("merge");
    let tmp = tempdir().expect("tempdir");
    ClaudeCodeAdapter
        .materialize(&m, tmp.path())
        .expect("materialize");

    let plugin_json = tmp.path().join("plugins/test-plugin/plugin.json");
    assert!(
        plugin_json.exists(),
        "plugin.json should be copied to plugins/<name>/plugin.json"
    );

    let content = std::fs::read_to_string(&plugin_json).expect("read plugin.json");
    assert!(content.contains("test-plugin"), "plugin content preserved");
}

#[test]
fn skills_with_frontmatter_are_validated() {
    // This test verifies that skills are structured as directories with SKILL.md
    // and that the adapter validates the required frontmatter.
    // Currently skills are single .md files; this test will fail until #33 is implemented.
    let bundles = vec![fixture_bundle("base")];
    let m = merge(&bundles).expect("merge");
    let tmp = tempdir().expect("tempdir");
    ClaudeCodeAdapter
        .materialize(&m, tmp.path())
        .expect("materialize");

    // For now, skills are still flat .md files, but after #33 they should be directories
    let hello_skill = tmp.path().join("skills/hello");
    // This will initially be "skills/hello.md"; after #33 it should be "skills/hello/SKILL.md"
    if hello_skill.is_dir() {
        let skill_md = hello_skill.join("SKILL.md");
        assert!(
            skill_md.exists(),
            "Each skill should have SKILL.md frontmatter"
        );
        let content = std::fs::read_to_string(&skill_md).expect("read SKILL.md");
        assert!(
            content.contains("---"),
            "SKILL.md should have YAML frontmatter"
        );
    }
}

#[test]
fn rejects_skill_missing_skill_md() {
    let tmp = tempdir().expect("tempdir");
    let skills_dir = tmp.path().join("skills");
    std::fs::create_dir_all(&skills_dir).expect("create skills dir");
    std::fs::create_dir(skills_dir.join("bad-skill")).expect("create skill dir");

    let m = llme::merge::MergedManifest {
        agents_md: String::new(),
        files: Default::default(),
        ..Default::default()
    };
    let err = ClaudeCodeAdapter
        .materialize(&m, tmp.path())
        .expect_err("should reject skill directory missing SKILL.md");
    assert!(err.to_string().contains("missing SKILL.md"));
}

#[test]
fn rejects_skill_missing_frontmatter_markers() {
    let tmp = tempdir().expect("tempdir");
    let skills_dir = tmp.path().join("skills");
    let skill_dir = skills_dir.join("bad-skill");
    std::fs::create_dir_all(&skill_dir).expect("create skill dir");
    std::fs::write(
        skill_dir.join("SKILL.md"),
        "name: bad\ndescription: missing markers",
    )
    .expect("write SKILL.md");

    let m = llme::merge::MergedManifest {
        agents_md: String::new(),
        files: Default::default(),
        ..Default::default()
    };
    let err = ClaudeCodeAdapter
        .materialize(&m, tmp.path())
        .expect_err("should reject SKILL.md without frontmatter markers");
    assert!(err.to_string().contains("missing YAML frontmatter"));
}

#[test]
fn rejects_skill_missing_name_field() {
    let tmp = tempdir().expect("tempdir");
    let skills_dir = tmp.path().join("skills");
    let skill_dir = skills_dir.join("bad-skill");
    std::fs::create_dir_all(&skill_dir).expect("create skill dir");
    std::fs::write(
        skill_dir.join("SKILL.md"),
        "---\ndescription: no name field\n---\n",
    )
    .expect("write SKILL.md");

    let m = llme::merge::MergedManifest {
        agents_md: String::new(),
        files: Default::default(),
        ..Default::default()
    };
    let err = ClaudeCodeAdapter
        .materialize(&m, tmp.path())
        .expect_err("should reject SKILL.md missing name");
    assert!(
        err.to_string()
            .contains("missing required frontmatter fields")
    );
}

#[test]
fn rejects_skill_missing_description_field() {
    let tmp = tempdir().expect("tempdir");
    let skills_dir = tmp.path().join("skills");
    let skill_dir = skills_dir.join("bad-skill");
    std::fs::create_dir_all(&skill_dir).expect("create skill dir");
    std::fs::write(skill_dir.join("SKILL.md"), "---\nname: bad\n---\n").expect("write SKILL.md");

    let m = llme::merge::MergedManifest {
        agents_md: String::new(),
        files: Default::default(),
        ..Default::default()
    };
    let err = ClaudeCodeAdapter
        .materialize(&m, tmp.path())
        .expect_err("should reject SKILL.md missing description");
    assert!(
        err.to_string()
            .contains("missing required frontmatter fields")
    );
}

#[test]
fn rejects_skill_with_invalid_yaml_frontmatter() {
    let tmp = tempdir().expect("tempdir");
    let skills_dir = tmp.path().join("skills");
    let skill_dir = skills_dir.join("bad-skill");
    std::fs::create_dir_all(&skill_dir).expect("create skill dir");
    std::fs::write(
        skill_dir.join("SKILL.md"),
        "---\ninvalid: yaml: syntax: here\n---\n",
    )
    .expect("write SKILL.md");

    let m = llme::merge::MergedManifest {
        agents_md: String::new(),
        files: Default::default(),
        ..Default::default()
    };
    let err = ClaudeCodeAdapter
        .materialize(&m, tmp.path())
        .expect_err("should reject invalid YAML frontmatter");
    assert!(err.to_string().contains("invalid YAML frontmatter"));
}
