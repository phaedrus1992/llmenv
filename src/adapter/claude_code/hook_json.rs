//! Builds the handler object of one hook in Claude Code's `settings.json`.

/// Builds the handler object for `hook`, with `resolved_command` as its `command`.
///
/// Null-valued keys are omitted, not written as `null`. A null key differs from an absent key
/// in JSON equality, so a re-render would see a changed hook and add a duplicate (#699, #720).
///
/// `write_on_failure` is true when the engine reads `onFailure`. See [`on_failure_allowed`].
pub(super) fn hook_handler_value(
    hook: &crate::config::Hook,
    resolved_command: Option<String>,
    write_on_failure: bool,
) -> serde_json::Value {
    let mut m = serde_json::Map::new();
    let blocks = hook.handler.on_failure == Some(crate::config::HookOnFailure::Block);
    // Claude Code reads `onFailure` on a command hook only. Validation refuses it on another
    // kind, and this check keeps the render in line with it.
    let is_command = hook.handler.kind == crate::config::HookHandlerKind::Command;
    if write_on_failure && blocks && is_command {
        m.insert(
            "onFailure".into(),
            serde_json::Value::String("block".into()),
        );
    }
    if let Some(cmd) = resolved_command {
        m.insert("command".into(), serde_json::Value::String(cmd));
    }
    if let Some(ref tool) = hook.handler.tool {
        m.insert("tool".into(), serde_json::Value::String(tool.clone()));
    }
    m.insert(
        "type".into(),
        serde_json::Value::String(
            match hook.handler.kind {
                crate::config::HookHandlerKind::Command => "command",
                crate::config::HookHandlerKind::McpTool => "mcp_tool",
            }
            .into(),
        ),
    );
    serde_json::Value::Object(m)
}

/// Whether the render writes `onFailure` for the hooks that set `on_failure`. Claude Code reads
/// the key from 2.1.295 (#3101).
///
/// - A known older version gets no key, and an error names the hooks that then fail open.
/// - An unknown version gets the key, because the user asked the hook to fail closed. A warning
///   says that the version was not read.
///
/// `version` runs only when a hook sets `on_failure`, so a render with no such hook, such as each
/// test render, never runs `claude --version`.
pub(super) fn on_failure_allowed(
    hooks: &[crate::config::Hook],
    version: impl FnOnce() -> Option<String>,
) -> bool {
    let named: Vec<String> = hooks
        .iter()
        .filter(|h| h.handler.on_failure.is_some())
        .map(|h| {
            let what = h.handler.command.as_deref().or(h.handler.tool.as_deref());
            format!("{} ({})", h.event, what.unwrap_or("no command"))
        })
        .collect();
    if named.is_empty() {
        return false;
    }
    let version = version();
    match version
        .as_deref()
        .and_then(super::version::supports_on_failure)
    {
        Some(true) => true,
        Some(false) => {
            tracing::error!(
                "the hooks {} set on_failure: block, but Claude Code {} does not read onFailure \
                 before 2.1.295, so llmenv leaves it out and these hooks fail open. Fix: upgrade \
                 Claude Code, then run `llmenv regenerate`.",
                named.join(", "),
                version.as_deref().unwrap_or_default()
            );
            false
        }
        None => {
            tracing::warn!(
                "llmenv cannot read the Claude Code version ({}), so it writes onFailure for the \
                 hooks {} as asked. Claude Code before 2.1.295 does not read the key. Fix: check \
                 that `claude --version` runs from this shell.",
                version.as_deref().unwrap_or("no version"),
                named.join(", ")
            );
            true
        }
    }
}

#[cfg(test)]
#[expect(
    clippy::unwrap_used,
    reason = "a test fails loudly when its fixture cannot be set up"
)]
mod tests {
    use super::*;
    use crate::config::{Hook, HookHandler, HookHandlerKind, HookOnFailure};

    fn hook(on_failure: Option<HookOnFailure>) -> Hook {
        Hook {
            event: "PreToolUse".into(),
            matcher: Some("Bash".into()),
            handler: HookHandler {
                kind: HookHandlerKind::Command,
                command: Some("guard.sh".into()),
                tool: None,
                on_failure,
            },
            bundle_origin: None,
        }
    }

    #[test]
    fn on_failure_block_is_written_only_when_allowed() {
        let blocking = hook(Some(HookOnFailure::Block));
        let value = hook_handler_value(&blocking, Some("guard.sh".into()), true);
        assert_eq!(value["onFailure"], "block");
        let value = hook_handler_value(&blocking, Some("guard.sh".into()), false);
        assert!(value.get("onFailure").is_none(), "{value}");
        let plain = hook(None);
        let value = hook_handler_value(&plain, Some("guard.sh".into()), true);
        assert!(value.get("onFailure").is_none(), "{value}");
    }

