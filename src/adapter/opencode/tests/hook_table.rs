//! The hook table that materialize writes to `plugin/llmenv-hooks.json` (#2840).

use super::*;
use crate::adapter::hook_program::hook_command;

#[test]
fn materialize_hook_unsupported_event_skipped() {
    let tmp = tempfile::tempdir().unwrap();
    let mut manifest = MergedManifest::default();
    manifest.capabilities.hooks.push(crate::config::Hook {
        event: "ConfigChange".into(),
        matcher: None,
        handler: crate::config::HookHandler {
            kind: crate::config::HookHandlerKind::Command,
            command: Some("echo n".into()),
            tool: None,
            on_failure: None,
        },
        bundle_origin: None,
    });
    OpencodeAdapter.materialize(&manifest, tmp.path()).unwrap();
    let shim_src = std::fs::read_to_string(tmp.path().join("plugin/llmenv-hooks.json")).unwrap();
    assert!(!shim_src.contains("\"event\":\"ConfigChange\""));
}

#[test]
fn materialize_shim_contains_auto_hooks() {
    let tmp = tempfile::tempdir().unwrap();
    OpencodeAdapter
        .materialize(&MergedManifest::default(), tmp.path())
        .unwrap();
    let shim_src = std::fs::read_to_string(tmp.path().join("plugin/llmenv-hooks.json")).unwrap();
    assert!(shim_src.contains("check-stale --engine opencode"));
    assert!(shim_src.contains("config-context --engine opencode"));
    assert!(shim_src.contains("config-guard --engine opencode"));
}

#[test]
fn materialize_hook_with_supported_event_rendered_in_shim() {
    let tmp = tempfile::tempdir().unwrap();
    let mut manifest = MergedManifest::default();
    manifest.capabilities.hooks.push(crate::config::Hook {
        event: "PreToolUse".into(),
        matcher: None,
        handler: crate::config::HookHandler {
            kind: crate::config::HookHandlerKind::Command,
            command: Some("echo hi".into()),
            tool: None,
            on_failure: None,
        },
        bundle_origin: None,
    });
    OpencodeAdapter.materialize(&manifest, tmp.path()).unwrap();
    let shim_src = std::fs::read_to_string(tmp.path().join("plugin/llmenv-hooks.json")).unwrap();
    assert!(shim_src.contains("echo hi"));
    assert!(shim_src.contains("check-stale --engine opencode"));
}

#[test]
fn materialize_no_hooks_still_emits_shim_for_auto_hooks() {
    let tmp = tempfile::tempdir().unwrap();
    OpencodeAdapter
        .materialize(&MergedManifest::default(), tmp.path())
        .unwrap();
    assert!(tmp.path().join("plugin/llmenv.js").exists());
}

#[test]
fn materialize_plugin_hooks_happy_and_warnings() {
    let out = tempfile::tempdir().unwrap();
    let plugin_dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(plugin_dir.path().join("hooks")).unwrap();
    std::fs::write(
        plugin_dir.path().join("hooks/hooks.json"),
        serde_json::json!({
            "hooks": {
                "PreToolUse": [{"hooks": [{"type": "command", "command": "echo ok"}]}],
                "ConfigChange": [{"hooks": [{"type": "command", "command": "echo n"}]}],
            }
        })
        .to_string(),
    )
    .unwrap();
    let (plugins, marketplaces) = plugin_manifest(plugin_dir.path());
    let manifest = MergedManifest {
        plugins,
        marketplaces,
        ..Default::default()
    };
    OpencodeAdapter.materialize(&manifest, out.path()).unwrap();
    let shim_src = std::fs::read_to_string(out.path().join("plugin/llmenv-hooks.json")).unwrap();
    assert!(shim_src.contains("echo ok"));
    assert!(!shim_src.contains("echo n"), "ConfigChange is unsupported");
}

#[test]
fn materialize_manifest_hook_mcp_tool_kind_skipped() {
    let tmp = tempfile::tempdir().unwrap();
    let mut manifest = MergedManifest::default();
    manifest.capabilities.hooks.push(crate::config::Hook {
        event: "PreToolUse".into(),
        matcher: None,
        handler: crate::config::HookHandler {
            kind: crate::config::HookHandlerKind::McpTool,
            command: None,
            tool: Some("some_tool".into()),
            on_failure: None,
        },
        bundle_origin: None,
    });
    OpencodeAdapter.materialize(&manifest, tmp.path()).unwrap();
    let shim_src = std::fs::read_to_string(tmp.path().join("plugin/llmenv-hooks.json")).unwrap();
    assert!(!shim_src.contains("some_tool"));
}

