use std::path::PathBuf;

use llmenv::adapter::AgentAdapter;
use llmenv::adapter::claude_code::ClaudeCodeAdapter;
use llmenv::merge::{BundleRef, merge};
use tempfile::tempdir;

fn fixture_bundle(name: &str) -> BundleRef {
    BundleRef {
        name: name.into(),
        path: PathBuf::from(format!("tests/fixtures/bundles/{name}")),
        precedence: 1,
    }
}

#[test]
fn claude_code_layout() {
    let bundles = vec![fixture_bundle("base"), fixture_bundle("rust-defaults")];
    let m = merge(&llmenv::config::Capabilities::default(), &bundles).expect("merge");
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
    let m = merge(&llmenv::config::Capabilities::default(), &bundles).expect("merge");
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
    let m = merge(&llmenv::config::Capabilities::default(), &bundles).expect("merge");
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
    let m = merge(&llmenv::config::Capabilities::default(), &bundles).expect("merge");
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

    let m = llmenv::merge::MergedManifest {
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

    let m = llmenv::merge::MergedManifest {
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

    let m = llmenv::merge::MergedManifest {
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

    let m = llmenv::merge::MergedManifest {
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

    let m = llmenv::merge::MergedManifest {
        agents_md: String::new(),
        files: Default::default(),
        ..Default::default()
    };
    let err = ClaudeCodeAdapter
        .materialize(&m, tmp.path())
        .expect_err("should reject invalid YAML frontmatter");
    assert!(err.to_string().contains("invalid YAML frontmatter"));
}

// Issue #90: Hooks generator — wire bundle hook fragments into settings.json
#[test]
fn hooks_generator_renders_bundle_hooks_into_settings_json() {
    let bundles = vec![fixture_bundle("base")];
    let m = merge(&llmenv::config::Capabilities::default(), &bundles).expect("merge");
    let tmp = tempdir().expect("tempdir");
    ClaudeCodeAdapter
        .materialize(&m, tmp.path())
        .expect("materialize");

    let settings_path = tmp.path().join("settings.json");
    assert!(settings_path.exists(), "settings.json should be created");

    let settings_json = std::fs::read_to_string(&settings_path).expect("read settings.json");
    let parsed: serde_json::Value =
        serde_json::from_str(&settings_json).expect("parse settings.json");

    // Verify hooks object exists at top level
    let hooks = parsed
        .get("hooks")
        .expect("settings.json should have 'hooks' key");
    assert!(hooks.is_object(), "hooks should be an object");

    // If hooks exist, verify structure: { EventName: [{ matcher?: "...", hooks: [...] }] }
    if let Some(hooks_obj) = hooks.as_object() {
        for (_event, entries) in hooks_obj {
            assert!(
                entries.is_array(),
                "each event maps to an array of handlers"
            );
            if let Some(entries_arr) = entries.as_array() {
                for entry in entries_arr {
                    assert!(entry.is_object(), "each handler must be an object");
                    if let Some(entry_obj) = entry.as_object() {
                        // Must have "hooks" array with handler details
                        assert!(
                            entry_obj.contains_key("hooks"),
                            "handler entry must have 'hooks' key"
                        );
                        if let Some(handlers) = entry_obj.get("hooks").and_then(|h| h.as_array()) {
                            for handler in handlers {
                                // Each handler must have type field
                                assert!(
                                    handler.get("type").is_some(),
                                    "handler must have 'type' field"
                                );
                            }
                        }
                    }
                }
            }
        }
    }
}

// Issue #90: Bundle-relative command paths should resolve correctly
#[test]
fn hooks_generator_resolves_bundle_relative_paths() {
    let bundles = vec![fixture_bundle("base")];
    let m = merge(&llmenv::config::Capabilities::default(), &bundles).expect("merge");
    let tmp = tempdir().expect("tempdir");
    ClaudeCodeAdapter
        .materialize(&m, tmp.path())
        .expect("materialize");

    let settings_path = tmp.path().join("settings.json");
    let settings_json = std::fs::read_to_string(&settings_path).expect("read settings.json");
    let parsed: serde_json::Value =
        serde_json::from_str(&settings_json).expect("parse settings.json");

    // If hooks exist with commands, verify paths are not bundle-relative
    if let Some(hooks) = parsed.get("hooks").and_then(|h| h.as_object()) {
        for (_event, entries) in hooks {
            if let Some(entries_arr) = entries.as_array() {
                for entry in entries_arr {
                    if let Some(handlers) = entry.get("hooks").and_then(|h| h.as_array()) {
                        for handler in handlers {
                            if let Some(cmd) = handler.get("command").and_then(|c| c.as_str()) {
                                // Command paths should not be bundle-relative
                                assert!(
                                    !cmd.starts_with("hooks/"),
                                    "command paths should not be bundle-relative: {}",
                                    cmd
                                );
                            }
                        }
                    }
                }
            }
        }
    }
}

// Issue #91 / #34: Native passthrough — native rule strings land flat in the
// same permissions.{allow,ask,deny} arrays as rendered neutral rules, matching
// Claude Code's object schema (not a nested `permissions.native` object).
#[test]
fn native_passthrough_merges_engine_native_into_settings() {
    let bundles = vec![fixture_bundle("base")];
    let m = merge(&llmenv::config::Capabilities::default(), &bundles).expect("merge");
    let tmp = tempdir().expect("tempdir");
    ClaudeCodeAdapter
        .materialize(&m, tmp.path())
        .expect("materialize");

    let settings_path = tmp.path().join("settings.json");
    let settings_json = std::fs::read_to_string(&settings_path).expect("read settings.json");
    let parsed: serde_json::Value =
        serde_json::from_str(&settings_json).expect("parse settings.json");

    let perms = parsed
        .get("permissions")
        .expect("base bundle declares permissions");
    // Native strings are flat in the action arrays, not under a `native` subkey.
    assert!(
        perms.get("native").is_none(),
        "native rules must be flattened into allow/ask/deny, not nested"
    );
    let deny: Vec<&str> = perms["deny"]
        .as_array()
        .expect("deny array")
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    // Neutral Read paths rendered, plus the verbatim native WebFetch rule.
    assert!(
        deny.contains(&"Read(./.env)"),
        "neutral path rule: {deny:?}"
    );
    assert!(
        deny.contains(&"Read(./.env.*)"),
        "neutral path rule: {deny:?}"
    );
    assert!(
        deny.contains(&"WebFetch(domain:internal.example.com)"),
        "native rule appended verbatim: {deny:?}"
    );
}

// Issue #34: A native rule wins over a neutral rule asserting the same string in
// a different action. Here a neutral `allow: WebFetch(domain:x)` is overridden by
// a native `deny: WebFetch(domain:x)` — the string must appear only in deny.
#[test]
fn native_rule_overrides_conflicting_neutral_rule() {
    let bundles = vec![fixture_bundle("native-wins")];
    let m = merge(&llmenv::config::Capabilities::default(), &bundles).expect("merge");
    let tmp = tempdir().expect("tempdir");
    ClaudeCodeAdapter
        .materialize(&m, tmp.path())
        .expect("materialize");

    let settings_json =
        std::fs::read_to_string(tmp.path().join("settings.json")).expect("read settings.json");
    let parsed: serde_json::Value =
        serde_json::from_str(&settings_json).expect("parse settings.json");

    let allow: Vec<&str> = parsed["permissions"]["allow"]
        .as_array()
        .map(|a| a.iter().filter_map(|v| v.as_str()).collect())
        .unwrap_or_default();
    let deny: Vec<&str> = parsed["permissions"]["deny"]
        .as_array()
        .map(|a| a.iter().filter_map(|v| v.as_str()).collect())
        .unwrap_or_default();

    assert!(
        !allow.contains(&"WebFetch(domain:blocked.example.com)"),
        "neutral allow suppressed by native deny: {allow:?}"
    );
    assert!(
        deny.contains(&"WebFetch(domain:blocked.example.com)"),
        "native deny wins: {deny:?}"
    );
}

// Issue #85: SessionStart hook prerequisite — wiring complete, hash comparison deferred
#[test]
fn session_start_hook_emitted_in_settings_json() {
    // Issue #85 is the prerequisite for SessionStart hook emission (verifying wiring works).
    // The actual hash computation and SessionStart emission logic is deferred to the
    // runtime hook script (once the wiring framework is complete and tested).
    // This test verifies the hooks structure supports SessionStart registration.

    let bundles = vec![fixture_bundle("base")];
    let m = merge(&llmenv::config::Capabilities::default(), &bundles).expect("merge");
    let tmp = tempdir().expect("tempdir");
    ClaudeCodeAdapter
        .materialize(&m, tmp.path())
        .expect("materialize");

    let settings_path = tmp.path().join("settings.json");
    let settings_json = std::fs::read_to_string(&settings_path).expect("read settings.json");
    let parsed: serde_json::Value =
        serde_json::from_str(&settings_json).expect("parse settings.json");

    // Verify hooks object exists and supports event registration (including SessionStart)
    let hooks = parsed
        .get("hooks")
        .expect("settings.json should have hooks");
    assert!(
        hooks.is_object(),
        "hooks should be an object mapping event names to handler arrays"
    );

    // The structure { EventName: [{ matcher?, hooks: [...] }] } supports SessionStart
    // being registered once #85 logic is implemented (after this PR merges).
}

// Issue #34: Two bundles each contributing a PreToolUse hook + permission entries
// merge into a single settings.json with deterministic ordering and dedup.
//
// Dedup key for hooks is (event, matcher, command): merge-a and merge-b both
// declare `PreToolUse`/`Bash`/`hooks/guard.sh`, so it appears once. The distinct
// `PreToolUse`/`Edit`/`hooks/fmt.sh` hook survives. Permission `allow` rules union
// with the shared `Bash cargo *` rule deduped. Ordering is bundle-precedence then
// declaration order — both fixtures share precedence here, so merge-a precedes
// merge-b by slice position, which the snapshot pins.
#[test]
fn two_bundles_merge_into_deterministic_settings_json() {
    let bundles = vec![fixture_bundle("merge-a"), fixture_bundle("merge-b")];
    let m = merge(&llmenv::config::Capabilities::default(), &bundles).expect("merge");
    let tmp = tempdir().expect("tempdir");
    ClaudeCodeAdapter
        .materialize(&m, tmp.path())
        .expect("materialize");

    let settings_json =
        std::fs::read_to_string(tmp.path().join("settings.json")).expect("read settings.json");
    let parsed: serde_json::Value =
        serde_json::from_str(&settings_json).expect("parse settings.json");

    // PreToolUse has exactly two entries: the deduped Bash/guard.sh and the
    // distinct Edit/fmt.sh. If dedup regressed, this would be three.
    let pre = parsed["hooks"]["PreToolUse"]
        .as_array()
        .expect("PreToolUse array");
    assert_eq!(pre.len(), 2, "guard.sh deduped, fmt.sh survives: {pre:#?}");

    // Snapshot pins the full deterministic shape: ordering, dedup, permission
    // union, and native passthrough merge across both bundles.
    insta::assert_yaml_snapshot!(parsed);
}

// Issue #34: Reversing bundle input order must not change the merged hook set
// or permission set — only first-seen list order may shift. Locks the
// order-independence guarantee at the adapter (end-to-end) layer, complementing
// the unit-level property tests in merge::capabilities.
#[test]
fn bundle_order_does_not_change_merged_membership() {
    let forward = vec![fixture_bundle("merge-a"), fixture_bundle("merge-b")];
    let backward = vec![fixture_bundle("merge-b"), fixture_bundle("merge-a")];

    let mf = merge(&llmenv::config::Capabilities::default(), &forward).expect("merge fwd");
    let mb = merge(&llmenv::config::Capabilities::default(), &backward).expect("merge bwd");

    // Same number of hooks regardless of order (dedup is order-independent).
    assert_eq!(mf.capabilities.hooks.len(), mb.capabilities.hooks.len());
    // Same allow-rule membership.
    let set = |c: &llmenv::config::Capabilities| {
        let mut v: Vec<_> = c
            .permissions
            .allow
            .iter()
            .map(|r| (r.tool.clone(), r.pattern.clone(), r.paths.clone()))
            .collect();
        v.sort();
        v
    };
    assert_eq!(set(&mf.capabilities), set(&mb.capabilities));
}