    #[test]
    fn the_version_is_read_only_when_a_hook_asks_for_on_failure() {
        let calls = std::cell::Cell::new(0);
        let read = || {
            calls.set(calls.get() + 1);
            Some("2.1.300".to_string())
        };
        assert!(!on_failure_allowed(&[hook(None)], read));
        assert_eq!(calls.get(), 0);
        assert!(on_failure_allowed(
            &[hook(None), hook(Some(HookOnFailure::Block))],
            read
        ));
        assert_eq!(calls.get(), 1);
    }

    #[test]
    fn an_old_engine_gets_no_on_failure_and_an_error_that_names_the_hook() {
        let hooks = [hook(Some(HookOnFailure::Block))];
        let logs = crate::test_log_capture::capture_logs(|| {
            assert!(!on_failure_allowed(&hooks, || Some("2.1.294".into())));
            assert!(!on_failure_allowed(&hooks, || Some("2.1.295-beta".into())));
        });
        assert!(
            logs.contains("Claude Code 2.1.294 does not read onFailure"),
            "{logs}"
        );
        assert!(logs.contains("PreToolUse (guard.sh)"), "{logs}");
        assert!(logs.contains("Fix: upgrade Claude Code"), "{logs}");
    }

    // The user asked the hook to fail closed, so an unknown version keeps the key (#3101).
    #[test]
    fn an_unknown_engine_version_keeps_on_failure_and_warns() {
        let hooks = [hook(Some(HookOnFailure::Block))];
        let logs = crate::test_log_capture::capture_logs(|| {
            assert!(on_failure_allowed(&hooks, || None));
            assert!(on_failure_allowed(&hooks, || Some("2.x".into())));
        });
        assert!(
            logs.contains("cannot read the Claude Code version (no version)"),
            "{logs}"
        );
        assert!(
            logs.contains("Fix: check that `claude --version` runs"),
            "{logs}"
        );
    }

    #[test]
    fn an_mcp_tool_hook_never_gets_on_failure() {
        let mut tool = hook(Some(HookOnFailure::Block));
        tool.handler.kind = HookHandlerKind::McpTool;
        let value = hook_handler_value(&tool, None, true);
        assert!(value.get("onFailure").is_none(), "{value}");
    }

    /// The render reads the engine version and writes `onFailure` for a new enough engine.
    #[test]
    fn the_settings_render_writes_on_failure_for_a_new_engine() {
        let render = |version: &str| {
            super::super::version::TEST_VERSION
                .with(|v| *v.borrow_mut() = Some(Some(version.to_string())));
            let tmp = tempfile::tempdir().unwrap();
            let mut manifest = crate::merge::MergedManifest::default();
            manifest
                .capabilities
                .hooks
                .push(hook(Some(HookOnFailure::Block)));
            super::super::generate_settings_json(tmp.path(), &manifest).unwrap();
            let bytes = std::fs::read(tmp.path().join("settings.json")).unwrap();
            let settings: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            super::super::version::TEST_VERSION.with(|v| *v.borrow_mut() = None);
            settings["hooks"]["PreToolUse"]
                .as_array()
                .unwrap()
                .iter()
                .flat_map(|entry| entry["hooks"].as_array().unwrap().iter())
                .filter(|h| {
                    h["command"]
                        .as_str()
                        .is_some_and(|c| c.contains("guard.sh"))
                })
                .map(|h| h.get("onFailure").cloned())
                .collect::<Vec<_>>()
        };
        assert_eq!(render("2.1.295"), vec![Some(serde_json::json!("block"))]);
        assert_eq!(render("2.1.294"), vec![None]);
    }

    proptest::proptest! {
        /// For any hook, the handler object holds no null, passes command and tool through, and
        /// has `onFailure` exactly when the render allows it and the hook is a blocking command.
        #[test]
        fn the_handler_object_follows_its_hook(
            is_command in proptest::bool::ANY,
            command in proptest::option::of("[a-z./ ]{1,12}"),
            tool in proptest::option::of("[a-z:]{1,12}"),
            block in proptest::bool::ANY,
            write in proptest::bool::ANY,
        ) {
            let mut h = hook(block.then_some(HookOnFailure::Block));
            h.handler.kind = if is_command { HookHandlerKind::Command } else { HookHandlerKind::McpTool };
            h.handler.command = command.clone();
            h.handler.tool = tool.clone();
            let value = hook_handler_value(&h, command.clone(), write);
            let object = value.as_object().unwrap();
            proptest::prop_assert!(object.values().all(|v| !v.is_null()), "{}", value);
            proptest::prop_assert_eq!(object.get("command").and_then(|v| v.as_str()), command.as_deref());
            proptest::prop_assert_eq!(object.get("tool").and_then(|v| v.as_str()), tool.as_deref());
            proptest::prop_assert_eq!(object.contains_key("onFailure"), write && block && is_command);
        }
    }
}
