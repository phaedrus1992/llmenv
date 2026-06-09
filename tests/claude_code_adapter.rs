#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::BTreeMap;
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

fn empty_native() -> BTreeMap<String, serde_yaml::Value> {
    BTreeMap::new()
}

#[test]
fn claude_code_layout() {
    let bundles = vec![fixture_bundle("base"), fixture_bundle("rust-defaults")];
    let m = merge(
        &llmenv::config::Capabilities::default(),
        &empty_native(),
        &bundles,
    )
    .expect("merge");
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
    let m = merge(
        &llmenv::config::Capabilities::default(),
        &empty_native(),
        &bundles,
    )
    .expect("merge");
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
    let m = merge(
        &llmenv::config::Capabilities::default(),
        &empty_native(),
        &bundles,
    )
    .expect("merge");
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
    let m = merge(
        &llmenv::config::Capabilities::default(),
        &empty_native(),
        &bundles,
    )
    .expect("merge");
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
    let m = merge(
        &llmenv::config::Capabilities::default(),
        &empty_native(),
        &bundles,
    )
    .expect("merge");
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
    let m = merge(
        &llmenv::config::Capabilities::default(),
        &empty_native(),
        &bundles,
    )
    .expect("merge");
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
    let m = merge(
        &llmenv::config::Capabilities::default(),
        &empty_native(),
        &bundles,
    )
    .expect("merge");
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
    let m = merge(
        &llmenv::config::Capabilities::default(),
        &empty_native(),
        &bundles,
    )
    .expect("merge");
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

// Issue #34: deny is authoritative. A native `allow` must NEVER suppress a
// neutral `deny` of the same string — silently weakening a deny rule is a
// security regression. The string must remain in deny; it may also appear in
// allow (the native rule still emits), but the deny is never dropped.
#[test]
fn native_allow_does_not_suppress_neutral_deny() {
    let bundles = vec![fixture_bundle("native-allow-vs-neutral-deny")];
    let m = merge(
        &llmenv::config::Capabilities::default(),
        &empty_native(),
        &bundles,
    )
    .expect("merge");
    let tmp = tempdir().expect("tempdir");
    ClaudeCodeAdapter
        .materialize(&m, tmp.path())
        .expect("materialize");

    let settings_json =
        std::fs::read_to_string(tmp.path().join("settings.json")).expect("read settings.json");
    let parsed: serde_json::Value =
        serde_json::from_str(&settings_json).expect("parse settings.json");

    let deny: Vec<&str> = parsed["permissions"]["deny"]
        .as_array()
        .map(|a| a.iter().filter_map(|v| v.as_str()).collect())
        .unwrap_or_default();

    assert!(
        deny.contains(&"Read(./secrets/**)"),
        "neutral deny must not be suppressed by a native allow: {deny:?}"
    );
}

// Issue #97: native_hooks["claude_code"] is a settings.json `hooks`-shaped
// fragment that deep-merges into the generated hooks object. Engine-only hook
// events appear verbatim; events shared with generic hooks concat their entries.
#[test]
fn native_hooks_merge_into_settings_hooks() {
    let mut native_hooks = BTreeMap::new();
    native_hooks.insert(
        "claude_code".to_string(),
        serde_yaml::from_str::<serde_yaml::Value>(
            "PreCompact:\n  - hooks:\n      - type: command\n        command: /bin/engine-only.sh\n",
        )
        .expect("parse native hooks"),
    );
    let m = llmenv::merge::MergedManifest {
        capabilities: llmenv::config::Capabilities {
            native_hooks,
            ..Default::default()
        },
        ..Default::default()
    };
    let tmp = tempdir().expect("tempdir");
    ClaudeCodeAdapter
        .materialize(&m, tmp.path())
        .expect("materialize");

    let settings_json =
        std::fs::read_to_string(tmp.path().join("settings.json")).expect("read settings.json");
    let parsed: serde_json::Value =
        serde_json::from_str(&settings_json).expect("parse settings.json");

    let pre_compact = parsed["hooks"]["PreCompact"]
        .as_array()
        .expect("native PreCompact event rendered into hooks");
    let cmd = pre_compact[0]["hooks"][0]["command"]
        .as_str()
        .expect("native hook command");
    assert_eq!(cmd, "/bin/engine-only.sh");
}

// Issue #97: native_plugins["claude_code"] is a settings.json fragment that
// deep-merges at the top level (e.g. extra plugin settings Claude understands
// but llmenv has no neutral representation for).
#[test]
fn native_plugins_merge_into_settings() {
    let mut native_plugins = BTreeMap::new();
    native_plugins.insert(
        "claude_code".to_string(),
        serde_yaml::from_str::<serde_yaml::Value>("enabledPlugins:\n  \"extra@market\": true\n")
            .expect("parse native plugins"),
    );
    let m = llmenv::merge::MergedManifest {
        capabilities: llmenv::config::Capabilities {
            native_plugins,
            ..Default::default()
        },
        ..Default::default()
    };
    let tmp = tempdir().expect("tempdir");
    ClaudeCodeAdapter
        .materialize(&m, tmp.path())
        .expect("materialize");

    let settings_json =
        std::fs::read_to_string(tmp.path().join("settings.json")).expect("read settings.json");
    let parsed: serde_json::Value =
        serde_json::from_str(&settings_json).expect("parse settings.json");

    assert_eq!(
        parsed["enabledPlugins"]["extra@market"],
        serde_json::Value::Bool(true),
        "native plugin setting merged into settings.json"
    );
}

// Issue #97 + #244: a native_mcp["claude_code"] fragment carrying its own
// `mcpServers` injects engine-specific server entries, which merge into the
// top-level `mcpServers` of `.claude.json` even with no resolved MCPs.
#[test]
fn native_mcp_servers_merge_into_claude_json() {
    let mut native_mcp = BTreeMap::new();
    native_mcp.insert(
        "claude_code".to_string(),
        serde_yaml::from_str::<serde_yaml::Value>(
            "mcpServers:\n  stdio_server:\n    command: native-bin\n",
        )
        .expect("parse native mcp"),
    );
    let m = llmenv::merge::MergedManifest {
        capabilities: llmenv::config::Capabilities {
            native_mcp,
            ..Default::default()
        },
        ..Default::default()
    };
    let tmp = tempdir().expect("tempdir");
    ClaudeCodeAdapter
        .materialize(&m, tmp.path())
        .expect("materialize");

    let claude_json = std::fs::read_to_string(tmp.path().join(".claude.json"))
        .expect(".claude.json emitted for native servers");
    let parsed: serde_json::Value = serde_json::from_str(&claude_json).expect("parse .claude.json");

    assert_eq!(
        parsed["mcpServers"]["stdio_server"]["command"],
        "native-bin"
    );
}

// Issue #96: the top-level `native.claude_code` catch-all (keys no modeled
// feature produces) deep-merges into settings.json after modeled capabilities.
#[test]
fn top_level_native_passthrough_merges_into_settings() {
    let mut native = BTreeMap::new();
    native.insert(
        "claude_code".to_string(),
        serde_yaml::from_str::<serde_yaml::Value>(
            "alwaysThinkingEnabled: false\noutputStyle: Explanatory\n",
        )
        .expect("parse native"),
    );
    let m = llmenv::merge::MergedManifest {
        native,
        ..Default::default()
    };
    let tmp = tempdir().expect("tempdir");
    ClaudeCodeAdapter
        .materialize(&m, tmp.path())
        .expect("materialize");

    let settings_json =
        std::fs::read_to_string(tmp.path().join("settings.json")).expect("read settings.json");
    let parsed: serde_json::Value =
        serde_json::from_str(&settings_json).expect("parse settings.json");

    assert_eq!(
        parsed["alwaysThinkingEnabled"],
        serde_json::Value::Bool(false)
    );
    assert_eq!(
        parsed["outputStyle"],
        serde_json::Value::String("Explanatory".into())
    );
}

// Issue #96/#102 (security): the top-level `native.claude_code` catch-all is for
// keys that belong to NO modeled feature. A modeled-feature key (`permissions`,
// `hooks`) appearing there would be overlaid LAST over the rendered settings,
// silently clobbering the security-rendered output — e.g. erasing the
// permission `deny` array, bypassing the deny-never-weakened invariant. Per
// design D3 ("Layer 1 wins, or hard-error"), this must hard-error, not silently
// honor the catch-all. The key belongs in the `native_<feature>` sibling.
#[test]
fn top_level_native_with_modeled_key_hard_errors() {
    let mut native = BTreeMap::new();
    native.insert(
        "claude_code".to_string(),
        serde_yaml::from_str::<serde_yaml::Value>("permissions:\n  deny: null\n")
            .expect("parse native"),
    );
    let m = llmenv::merge::MergedManifest {
        native,
        ..Default::default()
    };
    let tmp = tempdir().expect("tempdir");
    let err = ClaudeCodeAdapter
        .materialize(&m, tmp.path())
        .expect_err("modeled key in top-level native must hard-error");
    let msg = err.to_string();
    assert!(
        msg.contains("permissions") && msg.contains("native"),
        "error must name the offending modeled key and point at native_<feature>: {msg}"
    );
}

// Issue #96/#102: the guard above only fires for modeled-feature keys. A
// genuine catch-all key (no modeled feature owns it) must still pass through.
#[test]
fn top_level_native_with_hooks_key_hard_errors() {
    let mut native = BTreeMap::new();
    native.insert(
        "claude_code".to_string(),
        serde_yaml::from_str::<serde_yaml::Value>("hooks:\n  PreToolUse: []\n")
            .expect("parse native"),
    );
    let m = llmenv::merge::MergedManifest {
        native,
        ..Default::default()
    };
    let tmp = tempdir().expect("tempdir");
    let err = ClaudeCodeAdapter
        .materialize(&m, tmp.path())
        .expect_err("hooks key in top-level native must hard-error");
    assert!(
        err.to_string().contains("hooks"),
        "error must name the offending modeled key: {err}"
    );
}

// Issue #121 (#85): the adapter always emits a SessionStart hook that runs the
// stale-context check command. The command shells back into `llmenv` so the
// runtime hook can compare the booted content hash (the CLAUDE_CONFIG_DIR folder
// name) against what llmenv would materialize now, and warn the user to restart.
#[test]
fn session_start_stale_check_hook_emitted_in_settings_json() {
    let bundles = vec![fixture_bundle("base")];
    let m = merge(
        &llmenv::config::Capabilities::default(),
        &empty_native(),
        &bundles,
    )
    .expect("merge");
    let tmp = tempdir().expect("tempdir");
    ClaudeCodeAdapter
        .materialize(&m, tmp.path())
        .expect("materialize");

    let settings_path = tmp.path().join("settings.json");
    let settings_json = std::fs::read_to_string(&settings_path).expect("read settings.json");
    let parsed: serde_json::Value =
        serde_json::from_str(&settings_json).expect("parse settings.json");

    let session_start = parsed["hooks"]["SessionStart"]
        .as_array()
        .expect("SessionStart hook registered in settings.json");
    assert!(
        !session_start.is_empty(),
        "SessionStart must carry at least one handler"
    );
    let handler = &session_start[0]["hooks"][0];
    assert_eq!(
        handler["type"],
        serde_json::Value::String("command".into()),
        "stale-check is a command-type handler"
    );
    let cmd = handler["command"]
        .as_str()
        .expect("SessionStart handler has a command");
    assert!(
        cmd.contains("llmenv") && cmd.contains("check-stale"),
        "SessionStart command must invoke `llmenv check-stale`: {cmd}"
    );
}

// Issue #121: a user-supplied native_hooks SessionStart entry must not be
// clobbered by the auto-emitted stale-check — both survive (events concat).
#[test]
fn session_start_native_hook_coexists_with_stale_check() {
    let mut native_hooks = BTreeMap::new();
    native_hooks.insert(
        "claude_code".to_string(),
        serde_yaml::from_str::<serde_yaml::Value>(
            "SessionStart:\n  - hooks:\n      - type: command\n        command: /bin/user-start.sh\n",
        )
        .expect("parse native hooks"),
    );
    let m = llmenv::merge::MergedManifest {
        capabilities: llmenv::config::Capabilities {
            native_hooks,
            ..Default::default()
        },
        ..Default::default()
    };
    let tmp = tempdir().expect("tempdir");
    ClaudeCodeAdapter
        .materialize(&m, tmp.path())
        .expect("materialize");

    let settings_json =
        std::fs::read_to_string(tmp.path().join("settings.json")).expect("read settings.json");
    let parsed: serde_json::Value =
        serde_json::from_str(&settings_json).expect("parse settings.json");

    let session_start = parsed["hooks"]["SessionStart"]
        .as_array()
        .expect("SessionStart array");
    let commands: Vec<&str> = session_start
        .iter()
        .filter_map(|e| e["hooks"][0]["command"].as_str())
        .collect();
    assert!(
        commands.iter().any(|c| c.contains("check-stale")),
        "auto stale-check survives: {commands:?}"
    );
    assert!(
        commands.contains(&"/bin/user-start.sh"),
        "user SessionStart hook survives: {commands:?}"
    );
}

// Issue #244: resolved servers land in the top-level `mcpServers` of
// `.claude.json` (the surface Claude reads), with the remote transport `type`
// emitted. User-scoped servers there are auto-trusted, so no approval gate
// (`enabledMcpjsonServers`) is written.
#[test]
fn resolved_servers_land_in_claude_json_mcp_servers() {
    use llmenv::mcp::resolve::{ResolvedKind, ResolvedMcp};

    let m = llmenv::merge::MergedManifest {
        mcps: vec![
            ResolvedMcp {
                name: "playwright".into(),
                kind: ResolvedKind::Stdio {
                    command: "npx".into(),
                    args: vec!["playwright".into()],
                    env: BTreeMap::new(),
                },
            },
            ResolvedMcp {
                name: "icm".into(),
                kind: ResolvedKind::Remote {
                    url: "http://still.local:9100/mcp".into(),
                    transport: llmenv::config::McpTransport::Http,
                },
            },
        ],
        ..Default::default()
    };
    let tmp = tempdir().expect("tempdir");
    ClaudeCodeAdapter
        .materialize(&m, tmp.path())
        .expect("materialize");

    // mcp.json is dead and must never be written (#244 sub-point 3).
    assert!(
        !tmp.path().join("mcp.json").exists(),
        "legacy mcp.json must not be written"
    );

    let claude_json =
        std::fs::read_to_string(tmp.path().join(".claude.json")).expect(".claude.json emitted");
    let parsed: serde_json::Value = serde_json::from_str(&claude_json).expect("parse .claude.json");

    let servers = parsed["mcpServers"]
        .as_object()
        .expect("mcpServers object present");
    assert!(servers.contains_key("playwright"), "stdio server present");
    assert_eq!(servers["playwright"]["command"], "npx");
    assert_eq!(servers["icm"]["type"], "http", "remote carries type");
    assert_eq!(servers["icm"]["url"], "http://still.local:9100/mcp");
    assert!(
        parsed.get("enabledMcpjsonServers").is_none(),
        "no approval gate in .claude.json"
    );
}

// Issue #329: mcp: entries in bundle.yaml are resolved and rendered into
// `.claude.json` mcpServers. Tagless entries are always active; tagged entries
// require an active scope tag match.
#[test]
fn bundle_mcp_entries_render_into_claude_json() {
    use llmenv::mcp::resolve::resolve_bundle_mcps;
    use std::collections::BTreeSet;

    let bundles = vec![fixture_bundle("with-mcp")];
    let mut manifest = merge(
        &llmenv::config::Capabilities::default(),
        &empty_native(),
        &bundles,
    )
    .expect("merge");

    let active_tags: BTreeSet<String> = BTreeSet::new();
    manifest.mcps.extend(
        resolve_bundle_mcps(&manifest.capabilities.mcp, &active_tags).expect("resolve bundle mcps"),
    );

    let tmp = tempdir().expect("tempdir");
    ClaudeCodeAdapter
        .materialize(&manifest, tmp.path())
        .expect("materialize");

    let claude_json =
        std::fs::read_to_string(tmp.path().join(".claude.json")).expect(".claude.json emitted");
    let parsed: serde_json::Value = serde_json::from_str(&claude_json).expect("parse");

    let servers = parsed["mcpServers"]
        .as_object()
        .expect("mcpServers present");
    assert!(
        servers.contains_key("ctx"),
        "tagless bundle mcp must be active: {servers:?}"
    );
    assert!(
        !servers.contains_key("playwright"),
        "tagged bundle mcp with no matching active tag must be inactive: {servers:?}"
    );
}

// Issue #329 (tagged variant): bundle mcp with a matching tag is active.
#[test]
fn bundle_mcp_tagged_entry_active_when_tag_matches() {
    use llmenv::mcp::resolve::resolve_bundle_mcps;
    use std::collections::BTreeSet;

    let bundles = vec![fixture_bundle("with-mcp")];
    let mut manifest = merge(
        &llmenv::config::Capabilities::default(),
        &empty_native(),
        &bundles,
    )
    .expect("merge");

    let active_tags: BTreeSet<String> = BTreeSet::from(["feature-playwright".to_string()]);
    manifest.mcps.extend(
        resolve_bundle_mcps(&manifest.capabilities.mcp, &active_tags).expect("resolve bundle mcps"),
    );

    let tmp = tempdir().expect("tempdir");
    ClaudeCodeAdapter
        .materialize(&manifest, tmp.path())
        .expect("materialize");

    let claude_json =
        std::fs::read_to_string(tmp.path().join(".claude.json")).expect(".claude.json emitted");
    let parsed: serde_json::Value = serde_json::from_str(&claude_json).expect("parse");

    let servers = parsed["mcpServers"]
        .as_object()
        .expect("mcpServers present");
    assert!(
        servers.contains_key("ctx"),
        "tagless entry must still be active"
    );
    assert!(
        servers.contains_key("playwright"),
        "tagged entry must be active when tag matches"
    );
}

// Issue #329 (combined path): global MCPs and bundle MCPs are both rendered;
// bundle comes after global in declaration order (last-write-wins in json map).
#[test]
fn global_and_bundle_mcps_both_render() {
    use llmenv::mcp::resolve::{ResolvedKind, ResolvedMcp, resolve_bundle_mcps};
    use std::collections::BTreeSet;

    let bundles = vec![fixture_bundle("with-mcp")];
    let mut manifest = merge(
        &llmenv::config::Capabilities::default(),
        &empty_native(),
        &bundles,
    )
    .expect("merge");

    // Simulate a pre-resolved global MCP (as build_manifest does via resolve_mcps).
    manifest.mcps.push(ResolvedMcp {
        name: "global-tool".into(),
        kind: ResolvedKind::Stdio {
            command: "global-cmd".into(),
            args: vec![],
            env: BTreeMap::new(),
        },
    });
    manifest.mcps.extend(
        resolve_bundle_mcps(&manifest.capabilities.mcp, &BTreeSet::new())
            .expect("resolve bundle mcps"),
    );

    let tmp = tempdir().expect("tempdir");
    ClaudeCodeAdapter
        .materialize(&manifest, tmp.path())
        .expect("materialize");

    let claude_json =
        std::fs::read_to_string(tmp.path().join(".claude.json")).expect(".claude.json emitted");
    let parsed: serde_json::Value = serde_json::from_str(&claude_json).expect("parse");

    let servers = parsed["mcpServers"]
        .as_object()
        .expect("mcpServers present");
    assert!(
        servers.contains_key("global-tool"),
        "global mcp must render"
    );
    assert!(
        servers.contains_key("ctx"),
        "bundle mcp must render alongside global"
    );
    assert_eq!(servers["global-tool"]["command"], "global-cmd");
}

// Issue #244: a stray `enabledMcpjsonServers` in a native_mcp fragment is a
// project `.mcp.json` approval gate — irrelevant for the auto-trusted
// user-scoped servers in `.claude.json` — so it is dropped, while the resolved
// server still lands under `mcpServers`.
#[test]
fn native_mcp_enabled_list_is_dropped() {
    use llmenv::mcp::resolve::{ResolvedKind, ResolvedMcp};

    let mut native_mcp = BTreeMap::new();
    native_mcp.insert(
        "claude_code".to_string(),
        serde_yaml::from_str::<serde_yaml::Value>("enabledMcpjsonServers:\n  - only-this\n")
            .expect("parse native mcp"),
    );
    let m = llmenv::merge::MergedManifest {
        mcps: vec![ResolvedMcp {
            name: "playwright".into(),
            kind: ResolvedKind::Stdio {
                command: "npx".into(),
                args: vec![],
                env: BTreeMap::new(),
            },
        }],
        capabilities: llmenv::config::Capabilities {
            native_mcp,
            ..Default::default()
        },
        ..Default::default()
    };
    let tmp = tempdir().expect("tempdir");
    ClaudeCodeAdapter
        .materialize(&m, tmp.path())
        .expect("materialize");

    let claude_json =
        std::fs::read_to_string(tmp.path().join(".claude.json")).expect(".claude.json emitted");
    let parsed: serde_json::Value = serde_json::from_str(&claude_json).expect("parse .claude.json");
    // Resolved server present under mcpServers...
    assert_eq!(parsed["mcpServers"]["playwright"]["command"], "npx");
    // ...and the approval-gate key is never written into .claude.json.
    assert!(
        parsed.get("enabledMcpjsonServers").is_none(),
        "enabledMcpjsonServers dropped: {parsed}"
    );
}

// Issue #123 (design O4): when llmenv's ICM memory backend is active (the `icm`
// MCP server is resolved), the adapter disables Claude's native auto memory so
// the two memory systems don't compete. `autoMemoryEnabled: false` lands in
// settings.json by default.
#[test]
fn auto_memory_disabled_when_icm_active() {
    use llmenv::mcp::resolve::{ResolvedKind, ResolvedMcp};

    let m = llmenv::merge::MergedManifest {
        mcps: vec![ResolvedMcp {
            name: "icm".into(),
            kind: ResolvedKind::Remote {
                url: "http://still.local:9100/mcp".into(),
                transport: llmenv::config::McpTransport::Http,
            },
        }],
        ..Default::default()
    };
    let tmp = tempdir().expect("tempdir");
    ClaudeCodeAdapter
        .materialize(&m, tmp.path())
        .expect("materialize");

    let settings_json =
        std::fs::read_to_string(tmp.path().join("settings.json")).expect("read settings.json");
    let parsed: serde_json::Value =
        serde_json::from_str(&settings_json).expect("parse settings.json");
    assert_eq!(
        parsed["autoMemoryEnabled"],
        serde_json::Value::Bool(false),
        "ICM active ⇒ native auto memory disabled"
    );
}

// Issue #123: when ICM is NOT active, llmenv leaves Claude's native auto memory
// alone — no `autoMemoryEnabled` key is emitted, so the user's own setting (or
// Claude's default) stands.
#[test]
fn auto_memory_untouched_when_icm_inactive() {
    let bundles = vec![fixture_bundle("base")];
    let m = merge(
        &llmenv::config::Capabilities::default(),
        &empty_native(),
        &bundles,
    )
    .expect("merge");
    let tmp = tempdir().expect("tempdir");
    ClaudeCodeAdapter
        .materialize(&m, tmp.path())
        .expect("materialize");

    let settings_json =
        std::fs::read_to_string(tmp.path().join("settings.json")).expect("read settings.json");
    let parsed: serde_json::Value =
        serde_json::from_str(&settings_json).expect("parse settings.json");
    assert!(
        parsed.get("autoMemoryEnabled").is_none(),
        "no ICM ⇒ llmenv must not touch autoMemoryEnabled: {parsed}"
    );
}

// Issue #123: a user who explicitly sets `autoMemoryEnabled` via the top-level
// native catch-all wins over llmenv's default disable — the native overlay is
// applied last, so the user's intent is respected even with ICM active.
#[test]
fn user_native_auto_memory_overrides_icm_default() {
    use llmenv::mcp::resolve::{ResolvedKind, ResolvedMcp};

    let mut native = BTreeMap::new();
    native.insert(
        "claude_code".to_string(),
        serde_yaml::from_str::<serde_yaml::Value>("autoMemoryEnabled: true\n")
            .expect("parse native"),
    );
    let m = llmenv::merge::MergedManifest {
        mcps: vec![ResolvedMcp {
            name: "icm".into(),
            kind: ResolvedKind::Remote {
                url: "http://still.local:9100/mcp".into(),
                transport: llmenv::config::McpTransport::Http,
            },
        }],
        native,
        ..Default::default()
    };
    let tmp = tempdir().expect("tempdir");
    ClaudeCodeAdapter
        .materialize(&m, tmp.path())
        .expect("materialize");

    let settings_json =
        std::fs::read_to_string(tmp.path().join("settings.json")).expect("read settings.json");
    let parsed: serde_json::Value =
        serde_json::from_str(&settings_json).expect("parse settings.json");
    assert_eq!(
        parsed["autoMemoryEnabled"],
        serde_json::Value::Bool(true),
        "explicit user native setting wins over llmenv's ICM default"
    );
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
    let m = merge(
        &llmenv::config::Capabilities::default(),
        &empty_native(),
        &bundles,
    )
    .expect("merge");
    let tmp = tempdir().expect("tempdir");
    ClaudeCodeAdapter
        .materialize(&m, tmp.path())
        .expect("materialize");

    let settings_json =
        std::fs::read_to_string(tmp.path().join("settings.json")).expect("read settings.json");
    let parsed: serde_json::Value =
        serde_json::from_str(&settings_json).expect("parse settings.json");

    // PreToolUse has exactly three entries: the deduped Bash/guard.sh, the
    // distinct Edit/fmt.sh, and the auto-injected config-guard hook (#289).
    // If bundle dedup regressed this would be four.
    let pre = parsed["hooks"]["PreToolUse"]
        .as_array()
        .expect("PreToolUse array");
    assert_eq!(
        pre.len(),
        3,
        "guard.sh deduped, fmt.sh survives, config-guard added: {pre:#?}"
    );

    // Snapshot pins the full deterministic shape: ordering, dedup, permission
    // union, and native passthrough merge across both bundles.
    insta::assert_yaml_snapshot!(parsed);
}

// Issue #336: Empty directories from bundles that contributed no files must be
// pruned from the rendered output. A directory pre-created via create_dir_all
// for a file path that was never actually written must not remain.
#[test]
fn empty_dirs_are_pruned_after_render() {
    // Build a manifest whose `files` map contains a file under `subdir/`, but
    // we craft the manifest directly so we can simulate the case where the
    // directory was "set up" but nothing lands in a sibling subdirectory.
    // We do this by writing a file into `subdir/kept/`, then manually creating
    // `subdir/empty/` in the output before calling materialize — verifying that
    // the post-render prune removes `subdir/empty/`.
    let tmp = tempdir().expect("tempdir");
    let out = tmp.path();

    // Pre-create an empty directory to simulate a leftover from rendering.
    let empty_dir = out.join("subdir").join("empty");
    std::fs::create_dir_all(&empty_dir).expect("create empty dir");

    // A manifest that writes nothing into `subdir/empty/`.
    let m = llmenv::merge::MergedManifest {
        agents_md: String::new(),
        files: Default::default(),
        ..Default::default()
    };
    ClaudeCodeAdapter.materialize(&m, out).expect("materialize");

    assert!(
        !empty_dir.exists(),
        "empty directory should be pruned after render: {}",
        empty_dir.display()
    );
    assert!(
        !out.join("subdir").exists(),
        "empty parent directory should also be pruned: {}",
        out.join("subdir").display()
    );
    // The output root itself is never removed.
    assert!(out.exists(), "output root must not be removed");
}

// Issue #336: Directories that contain files must NOT be pruned.
#[test]
fn non_empty_dirs_are_preserved_after_render() {
    let tmp = tempdir().expect("tempdir");
    let source_file = tmp.path().join("src_file.md");
    std::fs::write(&source_file, "content").expect("write source file");

    // A manifest that writes one file into `kept/`.
    let mut files = std::collections::BTreeMap::new();
    files.insert(PathBuf::from("kept/file.md"), source_file.clone());
    let m = llmenv::merge::MergedManifest {
        agents_md: String::new(),
        files,
        ..Default::default()
    };

    let out_tmp = tempdir().expect("tempdir");
    ClaudeCodeAdapter
        .materialize(&m, out_tmp.path())
        .expect("materialize");

    let kept_dir = out_tmp.path().join("kept");
    assert!(kept_dir.exists(), "non-empty directory must be preserved");
    assert!(
        kept_dir.join("file.md").exists(),
        "file inside directory must be preserved"
    );
}

// Issue #336: A bundle that contributes 0 files must not leave any empty dirs.
// This tests the scenario where create_dir_all is called for a path whose
// parent directory was prepared but the file never ended up being written
// (e.g. all of bundle's files were filtered out).
#[test]
fn bundle_with_no_files_leaves_no_dirs() {
    // Empty manifest — no files, no rules. Only CLAUDE.md and settings.json
    // are written; no subdirectories should exist.
    let m = llmenv::merge::MergedManifest {
        agents_md: String::new(),
        files: Default::default(),
        ..Default::default()
    };
    let out_tmp = tempdir().expect("tempdir");
    ClaudeCodeAdapter
        .materialize(&m, out_tmp.path())
        .expect("materialize");

    let subdirs: Vec<_> = walkdir::WalkDir::new(out_tmp.path())
        .min_depth(1)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_dir())
        .map(|e| e.path().to_path_buf())
        .collect();

    assert!(
        subdirs.is_empty(),
        "no subdirectories should remain when no bundle contributes files: {subdirs:?}"
    );
}

