use proptest::prelude::*;

use super::render_hooks;
use crate::config::{Hook, HookHandler, HookHandlerKind};
use crate::merge::MergedManifest;

fn user_hook(command: &str) -> Hook {
    Hook {
        event: "PreToolUse".into(),
        matcher: None,
        handler: HookHandler {
            kind: HookHandlerKind::Command,
            command: Some(command.into()),
            tool: None,
            on_failure: None,
        },
        bundle_origin: None,
    }
}

fn manifest_with(commands: &[String]) -> MergedManifest {
    let mut manifest = MergedManifest::default();
    manifest.capabilities.hooks = commands.iter().map(|c| user_hook(c)).collect();
    manifest
}

fn commands_of(entries: &serde_json::Value) -> Vec<String> {
    entries
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|entry| entry["hooks"][0]["command"].as_str())
        .map(str::to_owned)
        .collect()
}

proptest! {
    #[test]
    fn user_hooks_keep_their_order_and_come_before_the_built_in_guards(
        names in prop::collection::btree_set("[a-z]{3,8}", 1..5),
    ) {
        let commands: Vec<String> = names.iter().map(|n| format!("echo {n}")).collect();
        let manifest = manifest_with(&commands);
        let dir = tempfile::tempdir().unwrap();
        let first = render_hooks(dir.path(), &manifest).unwrap();
        let second = render_hooks(dir.path(), &manifest).unwrap();
        prop_assert_eq!(&first, &second);

        let pre = commands_of(&first["PreToolUse"]);
        prop_assert_eq!(&pre[..commands.len()], &commands[..]);
        let tmp_guard = pre.iter().position(|c| c.contains("tmp-guard"));
        let config_guard = pre.iter().position(|c| c.contains("config-guard"));
        prop_assert!(tmp_guard.is_some_and(|i| i >= commands.len()), "{:?}", pre);
        prop_assert!(config_guard < tmp_guard, "{:?}", pre);
    }
}