/// The commit gate of the task tracker reaches opencode: `hook-run pre_tool_use` runs for `Bash`
/// only, and turning `enforce_commit` off removes it (#2456).
#[test]
fn the_commit_gate_runs_pre_tool_use_for_bash() {
    let table = |tracker: &str| {
        let tmp = tempfile::tempdir().unwrap();
        let mut manifest = MergedManifest::default();
        manifest.capabilities.features = Some(serde_yaml::from_str(tracker).unwrap());
        OpencodeAdapter.materialize(&manifest, tmp.path()).unwrap();
        let raw = std::fs::read_to_string(tmp.path().join("plugin/llmenv-hooks.json")).unwrap();
        serde_json::from_str::<serde_json::Value>(&raw).unwrap()
    };
    let pre_tool = |doc: &serde_json::Value| {
        doc["hooks"]
            .as_array()
            .unwrap()
            .iter()
            .find(|h| {
                h["command"] == hook_command("hook-run pre_tool_use --engine opencode").unwrap()
            })
            .cloned()
    };
    let on = table("task_tracker: {enabled: true}");
    assert_eq!(pre_tool(&on).unwrap()["matcher"], "Bash");
    let off = table("task_tracker: {enabled: true, enforce_commit: false}");
    assert!(pre_tool(&off).is_none(), "{off}");
}

proptest::proptest! {
    /// Every hook with a command reaches the table with its event, matcher, and command,
    /// after the user hooks, and the table is deterministic. A matcher the plugin cannot check
    /// fails the build instead, so the build fails exactly when such a hook is present (#3003).
    #[test]
    fn every_command_hook_reaches_the_table_in_order(
        hooks in proptest::collection::vec(crate::adapter::skills::arb_hook(), 0..5)
    ) {
        let with_command: Vec<_> = hooks.iter().filter(|h| h.handler.command.is_some()).collect();
        let rejected = with_command
            .iter()
            .any(|h| hook_bridge::check_matcher_subject(h).is_err());
        let built = hook_bridge::hook_table(&hooks, &[], &[]);
        proptest::prop_assert_eq!(built.is_err(), rejected);
        if let Ok(table) = built {
            proptest::prop_assert_eq!(&table, &hook_bridge::hook_table(&hooks, &[], &[]).unwrap());
            let entries = table["hooks"].as_array().unwrap();
            for (hook, entry) in with_command.iter().zip(entries) {
                proptest::prop_assert_eq!(entry["event"].as_str(), Some(hook.event.as_str()));
                proptest::prop_assert_eq!(entry["matcher"].as_str(), hook.matcher.as_deref());
            }
            // The four built-in hooks follow the user hooks.
            proptest::prop_assert_eq!(entries.len(), with_command.len() + 4);
        }
    }
}

// #4052: the /tmp guard is a gate, so a run that cannot start blocks the call.
#[test]
fn materialize_shim_gates_the_shell_and_file_tools_with_the_tmp_guard() {
    let tmp = tempfile::tempdir().unwrap();
    OpencodeAdapter
        .materialize(&MergedManifest::default(), tmp.path())
        .unwrap();
    let table: serde_json::Value = serde_json::from_slice(
        &std::fs::read(tmp.path().join("plugin/llmenv-hooks.json")).unwrap(),
    )
    .unwrap();
    let entry = table["hooks"].as_array().unwrap().iter().find(|h| {
        h["command"]
            .as_str()
            .unwrap()
            .contains("tmp-guard --engine opencode")
    });
    assert!(entry.is_some(), "a hook runs tmp-guard");
    let entry = entry.unwrap();
    assert_eq!(entry["event"], "PreToolUse");
    assert_eq!(entry["failClosed"], true);
    let matcher = entry["matcher"].as_str().unwrap();
    for tool in ["Bash", "Write", "Edit", "MultiEdit", "patch"] {
        assert!(matcher.split('|').any(|t| t == tool), "{tool} in {matcher}");
    }
}