// Issue #336: Reversing bundle input order must not change the merged hook set
// or permission set — only first-seen list order may shift. Locks the
// order-independence guarantee at the adapter (end-to-end) layer, complementing
// the unit-level property tests in merge::capabilities.
#[test]
fn bundle_order_does_not_change_merged_membership() {
    let forward = vec![fixture_bundle("merge-a"), fixture_bundle("merge-b")];
    let backward = vec![fixture_bundle("merge-b"), fixture_bundle("merge-a")];

    let mf = merge(
        &llmenv::config::Capabilities::default(),
        &empty_native(),
        &forward,
    )
    .expect("merge fwd");
    let mb = merge(
        &llmenv::config::Capabilities::default(),
        &empty_native(),
        &backward,
    )
    .expect("merge bwd");

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

// Issue #162: Bundle-relative hook paths are resolved to absolute paths at
// adapter time, ensuring hooks work when run from a different cwd.
#[test]
fn bundle_relative_hook_paths_are_resolved() {
    let bundles = vec![fixture_bundle("with-relative-hook")];
    let m = merge(
        &llmenv::config::Capabilities::default(),
        &empty_native(),
        &bundles,
    )
    .expect("merge");

    // At merge time, the path is still bundle-relative
    let hook = m
        .capabilities
        .hooks
        .iter()
        .find(|h| h.event == "PostToolUse")
        .expect("PostToolUse hook");
    let cmd = hook.handler.command.as_ref().expect("hook command");
    assert!(
        cmd.contains("hooks/test.sh"),
        "merged hook command should be relative: {}",
        cmd
    );

    // After adapting to settings.json, the path is resolved
    let tmp = tempdir().expect("tempdir");
    ClaudeCodeAdapter
        .materialize(&m, tmp.path())
        .expect("materialize");

    let settings_json =
        std::fs::read_to_string(tmp.path().join("settings.json")).expect("read settings.json");
    let parsed: serde_json::Value =
        serde_json::from_str(&settings_json).expect("parse settings.json");

    // Find the PostToolUse hook in the rendered settings
    let post_tool_use = parsed["hooks"]["PostToolUse"]
        .as_array()
        .expect("PostToolUse array");
    let rendered_cmd = post_tool_use[0]["hooks"][0]["command"]
        .as_str()
        .expect("command string");

    // The rendered command should have the path resolved to absolute
    assert!(
        rendered_cmd.contains("with-relative-hook/hooks/test.sh"),
        "adapter should resolve path to absolute: {}",
        rendered_cmd
    );
}

#[test]
fn emit_hook_context_returns_empty_string_for_empty_input() {
    assert_eq!(ClaudeCodeAdapter.emit_hook_context(""), "");
}

#[test]
fn emit_hook_context_wraps_text_in_json() {
    let text = "test content";
    let output = ClaudeCodeAdapter.emit_hook_context(text);
    let parsed: serde_json::Value = serde_json::from_str(&output).expect("valid JSON");
    assert!(parsed.is_object());
    assert!(parsed.get("hookSpecificOutput").is_some());
    assert!(
        parsed["hookSpecificOutput"]
            .get("additionalContext")
            .is_some()
    );
}

#[test]
fn emit_hook_context_preserves_markdown_content() {
    let text = "## Memory\nContent";
    let output = ClaudeCodeAdapter.emit_hook_context(text);
    let parsed: serde_json::Value = serde_json::from_str(&output).expect("valid JSON");
    let context = parsed["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .expect("context is string");
    assert!(context.contains("## Memory"));
    assert!(context.contains("Content"));
}

#[test]
fn emit_hook_context_escapes_special_characters() {
    let text = r#"{"injection": "attempt", "quote": "\"", "backslash": "\\"}"#;
    let output = ClaudeCodeAdapter.emit_hook_context(text);
    // Should be valid JSON with properly escaped special chars
    let parsed: serde_json::Value = serde_json::from_str(&output).expect("valid JSON");
    let context = parsed["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .expect("context is string");
    // The text should be preserved correctly after round-tripping through JSON
    assert!(context.contains("injection"));
    assert!(context.contains("attempt"));
}

#[test]
fn emit_hook_context_wraps_with_barrier_comment() {
    let text = "context data";
    let output = ClaudeCodeAdapter.emit_hook_context(text);
    let parsed: serde_json::Value = serde_json::from_str(&output).expect("valid JSON");
    let context = parsed["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .expect("context is string");
    // Verify the barrier comment prefix is present
    assert!(context.starts_with("[ICM MEMORY CONTEXT"));
    assert!(context.contains("context data"));
}

#[test]
fn emit_hook_context_handles_newlines() {
    let text = "line1\nline2\nline3";
    let output = ClaudeCodeAdapter.emit_hook_context(text);
    let parsed: serde_json::Value = serde_json::from_str(&output).expect("valid JSON");
    let context = parsed["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .expect("context is string");
    assert!(context.contains("line1"));
    assert!(context.contains("line2"));
    assert!(context.contains("line3"));
}

#[test]
fn emit_hook_context_handles_unicode() {
    let text = "émojis: 🚀 🔒 日本語 中文";
    let output = ClaudeCodeAdapter.emit_hook_context(text);
    let parsed: serde_json::Value = serde_json::from_str(&output).expect("valid JSON");
    let context = parsed["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .expect("context is string");
    assert!(context.contains("émojis"));
    assert!(context.contains("🚀"));
    assert!(context.contains("日本語"));
}
